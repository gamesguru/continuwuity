use std::{
	collections::{BTreeMap, HashMap, HashSet, hash_map},
	time::Instant,
};

use conduwuit::{
	Event, PduEvent, debug, implement, info, matrix::event::gen_event_id_canonical_json,
	state_res, trace, utils::continue_exponential_backoff_secs, warn,
};
use conduwuit_core::debug_info;
use futures::{
	FutureExt, future,
	stream::{FuturesUnordered, StreamExt},
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, ServerName,
	api::federation::event::get_event,
};

#[implement(super::Service)]
pub async fn fetch_and_handle_outliers<'a, Pdu, Events>(
	&self,
	origin: &'a ServerName,
	events: Events,
	create_event: Option<&'a Pdu>,
	room_id: &'a RoomId,
	skip_sig_verify: bool,
	room_version_override: Option<&'a ruma::RoomVersionId>,
) -> Vec<(PduEvent, Option<BTreeMap<String, CanonicalJsonValue>>)>
where
	Pdu: Event + Send + Sync,
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let back_off = |id| match self
		.services
		.globals
		.bad_event_ratelimiter
		.write()
		.entry(id)
	{
		| hash_map::Entry::Vacant(e) => {
			e.insert((Instant::now(), 1));
		},
		| hash_map::Entry::Occupied(mut e) => {
			*e.get_mut() = (Instant::now(), e.get().1.saturating_add(1));
		},
	};

	let mut routing_servers = self
		.build_federation_server_list(
			room_id,
			origin,
			self.services.server.config.federation_fallback_room_servers,
		)
		.await;

	if routing_servers.len() > 2 {
		conduwuit::utils::shuffle(&mut routing_servers[1..]);
	}
	routing_servers.truncate(4);

	debug!(
		origin = %origin,
		n_total = routing_servers.len(),
		"Built federation fallback server list for outlier fetching"
	);

	let mut fetched_info: HashMap<OwnedEventId, CanonicalJsonObject> = HashMap::new();
	let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::with_capacity(128);
	let mut active_fetches = FuturesUnordered::new();
	let fetch_concurrency = std::sync::Arc::new(tokio::sync::Semaphore::new(
		self.services.server.concurrency_scaled(2),
	));
	let limit = self.services.server.config.max_fetch_prev_events;

	let mut requested_seeds = Vec::new();
	let mut pre_resolved_pdus = Vec::new();

	let push_fetch =
		|event_id: OwnedEventId, is_retry: bool, fetches: &mut FuturesUnordered<_>| {
			let servers = routing_servers.clone();
			let sem = fetch_concurrency.clone();
			fetches.push(
				async move {
					let _permit = sem.acquire().await;
					for attempt in 0..2_u8 {
						if is_retry && attempt > 0 {
							tokio::time::sleep(std::time::Duration::from_secs(
								2_u64.pow(attempt.into()),
							))
							.await;
							debug!(%event_id, attempt, "Retrying fetch");
						}
						let reqs = servers.iter().enumerate().map(|(i, server)| {
							let event_id = event_id.clone();
							async move {
								let start = Instant::now();
								match self
									.services
									.sending
									.send_federation_request(server, get_event::v1::Request {
										event_id: event_id.clone(),
										include_unredacted_content: None,
									})
									.await
								{
									| Ok(res) => {
										self.update_peer_stats(server, true, start.elapsed());
										Ok((res, server.clone()))
									},
									| Err(e) => {
										self.update_peer_stats(server, false, start.elapsed());
										if i == 0 {
											debug!(%event_id, %server, "Origin server failed: {e}");
										}
										Err(e)
									},
								}
							}
							.boxed()
						});

						match future::select_ok(reqs).await {
							| Ok((res, _rem)) => return (event_id, Ok(res)),
							| Err(_all_errors) =>
								if is_retry {
									info!(%event_id, n_servers = servers.len(), attempt, "All servers exhausted");
								} else {
									debug!(%event_id, n_servers = servers.len(), "All servers exhausted");
									break;
								},
						}
					}
					(
						event_id,
						Err(conduwuit::err!(Request(NotFound(
							"event not found after trying all servers"
						)))),
					)
				}
				.boxed(),
			);
		};

	for id in events {
		requested_seeds.push(id.to_owned());

		if let Ok(local_pdu) = self.services.timeline.get_pdu(id).await {
			if self.services.pdu_metadata.is_event_soft_failed(id).await {
				info!(target: "auth_chain", "Found known soft-failed outlier locally: {id}");
			} else {
				trace!("Found {id} in main timeline or outlier tree");
			}
			pre_resolved_pdus.push((id.to_owned(), local_pdu));
			continue;
		}

		if self.services.pdu_metadata.is_event_soft_failed(id).await {
			warn!(target: "auth_chain", "Skipping unparsable soft-failed outlier: {id}");
			continue;
		}

		if let Some((time, tries)) = self.services.globals.bad_event_ratelimiter.read().get(id) {
			const MIN_DURATION: u64 = 60 * 2;
			const MAX_DURATION: u64 = 60 * 60 * 8;
			if continue_exponential_backoff_secs(
				MIN_DURATION,
				MAX_DURATION,
				time.elapsed(),
				*tries,
			) {
				info!(
					target: "auth_chain",
					tried = ?*tries,
					elapsed = ?time.elapsed(),
					"Backing off from {id} (ratelimited)"
				);
				continue;
			}
		}

		push_fetch(id.to_owned(), false, &mut active_fetches);
		graph.insert(id.to_owned(), HashSet::new());
	}

	let mut processed_pdus: HashMap<
		OwnedEventId,
		(PduEvent, Option<BTreeMap<String, CanonicalJsonValue>>),
	> = HashMap::new();

	loop {
		while let Some((next_id, fetch_res)) = active_fetches.next().await {
			match fetch_res {
				| Ok((res, successful_server)) => {
					debug!("Got {next_id} over federation from {successful_server}");

					let room_version_id = match create_event {
						| Some(ce) =>
							match crate::rooms::event_handler::get_room_version_id(ce) {
								| Ok(v) => v,
								| Err(_) => {
									warn!(
										"Provided create_event for {room_id} has no room \
										 version! Skipping outlier {next_id}"
									);
									back_off(next_id.clone());
									continue;
								},
							},
						| None => {
							let mut version = None;
							if let Ok(json) =
								serde_json::from_str::<serde_json::Value>(res.pdu.get())
							{
								if json.get("type").and_then(|t| t.as_str())
									== Some("m.room.create")
								{
									let v = json
										.get("content")
										.and_then(|c| c.get("room_version"))
										.and_then(|v| v.as_str())
										.unwrap_or("1");
									version = ruma::RoomVersionId::try_from(v).ok();
								}
							}
							match version {
								| Some(v) => v,
								| None =>
									if let Some(override_v) = room_version_override {
										override_v.clone()
									} else {
										match self.services.state.get_room_version(room_id).await
										{
											| Ok(v) => v,
											| Err(e) => {
												warn!(
													"Unknown room version for {room_id}, \
													 skipping outlier {next_id}: {e}"
												);
												back_off(next_id.clone());
												continue;
											},
										}
									},
							}
						},
					};
					let Ok((calculated_event_id, value)) =
						gen_event_id_canonical_json(&res.pdu, &room_version_id)
					else {
						back_off(next_id);
						continue;
					};

					if calculated_event_id != next_id {
						warn!(
							"Server didn't return event id we requested: requested: {next_id}, \
							 we got {calculated_event_id}. Event: {:?}",
							&res.pdu
						);
					}

					let mut next_auth_events = HashSet::new();
					if let Some(auth_events) = value
						.get("auth_events")
						.and_then(CanonicalJsonValue::as_array)
					{
						for auth_event in auth_events {
							if let Ok(auth_event) =
								serde_json::from_value::<OwnedEventId>(auth_event.clone().into())
							{
								if self
									.services
									.pdu_metadata
									.is_event_soft_failed(&auth_event)
									.await
								{
									info!(target: "auth_chain", "Found known soft-failed auth event locally: {auth_event}");
								}

								if !graph.contains_key(&auth_event) {
									if !self.services.timeline.pdu_exists(&auth_event).await {
										let ratelimited = if let Some((time, tries)) = self
											.services
											.globals
											.bad_event_ratelimiter
											.read()
											.get(&*auth_event)
										{
											const MIN_DURATION: u64 = 60 * 2;
											const MAX_DURATION: u64 = 60 * 60 * 8;
											continue_exponential_backoff_secs(
												MIN_DURATION,
												MAX_DURATION,
												time.elapsed(),
												*tries,
											)
										} else {
											false
										};

										if ratelimited {
											info!(target: "auth_chain", "Backing off from {auth_event} (auth event ratelimited)");
											continue;
										}

										if graph.len() >= limit.into() {
											info!(target: "auth_chain", "Max auth event limit reached! Limit: {limit}");
											continue;
										}

										trace!(
											"Found auth event id {auth_event} for event \
											 {next_id}"
										);
										push_fetch(auth_event.clone(), true, &mut active_fetches);
									}
									graph.insert(auth_event.clone(), HashSet::new());
								}

								if graph.contains_key(&auth_event) {
									next_auth_events.insert(auth_event);
								}
							}
						}
					} else {
						warn!("Auth event list invalid");
					}

					graph.insert(next_id.clone(), next_auth_events);
					fetched_info.insert(next_id, value);
				},
				| Err(e) => {
					debug!(
						target: "auth_chain",
						"Failed to fetch event {next_id} from all fallback servers: {e}"
					);
					back_off(next_id);
				},
			}
		}

		if fetched_info.is_empty() {
			break;
		}

		let event_fetch = |event_id: OwnedEventId| {
			let origin_server_ts = fetched_info
				.get(&event_id)
				.and_then(|info| info.get("origin_server_ts"))
				.and_then(CanonicalJsonValue::as_integer)
				.map(i64::from)
				.and_then(|i| ruma::UInt::try_from(i).ok())
				.unwrap_or_else(|| ruma::uint!(0));

			future::ready(conduwuit_core::Result::Ok((
				ruma::int!(0),
				ruma::MilliSecondsSinceUnixEpoch(origin_server_ts),
			)))
		};

		let sorted = state_res::lexicographical_topological_sort(&graph, &event_fetch)
			.await
			.unwrap_or_else(|e| {
				warn!("lexicographical_topological_sort failed for batch: {e}");
				let mut ids: Vec<_> = fetched_info.keys().cloned().collect();
				ids.sort_unstable();
				ids
			});

		let events_in_order: Vec<(OwnedEventId, CanonicalJsonObject)> = sorted
			.into_iter()
			.filter_map(|id| fetched_info.remove(&id).map(|info| (id, info)))
			.collect();

		let mut suspended = false;
		let mut unprocessed = Vec::new();

		for (next_id, value) in events_in_order {
			if suspended {
				unprocessed.push((next_id, value));
				continue;
			}

			if let Some((time, tries)) = self
				.services
				.globals
				.bad_event_ratelimiter
				.read()
				.get(&*next_id)
			{
				const MIN_DURATION: u64 = 5 * 60;
				const MAX_DURATION: u64 = 60 * 60 * 24;
				if continue_exponential_backoff_secs(
					MIN_DURATION,
					MAX_DURATION,
					time.elapsed(),
					*tries,
				) {
					debug!("Backing off from {next_id}");
					continue;
				}
			}

			trace!("Handling outlier {next_id}");
			match Box::pin(self.handle_outlier_pdu(
				origin,
				create_event,
				&next_id,
				room_id,
				value.clone(),
				true,
				skip_sig_verify,
				room_version_override,
			))
			.await
			{
				| Ok((pdu, json)) => {
					processed_pdus.insert(next_id.clone(), (pdu, Some(json)));
				},
				| Err(e) =>
					if let conduwuit::Error::MissingAuthEvents(missing) = &e {
						debug_info!(
							"Suspending outlier {next_id} to fetch {} missing auth events",
							missing.len()
						);
						for auth_event in missing {
							if !graph.contains_key(auth_event)
								&& !self.services.timeline.pdu_exists(auth_event).await
							{
								push_fetch(auth_event.clone(), true, &mut active_fetches);
								graph.insert(auth_event.clone(), HashSet::new());
							}
						}
						suspended = true;
						unprocessed.push((next_id, value));
					} else {
						warn!(target: "auth_chain", "Permanently backing off event {next_id} after auth failure: {e:?}");
						back_off(next_id);
					},
			}
		}

		for (id, val) in unprocessed {
			fetched_info.insert(id, val);
		}

		if !suspended {
			break;
		}
	}

	let mut final_pdus = Vec::with_capacity(requested_seeds.len());

	for (id, pdu) in pre_resolved_pdus {
		processed_pdus.insert(id, (pdu, None));
	}

	for id in requested_seeds {
		if let Some(pdu_tuple) = processed_pdus.remove(&id) {
			final_pdus.push(pdu_tuple);
		}
	}

	trace!("Fetched and handled {} outlier pdus", final_pdus.len());
	final_pdus
}
