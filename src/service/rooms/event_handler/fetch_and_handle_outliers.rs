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

	// Cap fallback servers to prevent thread exhaustion during bulk 404s
	routing_servers.truncate(4);

	debug!(
		origin = %origin,
		n_total = routing_servers.len(),
		"Built federation fallback server list for outlier fetching"
	);

	let mut events_with_auth_events = Vec::with_capacity(events.clone().count());
	trace!("Fetching {} outlier pdus", events.clone().count());

	for id in events {
		if let Ok(local_pdu) = self.services.timeline.get_pdu(id).await {
			if self.services.pdu_metadata.is_event_soft_failed(id).await {
				info!(target: "auth_chain", "Found known soft-failed outlier locally: {id}");
			} else {
				trace!("Found {id} in main timeline or outlier tree");
			}
			events_with_auth_events.push((id.to_owned(), Some(local_pdu), vec![]));
			continue;
		}

		// If the event is soft-failed but we couldn't parse it into a local_pdu (e.g. invalid JSON),
		// we MUST skip it so we don't spam the network trying to fetch it again.
		if self.services.pdu_metadata.is_event_soft_failed(id).await {
			warn!(target: "auth_chain", "Skipping unparseable soft-failed outlier: {id}");
			continue;
		}

		let mut fetched_info: HashMap<OwnedEventId, CanonicalJsonObject> = HashMap::new();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::with_capacity(32);
		let mut active_fetches = FuturesUnordered::new();

		let limit = self.services.server.config.max_fetch_prev_events;
		if let Some((time, tries)) = self.services.globals.bad_event_ratelimiter.read().get(id) {
			const MIN_DURATION: u64 = 60 * 2;
			const MAX_DURATION: u64 = 60 * 60 * 8;
			// These logs can be disabled with CONDUWUIT_LOG="auth_chain=off,backfill=off"
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
			} else {
				let id_clone = id.to_owned();
				let servers = routing_servers.clone();
				active_fetches.push(
					async move {
						for (i, server) in servers.iter().enumerate() {
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
									return (id_clone, Ok((res, server.clone())));
								},
								| Err(e) => {
									self.update_peer_stats(server, false, start.elapsed());
									if i == 0 {
										debug!(%id_clone, %server, "Origin server failed: {e}");
									}
								},
							}
						}
						warn!(%id_clone, n_servers = servers.len(), "All fallback servers exhausted");
						(
							id_clone,
							Err(conduwuit::err!(Request(NotFound(
								"event not found after trying all servers"
							)))),
						)
					}
					.boxed(),
				);
				graph.insert(id.to_owned(), HashSet::new());
			}
		} else {
			let id_clone = id.to_owned();
			let servers = routing_servers.clone();
			active_fetches.push(
				async move {
					for (i, server) in servers.iter().enumerate() {
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
								return (id_clone, Ok((res, server.clone())));
							},
							| Err(e) => {
								self.update_peer_stats(server, false, start.elapsed());
								if i == 0 {
									debug!(%id_clone, %server, "Origin server failed: {e}");
								}
							},
						}
					}
					warn!(%id_clone, n_servers = servers.len(), "All fallback servers exhausted");
					(
						id_clone,
						Err(conduwuit::err!(Request(NotFound(
							"event not found after trying all servers"
						)))),
					)
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
						| None => self
							.services
							.state
							.get_room_version_or_fallback(room_id)
							.await
							.unwrap_or(ruma::RoomVersionId::V11),
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

									trace!(
										"Found auth event id {auth_event} for event {next_id}"
									);
									let auth_event_clone = auth_event.clone();
									let servers = routing_servers.clone();
									active_fetches.push(
										async move {
											for attempt in 0..2_u8 {
												if attempt > 0 {
													tokio::time::sleep(std::time::Duration::from_secs(2_u64.pow(attempt.into()))).await;
													debug!(%auth_event_clone, attempt, "Retrying auth event fetch");
												}
												for (i, server) in servers.iter().enumerate() {
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
														| Ok(res) =>
															return (auth_event_clone, Ok((res, server.clone()))),
														| Err(e) => {
															if i == 0 {
																debug!(%auth_event_clone, %server, "Origin server failed: {e}");
															}
															let _ = e;
														},
													}
												}
												warn!(%auth_event_clone, n_servers = servers.len(), attempt, "All servers exhausted for auth event");
											}
											(auth_event_clone, Err(conduwuit::err!(Request(NotFound("auth event not found after retries")))))
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
					info!(
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
				warn!("lexicographical_topological_sort failed for {id}: {e}");
				// Fallback to ensuring we do not arbitrarily drop successfully fetched events
				// if the graph has a cycle or is structurally broken by network truncation.
				{
					let mut ids: Vec<_> = fetched_info.keys().cloned().collect();
					ids.sort_unstable();
					ids
				}
			});

		let events_in_reverse_order: Vec<(OwnedEventId, CanonicalJsonObject)> = sorted
			.into_iter()
			.rev()
			.filter_map(|id| fetched_info.remove(&id).map(|info| (id, info)))
			.collect();

		events_with_auth_events.push((id.to_owned(), None, events_in_reverse_order));
	}

	let mut pdus = Vec::with_capacity(events_with_auth_events.len());
	for (id, local_pdu, events_in_reverse_order) in events_with_auth_events {
		if let Some(local_pdu) = local_pdu {
			trace!("Found {id} in main timeline or outlier tree");
			pdus.push((local_pdu.clone(), None));
		}

		for (next_id, value) in events_in_reverse_order.into_iter().rev() {
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
				| Ok((pdu, json)) =>
					if next_id == *id {
						trace!("Handled outlier {next_id} (original request)");
						pdus.push((pdu, Some(json)));
					},
				| Err(e) => {
					info!(
						target: "auth_chain",
						"Authentication of event {next_id} failed: {e:?}"
					);
					back_off(next_id);
				},
			}
		}
	}
	trace!("Fetched and handled {} outlier pdus", pdus.len());
	pdus
}
