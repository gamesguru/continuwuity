use std::collections::{BTreeMap, HashMap, hash_map};

use conduwuit::{
	Err, Event, PduEvent, Result, debug, debug_info, err, implement, info, state_res, trace, warn,
};
use futures::future::ready;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, ServerName,
	events::{StateEventType, TimelineEventType},
};

use super::{check_room_id, get_room_version_id, to_room_version};
use crate::rooms::timeline::pdu_fits;

#[implement(super::Service)]
#[allow(clippy::too_many_arguments)]
pub async fn handle_outlier_pdu<'a, Pdu>(
	&self,
	origin: &'a ServerName,
	create_event: Option<&'a Pdu>,
	event_id: &'a EventId,
	room_id: &'a RoomId,
	mut value: CanonicalJsonObject,
	_auth_events_known: bool,
	skip_sig_verify: bool,
	room_version_override: Option<&'a ruma::RoomVersionId>,
) -> Result<(PduEvent, BTreeMap<String, CanonicalJsonValue>)>
where
	Pdu: Event + Send + Sync,
{
	// Skip the PDU if we already have it
	if let (Ok(pdu), Ok(json)) = (
		self.services.timeline.get_pdu(event_id).await,
		self.services.timeline.get_pdu_json(event_id).await,
	) {
		if pdu.room_id_or_hash().as_deref() == Some(room_id) {
			info!(
				target: "state_res_debug",
				%event_id,
				event_type = ?pdu.kind,
				"handle_outlier_pdu: early return, event already known"
			);
			return Ok((pdu, json));
		}
	}

	if !pdu_fits(&mut value.clone()) {
		warn!(
			"dropping incoming PDU {event_id} in room {room_id} from {origin} because it \
			 exceeds 65535 bytes or is otherwise too large."
		);
		return Err!(Request(TooLarge("PDU is too large")));
	}
	// Strip unsigned before signature verification (unsigned is not signed,
	// so it must be excluded). Stash it so we can re-attach origin's
	// prev_content after verification succeeds.
	let stashed_unsigned = value.remove("unsigned");

	// TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

	let room_version_id = match create_event {
		| Some(ce) => get_room_version_id(ce)?,
		| None =>
			if let Some(override_v) = room_version_override {
				override_v.clone()
			} else {
				self.services
					.state
					.get_room_version(room_id)
					.await
					.map_err(|e| {
						err!(Request(InvalidParam(
							"Room version is unknown locally and no override was provided: {e}"
						)))
					})?
			},
	};

	let mut incoming_pdu = if skip_sig_verify {
		// Caller already verified signatures (e.g. import_pdus via
		// validate_and_add_event_id). Skip redundant verification.
		value
	} else if self
		.services
		.server
		.config
		.bypassed_signature_events
		.contains(&event_id.to_owned())
	{
		// Configured exception — skip signature verification for known-bad events
		conduwuit::info!(
			"Bypassing signature verification for configured exception event: {event_id}"
		);
		value
	} else {
		// Check signatures, otherwise drop
		// check content hash, redact if doesn't match
		match self
			.services
			.server_keys
			.verify_event(&value, Some(&room_version_id))
			.await
		{
			| Ok(ruma::signatures::Verified::All) => value,
			| Ok(ruma::signatures::Verified::Signatures) => {
				// Content hash mismatch: content may have been tampered by a relay.
				// If we already have this event locally, re-use our known-good content
				// instead of redacting or re-fetching from the origin.
				if let Ok(known_pdu) = self.services.timeline.get_pdu(event_id).await {
					info!(
						%event_id,
						"Received redacted copy, but we already have known-good content. Re-using."
					);
					check_room_id(room_id, &known_pdu)?;
					let obj = known_pdu.to_canonical_object();
					return Ok((known_pdu, obj));
				}

				// Attempt to fetch a pristine copy from the sender's server.
				let sender_server = value
					.get("sender")
					.and_then(|v| v.as_str())
					.and_then(|s| ruma::UserId::parse(s).ok())
					.map(|u| u.server_name().to_owned());

				let mut recovered = false;
				if let Some(ref server) = sender_server {
					if server.as_str() != origin.as_str() {
						debug_info!(
							%event_id,
							"Hash mismatch, fetching pristine copy from {server}"
						);
						if let Ok(res) = self
							.services
							.sending
							.send_federation_request(
								server,
								ruma::api::federation::event::get_event::v1::Request {
									event_id: event_id.to_owned(),
									include_unredacted_content: None,
								},
							)
							.await
						{
							if let Ok((eid, clean_val)) =
								conduwuit::matrix::event::gen_event_id_canonical_json(
									&res.pdu,
									&room_version_id,
								) {
								if eid == *event_id {
									if matches!(
										self.services
											.server_keys
											.verify_event(&clean_val, Some(&room_version_id))
											.await,
										Ok(ruma::signatures::Verified::All)
									) {
										debug_info!(
											%event_id,
											"Recovered pristine copy from {server}"
										);
										recovered = true;
									}
								}
							}
						}
					}
				}

				if recovered {
					// Re-fetch since we can't move clean_val out of the nested scope
					if let Ok(res) = self
						.services
						.sending
						.send_federation_request(
							sender_server.as_ref().unwrap(),
							ruma::api::federation::event::get_event::v1::Request {
								event_id: event_id.to_owned(),
								include_unredacted_content: None,
							},
						)
						.await
					{
						if let Ok((_, clean_val)) =
							conduwuit::matrix::event::gen_event_id_canonical_json(
								&res.pdu,
								&room_version_id,
							) {
							clean_val
						} else {
							debug_info!("Calculated hash does not match (redaction): {event_id}");
							ruma::canonical_json::redact(value, &room_version_id, None)
								.map_err(|_| err!(Request(InvalidParam("Redaction failed"))))?
						}
					} else {
						debug_info!("Calculated hash does not match (redaction): {event_id}");
						ruma::canonical_json::redact(value, &room_version_id, None)
							.map_err(|_| err!(Request(InvalidParam("Redaction failed"))))?
					}
				} else {
					debug_info!("Calculated hash does not match (redaction): {event_id}");
					ruma::canonical_json::redact(value, &room_version_id, None)
						.map_err(|_| err!(Request(InvalidParam("Redaction failed"))))?
				}
			},
			| Err(e) => {
				// Persist as rejected outlier so we don't re-fetch from
				// federation on every auth chain walk
				value.insert(
					"event_id".to_owned(),
					CanonicalJsonValue::String(event_id.as_str().to_owned()),
				);
				self.services.pdu_metadata.mark_event_rejected(event_id);
				self.services
					.outlier
					.add_pdu_outlier(event_id, &value, Some(room_id));
				return Err!(Request(InvalidParam(debug_error!(
					"Signature verification failed for {event_id}: {e}"
				))));
			},
		}
	};

	// Now that we have checked the signature and hashes we can add the eventID and
	// convert to our PduEvent type
	incoming_pdu.insert(
		"event_id".to_owned(),
		CanonicalJsonValue::String(event_id.as_str().to_owned()),
	);

	// Re-attach the origin's unsigned field (age, etc.) after stripping
	// untrusted state metadata. append_pdu will recompute prev_content
	// from local state when a snapshot is available.
	if let Some(CanonicalJsonValue::Object(mut unsigned_obj)) = stashed_unsigned {
		unsigned_obj.remove("prev_content");
		unsigned_obj.remove("prev_sender");
		unsigned_obj.remove("replaces_state");
		if !unsigned_obj.is_empty() {
			incoming_pdu.insert("unsigned".to_owned(), CanonicalJsonValue::Object(unsigned_obj));
		}
	}

	let pdu_event = match serde_json::from_value::<PduEvent>(
		serde_json::to_value(&incoming_pdu).expect("CanonicalJsonObj is a valid JsonValue"),
	) {
		| Ok(pdu) => pdu,
		| Err(e) => {
			// Persist as a rejected outlier to preserve the DAG chain.
			// This prevents future valid events that reference this event from
			// failing with MissingAuthEvents.
			self.services.pdu_metadata.mark_event_rejected(event_id);
			self.services
				.outlier
				.add_pdu_outlier(event_id, &incoming_pdu, Some(room_id));
			return Err!(Request(BadJson(debug_warn!("Event is not a valid PDU: {e}"))));
		},
	};

	check_room_id(room_id, &pdu_event)?;

	// Fetch all auth events
	let mut auth_events: HashMap<OwnedEventId, PduEvent> = HashMap::new();

	for aid in pdu_event.auth_events() {
		// If any of the auth events are already marked as rejected, this event is
		// automatically rejected. We must check this BEFORE attempting to fetch the
		// auth event to avoid deadlocks (e.g. MissingAuthEvents) when an auth event
		// is unparsable but correctly marked as rejected in our database.
		if self.services.pdu_metadata.is_event_rejected(aid).await {
			self.services.pdu_metadata.mark_event_rejected(event_id);
			self.services.outlier.add_pdu_outlier(
				pdu_event.event_id(),
				&incoming_pdu,
				Some(room_id),
			);
			self.services
				.pdu_metadata
				.mark_event_rejected(pdu_event.event_id());
			return Err!(Request(Forbidden("Event depends on rejected auth event {aid}")));
		}

		if let Ok(auth_event) = self
			.services
			.timeline
			.get_pdu_in_room(Some(room_id), aid)
			.await
		{
			check_room_id(room_id, &auth_event)?;
			info!(
				target: "state_res_debug",
				%event_id,
				auth_event_id = %aid,
				event_type = ?auth_event.kind,
				"Found auth event locally for outlier"
			);
			auth_events.insert(aid.to_owned(), auth_event);
		} else if let Ok(auth_event) = self.services.outlier.get_pdu_outlier(aid).await {
			check_room_id(room_id, &auth_event)?;
			info!(
				target: "state_res_debug",
				%event_id,
				auth_event_id = %aid,
				event_type = ?auth_event.kind,
				"Found auth event in outlier store"
			);
			auth_events.insert(aid.to_owned(), auth_event);
		} else {
			info!(
				target: "state_res_debug",
				%event_id,
				auth_event_id = %aid,
				"Auth event NOT found locally for outlier"
			);
		}
	}

	// Check for auth events still missing after local + outlier lookup
	let missing_auth_events = pdu_event
		.auth_events()
		.filter(|id| !auth_events.contains_key(*id))
		.collect::<Vec<_>>();
	info!(
		target: "state_res_debug",
		%event_id,
		found = auth_events.len(),
		missing = missing_auth_events.len(),
		total_auth = pdu_event.auth_events().count(),
		"Auth events local lookup summary"
	);
	if !missing_auth_events.is_empty() {
		// For a small number of missing auth events, try /event_auth inline.
		// This satisfies complement tests that register /event_auth handlers
		// (e.g. TestInboundFederationRejectsEventsWithRejectedAuthEvents).
		// For large missing counts (e.g. MSC4297 with 250+ events), skip
		// /event_auth to avoid excessive HTTP overhead and let the caller
		// retry via /state_ids instead.
		const MAX_INLINE_FETCH: usize = 5;
		if missing_auth_events.len() <= MAX_INLINE_FETCH {
			info!(
				target: "state_res_debug",
				%event_id,
				count = missing_auth_events.len(),
				"Fetching missing auth events via /event_auth"
			);
			if let Ok(response) = self
				.services
				.sending
				.send_federation_request(
					origin,
					ruma::api::federation::authorization::get_event_authorization::v1::Request {
						room_id: room_id.to_owned(),
						event_id: event_id.to_owned(),
					},
				)
				.await
			{
				let mut auth_chain_map = HashMap::new();
				for auth_pdu in &response.auth_chain {
					if let Ok((auth_eid, auth_val)) =
						conduwuit::matrix::event::gen_event_id_canonical_json(
							auth_pdu,
							&room_version_id,
						) {
						if let Ok(parsed) = serde_json::from_value::<PduEvent>(
							serde_json::to_value(&auth_val).unwrap_or_default(),
						) {
							if check_room_id(room_id, &parsed).is_ok() {
								auth_chain_map.insert(auth_eid, (auth_val, parsed));
							}
						}
					}
				}

				let mut in_degree = HashMap::new();
				for (eid, (_, pdu)) in &auth_chain_map {
					let mut count = 0_usize;
					for auth_id in pdu.auth_events() {
						if auth_chain_map.contains_key(auth_id) {
							count = count.saturating_add(1);
						}
					}
					in_degree.insert(eid.clone(), count);
				}

				let mut sorted_auth_chain = Vec::new();
				let mut queue: Vec<_> = in_degree
					.iter()
					.filter_map(|(k, &v)| if v == 0 { Some(k.clone()) } else { None })
					.collect();

				while let Some(eid) = queue.pop() {
					sorted_auth_chain.push(eid.clone());
					for (other_eid, (_, other_pdu)) in &auth_chain_map {
						if other_pdu.auth_events().any(|aid| aid == eid) {
							if let Some(deg) = in_degree.get_mut(other_eid) {
								*deg = deg.saturating_sub(1);
								if *deg == 0 {
									queue.push(other_eid.clone());
								}
							}
						}
					}
				}

				for auth_eid in sorted_auth_chain {
					if let Some((auth_val, _)) = auth_chain_map.remove(&auth_eid) {
						if !auth_events.contains_key(&auth_eid) {
							if let Ok((pdu, _)) = Box::pin(self.handle_outlier_pdu(
								origin,
								create_event,
								&auth_eid,
								room_id,
								auth_val,
								true,
								false,
								room_version_override,
							))
							.await
							{
								auth_events.insert(pdu.event_id().to_owned(), pdu);
							}
						}
					}
				}
			}

			// Re-check: are we still missing auth events after /event_auth?
			let mut still_missing = Vec::new();
			for id in pdu_event.auth_events() {
				if !auth_events.contains_key(id) {
					if self.services.pdu_metadata.is_event_rejected(id).await {
						self.services.pdu_metadata.mark_event_rejected(event_id);
						self.services.outlier.add_pdu_outlier(
							pdu_event.event_id(),
							&incoming_pdu,
							Some(room_id),
						);
						self.services
							.pdu_metadata
							.mark_event_rejected(pdu_event.event_id());
						return Err!(Request(Forbidden(
							"Event depends on rejected auth event {id}"
						)));
					}
					still_missing.push(id.to_owned());
				}
			}

			if !still_missing.is_empty() {
				debug_info!(
					"Still missing {} auth events for {event_id} after /event_auth",
					still_missing.len()
				);
				return Err!(MissingAuthEvents(still_missing));
			}
		} else {
			info!(
				"Missing {} auth events for {event_id}; will be resolved via /state_ids retry",
				missing_auth_events.len()
			);
			let missing: Vec<_> = missing_auth_events
				.into_iter()
				.map(ToOwned::to_owned)
				.collect();
			return Err!(MissingAuthEvents(missing));
		}
	}
	debug!("No missing auth events for outlier event {event_id}");

	// Build map of auth events and reject if we are still missing some
	let mut auth_events_by_key: HashMap<_, _> = HashMap::with_capacity(auth_events.len());
	for id in pdu_event.auth_events() {
		// Re-check for rejected auth events. We might have fetched them via /event_auth
		// and discovered they were rejected. If they are, this event must be rejected.
		if self.services.pdu_metadata.is_event_rejected(id).await {
			self.services.pdu_metadata.mark_event_rejected(event_id);
			self.services.outlier.add_pdu_outlier(
				pdu_event.event_id(),
				&incoming_pdu,
				Some(room_id),
			);
			self.services
				.pdu_metadata
				.mark_event_rejected(pdu_event.event_id());
			return Err!(Request(Forbidden("Event depends on rejected auth event {id}")));
		}

		let Some(auth_event) = auth_events.get(id).map(ToOwned::to_owned) else {
			return Err!(Request(InvalidParam(debug_error!(
				"Could not fetch all auth events for outlier event {event_id}, still missing: \
				 {id}"
			))));
		};

		check_room_id(room_id, &auth_event)?;

		match auth_events_by_key.entry((
			auth_event.kind.to_string().into(),
			auth_event
				.state_key
				.clone()
				.expect("all auth events have state keys"),
		)) {
			| hash_map::Entry::Vacant(v) => {
				v.insert(auth_event);
			},
			| hash_map::Entry::Occupied(_) => {
				self.services.pdu_metadata.mark_event_rejected(event_id);
				self.services.outlier.add_pdu_outlier(
					pdu_event.event_id(),
					&incoming_pdu,
					Some(room_id),
				);
				return Err!(Request(InvalidParam(
					"Auth event's type and state_key combination exists multiple times: {}, {}",
					auth_event.kind,
					auth_event.state_key().unwrap_or("")
				)));
			},
		}
	}

	// The original create event must be in the auth events for v11 and below.
	// The create event itself has an empty auth_events array (it's the DAG root).
	// For v12+, create is not required in auth_events.
	if pdu_event.kind != TimelineEventType::RoomCreate
		&& !to_room_version(&room_version_id).room_ids_as_hashes
		&& !auth_events_by_key.contains_key(&(StateEventType::RoomCreate, String::new().into()))
	{
		self.services.pdu_metadata.mark_event_rejected(event_id);
		self.services
			.outlier
			.add_pdu_outlier(pdu_event.event_id(), &incoming_pdu, Some(room_id));
		return Err!(Request(InvalidParam(
			"Incoming event missing m.room.create in auth events"
		)));
	}

	let state_fetch = |ty: &StateEventType, sk: &str| {
		let key = (ty.to_owned(), sk.into());
		ready(auth_events_by_key.get(&key).map(ToOwned::to_owned))
	};

	let fetched_create;
	let create_event_ref = if let Some(ce) = create_event {
		ce.as_pdu()
	} else if let Some(ce) =
		auth_events_by_key.get(&(StateEventType::RoomCreate, String::new().into()))
	{
		ce
	} else if let Ok(ce) = self
		.services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.await
	{
		fetched_create = ce;
		&fetched_create
	} else {
		&pdu_event
	};

	let auth_check = state_res::event_auth::auth_check(
		&to_room_version(&room_version_id),
		&pdu_event,
		None, // TODO: third party invite
		state_fetch,
		create_event_ref,
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	if !auth_check {
		self.services.pdu_metadata.mark_event_rejected(event_id);
		self.services
			.outlier
			.add_pdu_outlier(pdu_event.event_id(), &incoming_pdu, Some(room_id));
		return Err!(Request(Forbidden(
			"Event authorisation fails based on event's claimed auth events"
		)));
	}

	trace!("Validation successful.");

	// 7. Persist the event as an outlier.
	self.services
		.outlier
		.add_pdu_outlier(pdu_event.event_id(), &incoming_pdu, Some(room_id));

	trace!("Added pdu as outlier.");

	Ok((pdu_event, incoming_pdu))
}
