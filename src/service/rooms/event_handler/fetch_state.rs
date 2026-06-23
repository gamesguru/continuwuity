use std::{
	collections::{HashMap, hash_map},
	time::Duration,
};

use conduwuit::{
	Err, Event, PduEvent, Result, debug, debug_warn, err, implement, info,
	utils::stream::{BroadbandExt, IterStream},
	warn,
};
use futures::StreamExt;
use ruma::{
	EventId, OwnedEventId, RoomId, ServerName, api::federation::event::get_room_state,
	events::StateEventType,
};

use crate::rooms::short::ShortStateKey;

/// Call /state to find out what the state at this pdu is. We trust the
/// server's response to some extent, but we still do a lot of checks
/// on the events.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(%origin),
)]
pub(super) async fn fetch_state<Pdu>(
	&self,
	origin: &ServerName,
	create_event: &Pdu,
	room_id: &RoomId,
	event_id: &EventId,
	skip_sig_verify: bool,
) -> Result<Option<HashMap<u64, OwnedEventId>>>
where
	Pdu: Event + Send + Sync,
{
	// Build the full fallback server list: origin → trusted → room members.
	let mut servers = self
		.build_federation_server_list(
			room_id,
			origin,
			self.services.server.config.federation_fallback_room_servers,
		)
		.await;

	// In inline synchronous fetches, we cap the number of fallback servers to 5
	// to prevent overwhelming the network while ensuring we hit room members.
	// We run these concurrently.
	servers.truncate(5);

	let mut last_err = err!(Request(NotFound("No server could provide /state")));
	let mut futures = futures::stream::FuturesUnordered::new();

	for server in &servers {
		let server = server.clone();
		let event_id = event_id.to_owned();
		let room_id = room_id.to_owned();
		futures.push(async move {
			let req = self.services.sending.send_federation_request(
				&server,
				get_room_state::v1::Request::new(event_id, room_id),
			);
			match tokio::time::timeout(Duration::from_mins(1), req).await {
				| Ok(Ok(res)) => Ok((server, res)),
				| Ok(Err(e)) => Err((server, e)),
				| Err(_) =>
					Err((server, err!(Request(Unknown("Server took too long to return /state"))))),
			}
		});
	}

	let res = 'found: {
		while let Some(result) = futures.next().await {
			match result {
				| Ok((server, res)) => {
					if server != origin {
						debug!(%server, "fetch_state: used fallback server for /state");
					}
					break 'found res;
				},
				| Err((server, e)) => {
					info!(%server, "fetch_state /state failed: {e}");
					last_err = e;
				},
			}
		}
		warn!(
			n_servers = servers.len(),
			"fetch_state: all servers failed /state for {event_id}"
		);
		return Err(last_err);
	};

	let room_version_id = self.services.state.get_room_version(room_id).await?;

	debug!(
		auth_chain_count = res.auth_chain.len(),
		state_count = res.pdus.len(),
		"Processing state and auth chain events from remote server"
	);

	// Deduplicate known events across auth_chain and state events
	let mut unknown_events = Vec::new();
	let mut known_count: usize = 0;
	for raw_json in res
		.auth_chain
		.into_iter()
		.chain(res.pdus.clone().into_iter())
	{
		if let Ok((eid, val)) =
			conduwuit::matrix::event::gen_event_id_canonical_json(&raw_json, &room_version_id)
		{
			if !self.services.timeline.pdu_exists(&eid).await
				&& self.services.outlier.get_pdu_outlier(&eid).await.is_err()
			{
				unknown_events.push((eid, val));
			} else {
				known_count = known_count.saturating_add(1);
			}
		}
	}
	debug!(
		"fetch_state: {} newly missing events, {} already known",
		unknown_events.len(),
		known_count
	);

	// Concurrently parse and verify signatures (Pure CPU and network keys fetch)
	let mut verified_events: HashMap<OwnedEventId, (PduEvent, ruma::CanonicalJsonObject)> =
		unknown_events
			.into_iter()
			.stream()
			.broad_filter_map({
				let room_version_id = room_version_id.clone();
				move |(eid, mut val): (OwnedEventId, ruma::CanonicalJsonObject)| {
					let room_version_id = room_version_id.clone();
					async move {
						let stashed_unsigned = val.remove("unsigned");

						let verification_result = if skip_sig_verify
							|| self
								.services
								.server
								.config
								.bypassed_signature_events
								.contains(&eid)
						{
							Ok(ruma::signatures::Verified::All)
						} else {
							self.services
								.server_keys
								.verify_event(&val, Some(&room_version_id))
								.await
						};

						match verification_result {
							| Ok(
								ruma::signatures::Verified::All
								| ruma::signatures::Verified::Signatures,
							) => {
								if matches!(
									verification_result,
									Ok(ruma::signatures::Verified::Signatures)
								) {
									if let Err(e) = ruma::canonical_json::redact_in_place(
										&mut val,
										&room_version_id,
										None,
									) {
										conduwuit::warn!("Redaction failed for {eid}: {e:?}");
										self.services.pdu_metadata.mark_event_rejected(
											&eid,
											"redaction failed after hash mismatch",
										);
										val.insert(
											"event_id".to_owned(),
											ruma::CanonicalJsonValue::String(
												eid.as_str().to_owned(),
											),
										);
										self.services.outlier.add_pdu_outlier(
											&eid,
											&val,
											Some(room_id),
										);
										return None;
									}
								}

								// Re-attach unsigned for completeness
								if let Some(ruma::CanonicalJsonValue::Object(unsigned_obj)) =
									stashed_unsigned
								{
									if !unsigned_obj.is_empty() {
										val.insert(
											"unsigned".to_owned(),
											ruma::CanonicalJsonValue::Object(unsigned_obj),
										);
									}
								}

								val.insert(
									"event_id".to_owned(),
									ruma::CanonicalJsonValue::String(eid.as_str().to_owned()),
								);

								if let Ok(pdu) =
									PduEvent::from_id_val(&eid, val.clone(), Some(room_id))
								{
									if crate::rooms::event_handler::check_room_id(room_id, &pdu)
										.is_ok()
									{
										return Some((eid, (pdu, val)));
									}
								}
							},
							| _ => {
								// Event sig failed; persist as rejected outlier so we don't
								// re-fetch
								self.services
									.pdu_metadata
									.mark_event_rejected(&eid, "signature verification failed");
								val.insert(
									"event_id".to_owned(),
									ruma::CanonicalJsonValue::String(eid.as_str().to_owned()),
								);
								self.services
									.outlier
									.add_pdu_outlier(&eid, &val, Some(room_id));
							},
						}
						None
					}
				}
			})
			.collect()
			.await;

	// Tologically sort the verified events based on auth_events
	let mut graph = HashMap::new();
	let mut entries = HashMap::new();
	for (eid, (pdu, _)) in &verified_events {
		graph.insert(eid.clone(), pdu.auth_events().map(ToOwned::to_owned).collect());
		entries.insert(eid.clone(), (0_u64.into(), pdu.origin_server_ts));
	}
	let sorted_eids = conduwuit::utils::timeline_sorter::sort_timeline_events(&entries, &graph);

	// Sequentially auth_check them (roots first so auth checks succeed since
	// handle_outlier_pdu queries the DB).
	for eid in sorted_eids {
		if let Some((_, val)) = verified_events.remove(&eid) {
			if let Err(e) = self
				.handle_outlier_pdu(
					origin,
					Some(create_event),
					&eid,
					room_id,
					val,
					true, // is_outlier
					true, // skip_sig_verify (already done above)
					Some(&room_version_id),
				)
				.await
			{
				debug_warn!("fetch_state: failed to handle outlier {eid}: {e}");
			}
		}
	}

	// Construct the returned state map
	let mut state: HashMap<ShortStateKey, OwnedEventId> = HashMap::with_capacity(res.pdus.len());
	for raw_json in res.pdus {
		if let Ok((eid, _)) =
			conduwuit::matrix::event::gen_event_id_canonical_json(&raw_json, &room_version_id)
		{
			// Read from our outlier store or timeline
			let pdu = match self.services.timeline.get_pdu(&eid).await {
				| Ok(pdu) => Ok(pdu),
				| Err(_) => self.services.outlier.get_pdu_outlier(&eid).await,
			};
			if let Ok(pdu) = pdu {
				let state_key = pdu
					.state_key()
					.ok_or_else(|| err!(Database("Found non-state pdu in state events.")))?;

				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(&pdu.kind().to_string().into(), state_key)
					.await;

				match state.entry(shortstatekey) {
					| hash_map::Entry::Vacant(v) => {
						v.insert(eid.clone());
					},
					| hash_map::Entry::Occupied(_) => {
						return Err!(Database(
							"State event's type and state_key combination exists multiple \
							 times: {}, {}",
							pdu.kind(),
							state_key
						));
					},
				}
			}
		}
	}

	// The original create event must still be in the state
	let create_shortstatekey = self
		.services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await?;

	if state.get(&create_shortstatekey).map(AsRef::as_ref) != Some(create_event.event_id()) {
		return Err!(Database("Incoming event refers to wrong create event."));
	}

	Ok(Some(state))
}
