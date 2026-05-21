use std::{
	collections::{BTreeMap, HashMap, HashSet, hash_map},
	time::Instant,
};

use conduwuit::{
	Event, PduEvent, debug, implement, info, matrix::event::gen_event_id_canonical_json,
	state_res, trace, utils::continue_exponential_backoff_secs, warn,
};
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

	// Build routing servers via shared helper: origin → trusted → room members
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

	// Track which events were the original seed requests
	let mut requested_seeds = Vec::new();
	let mut pre_resolved_pdus = Vec::new();

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

		let id_clone = id.to_owned();
		let servers = routing_servers.clone();
		let sem = fetch_concurrency.clone();
		active_fetches.push(
			async move {
				let _permit = sem.acquire().await;
				let reqs = servers.iter().enumerate().map(|(i, server)| {
					let id_clone = id_clone.clone();
					async move {
						let start = Instant::now();
						match self
							.services
							.sending
							.send_federation_request(server, get_event::v1::Request {
								event_id: id_clone.clone(),
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
									debug!(%id_clone, %server, "Origin server failed: {e}");
								}
								Err(e)
							},
						}
					}
					.boxed()
				});

				match future::select_ok(reqs).await {
					| Ok((res, _rem)) => (id_clone, Ok(res)),
					| Err(_all_errors) => {
						debug!(%id_clone, n_servers = servers.len(), "All fallback servers exhausted");
						(
							id_clone,
							Err(conduwuit::err!(Request(NotFound(
								"event not found after trying all servers"
							)))),
						)
					},
				}
			}
			.boxed(),
		);
		graph.insert(id.to_owned(), HashSet::new());
	}

	while let Some((next_id, fetch_res)) = active_fetches.next().await {
		match fetch_res {
			| Ok((res, successful_server)) => {
				debug!("Got {next_id} over federation from {successful_server}");

				let room_version_id = match create_event {
					| Some(ce) => crate::rooms::event_handler::get_room_version_id(ce)
						.unwrap_or(ruma::RoomVersionId::V11),
					| None => {
						let mut version = None;
						if let Ok(json) = serde_json::from_str::<serde_json::Value>(res.pdu.get())
						{
							if json.get("type").and_then(|t| t.as_str()) == Some("m.room.create")
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
							| None => self
								.services
								.state
								.get_room_version_or_fallback(room_id)
								.await
								.unwrap_or(ruma::RoomVersionId::V11),
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
						"Server didn't return event id we requested: requested: {next_id}, we \
						 got {calculated_event_id}. Event: {:?}",
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

							if !graph.contains_key(&auth_event)
								&& !self.services.timeline.pdu_exists(&auth_event).await
							{
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

								trace!("Found auth event id {auth_event} for event {next_id}");
								let auth_event_clone = auth_event.clone();
								let servers = routing_servers.clone();
								let sem = fetch_concurrency.clone();
								active_fetches.push(
									async move {
										let _permit = sem.acquire().await;
										for attempt in 0..2_u8 {
											if attempt > 0 {
												tokio::time::sleep(std::time::Duration::from_secs(
													2_u64.pow(attempt.into()),
												))
												.await;
												debug!(%auth_event_clone, attempt, "Retrying auth event fetch");
											}
											let reqs = servers.iter().enumerate().map(|(i, server)| {
												let auth_event_clone = auth_event_clone.clone();
												async move {
													match self
														.services
														.sending
														.send_federation_request(
															server,
															get_event::v1::Request {
																event_id: auth_event_clone.clone(),
																include_unredacted_content: None,
															},
														)
														.await
													{
														| Ok(res) => Ok((res, server.clone())),
														| Err(e) => {
															if i == 0 {
																debug!(%auth_event_clone, %server, "Origin server failed: {e}");
															}
															Err(e)
														},
													}
												}
												.boxed()
											});

											match future::select_ok(reqs).await {
												| Ok((res, _rem)) => return (auth_event_clone, Ok(res)),
												| Err(_all_errors) => {
													info!(%auth_event_clone, n_servers = servers.len(), attempt, "All servers exhausted for auth event");
												},
											}
										}
										(
											auth_event_clone,
											Err(conduwuit::err!(Request(NotFound(
												"auth event not found after retries"
											)))),
										)
									}
									.boxed(),
								);
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

	// Handle pdus in topological order (from oldest auth event to newest seed
	// event) sorted is top-down (newest to oldest)? No, topological sort usually
	// returns oldest first if it's auth-chain resolved, wait, `sorted` has
	// dependencies at the front or back? The original code did:
	// `events_in_reverse_order = sorted.into_iter().rev()` and then looped
	// `.into_iter().rev()` which is back to normal topological order!
	// Yes, `lexicographical_topological_sort` returns oldest first (leaves of the
	// auth tree first). So we can just iterate `sorted` normally to process leaves
	// first!
	let events_in_order: Vec<(OwnedEventId, CanonicalJsonObject)> = sorted
		.into_iter()
		.filter_map(|id| fetched_info.remove(&id).map(|info| (id, info)))
		.collect();

	let mut processed_pdus: HashMap<
		OwnedEventId,
		(PduEvent, Option<BTreeMap<String, CanonicalJsonValue>>),
	> = HashMap::new();

	for (next_id, value) in events_in_order {
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
			false, // skip_sig_verify
		))
		.await
		{
			| Ok((pdu, json)) => {
				processed_pdus.insert(next_id.clone(), (pdu, Some(json)));
			},
			| Err(e) => {
				info!(target: "auth_chain", "Authentication of event {next_id} failed: {e:?}");
				back_off(next_id);
			},
		}
	}

	// Reconstruct the result array matching the requested_seeds order
	let mut final_pdus = Vec::with_capacity(requested_seeds.len());

	// First add the pre-resolved ones (that we found locally)
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
