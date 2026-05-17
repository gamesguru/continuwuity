use conduwuit::{
	Err, Result, debug_warn, implement, matrix::event::gen_event_id_canonical_json, trace,
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedServerName, RoomVersionId,
	ServerName, UserId, signatures::Verified,
};
use serde_json::value::RawValue as RawJsonValue;

/// Extract the origin server(s) from an event and strip all non-origin
/// signatures. Per the Matrix spec, only the origin server's signature is
/// required for verification. Extra signatures added by relay/policy servers
/// (e.g. `asgard.chat`) can cause spurious verification failures when we don't
/// have their signing keys.
///
/// For room versions V1/V2, the "origin" includes both the sender's server and
/// the event_id's server (since event_ids were server-assigned). For V3+,
/// event_ids are content-hash-derived, so only the sender's server matters.
fn isolate_origin_signatures(
	event: &CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> CanonicalJsonObject {
	// Extract the sender's server name
	let sender_server: Option<OwnedServerName> = event
		.get("sender")
		.and_then(|v| match v {
			| CanonicalJsonValue::String(s) => UserId::parse(s.as_str()).ok(),
			| _ => None,
		})
		.map(|user_id| user_id.server_name().to_owned());

	// For V1/V2, event_id is server-assigned (e.g. "$abc:example.com"),
	// so that server's signature is also authoritative.
	let event_id_server: Option<OwnedServerName> = match room_version {
		| RoomVersionId::V1 | RoomVersionId::V2 => event
			.get("event_id")
			.and_then(|v| match v {
				| CanonicalJsonValue::String(s) => {
					// V1/V2 event_ids look like "$opaque:server.name"
					s.split_once(':')
						.and_then(|(_, srv)| ServerName::parse(srv).ok())
				},
				| _ => None,
			})
			.map(ToOwned::to_owned),
		| _ => None,
	};

	let Some(ref sender) = sender_server else {
		// Can't determine origin — return as-is, let ruma handle the failure
		return event.clone();
	};

	// Build the set of origin servers to retain
	let mut origin_servers: Vec<&ServerName> = vec![sender.as_ref()];
	if let Some(ref eid_server) = event_id_server {
		if eid_server != sender {
			origin_servers.push(eid_server.as_ref());
		}
	}

	let mut filtered = event.clone();
	if let Some(CanonicalJsonValue::Object(sigs)) = filtered.get_mut("signatures") {
		let orig_count = sigs.len();
		sigs.retain(|server_name, _| {
			origin_servers
				.iter()
				.any(|origin| server_name == origin.as_str())
		});
		let removed = orig_count.saturating_sub(sigs.len());
		if removed > 0 {
			trace!(
				sender = sender.as_str(),
				removed, "Stripped non-origin signatures before verification"
			);
		}
	}

	filtered
}

#[implement(super::Service)]
pub async fn validate_and_add_event_id(
	&self,
	pdu: &RawJsonValue,
	room_version: &RoomVersionId,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	let (event_id, mut value) = gen_event_id_canonical_json(pdu, room_version)?;
	if let Err(e) = self.verify_event(&value, Some(room_version)).await {
		return Err!(BadServerResponse(debug_error!(
			"Event {event_id} failed verification: {e:?}"
		)));
	}

	value.insert("event_id".into(), CanonicalJsonValue::String(event_id.as_str().into()));

	Ok((event_id, value))
}

#[implement(super::Service)]
pub async fn validate_and_add_event_id_no_fetch(
	&self,
	pdu: &RawJsonValue,
	room_version: &RoomVersionId,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	trace!(?pdu, "Validating PDU without fetching keys");
	let (event_id, mut value) = gen_event_id_canonical_json(pdu, room_version)?;
	trace!(event_id = event_id.as_str(), "Generated event ID, checking required keys");
	if !self.required_keys_exist(&value, room_version).await {
		debug_warn!(
			"Event {event_id} is missing required keys, cannot verify without fetching keys"
		);
		return Err!(BadServerResponse(debug_warn!(
			"Event {event_id} cannot be verified: missing keys."
		)));
	}
	trace!("All required keys exist, verifying event");
	if let Err(e) = self.verify_event(&value, Some(room_version)).await {
		debug_warn!("Event verification failed");
		return Err!(BadServerResponse(debug_error!(
			"Event {event_id} failed verification: {e:?}"
		)));
	}
	trace!("Event verified successfully");

	value.insert("event_id".into(), CanonicalJsonValue::String(event_id.as_str().into()));

	Ok((event_id, value))
}

#[implement(super::Service)]
pub async fn verify_event(
	&self,
	event: &CanonicalJsonObject,
	room_version: Option<&RoomVersionId>,
) -> Result<Verified> {
	let room_version = room_version.unwrap_or(&RoomVersionId::V12);

	// Per spec, only the origin server's signature is required.
	// Relay/policy servers (e.g. asgard.chat) may add extra signatures
	// that we don't have keys for, causing spurious verification failures.
	// Strip non-origin signatures before verification.
	let event = isolate_origin_signatures(event, room_version);

	let keys = self.get_event_keys(&event, room_version).await?;
	ruma::signatures::verify_event(&keys, &event, room_version).map_err(Into::into)
}

#[implement(super::Service)]
pub async fn verify_json(
	&self,
	event: &CanonicalJsonObject,
	room_version: Option<&RoomVersionId>,
) -> Result {
	let room_version = room_version.unwrap_or(&RoomVersionId::V12);
	let keys = self.get_event_keys(event, room_version).await?;
	ruma::signatures::verify_json(&keys, event.clone()).map_err(Into::into)
}
