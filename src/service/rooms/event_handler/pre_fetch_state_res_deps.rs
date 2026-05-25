use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	time::{Duration, Instant},
};

use conduwuit::{
	Event, implement, info,
	utils::stream::{BroadbandExt, IterStream, TryWidebandExt},
	warn,
};
use futures::{StreamExt, TryStreamExt, stream::FuturesUnordered};
use ruma::{
	OwnedEventId, RoomId, RoomVersionId,
	api::federation::event::{get_event, get_missing_events},
};

/// Pre-fetch missing auth chain events and recent DAG history from federation
/// BEFORE acquiring the room mutex lock. This runs in parallel across multiple
/// servers with a time budget to avoid blocking the pipeline.
#[implement(super::Service)]
#[tracing::instrument(name = "prefetch", level = "debug", skip_all)]
pub(super) async fn pre_fetch_state_res_deps(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	incoming_state: &HashMap<u64, OwnedEventId>,
	origin: &ruma::ServerName,
) {
	// Load current room state
	let Ok(current_sstatehash) = self.services.state.get_room_shortstatehash(room_id).await
	else {
		return;
	};

	let current_state_ids: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(current_sstatehash)
		.collect()
		.await;

	// Compute auth chain sets for both fork states
	let auth_chain_sets: Vec<HashSet<OwnedEventId>> = match [&current_state_ids, incoming_state]
		.iter()
		.try_stream()
		.wide_and_then(|state: &&HashMap<u64, OwnedEventId>| {
			self.services
				.auth_chain
				.event_ids_iter(room_id, state.values().map(Borrow::borrow))
				.try_collect()
		})
		.try_collect()
		.await
	{
		| Ok(sets) => sets,
		| Err(e) => {
			info!("Could not compute auth chains for pre-fetch: {e}");
			return;
		},
	};

	// Build server list once via shared helper
	let servers = self
		.build_federation_server_list(
			room_id,
			origin,
			self.services.server.config.federation_fallback_room_servers,
		)
		.await;

	let started = Instant::now();
	let budget = Duration::from_secs(self.services.server.config.pdu_receive_timeout);

	// Phase 1: Fetch individually missing auth chain events
	let all_auth_ids: HashSet<&OwnedEventId> = auth_chain_sets.iter().flatten().collect();
	let all_auth_count = all_auth_ids.len();
	let missing: Vec<OwnedEventId> = all_auth_ids
		.into_iter()
		.stream()
		.broad_filter_map(|event_id| async move {
			// Skip events already in the timeline
			if self.services.timeline.pdu_exists(event_id).await {
				return None;
			}

			// Skip events already stored as outliers (including rejected ones)
			if self
				.services
				.outlier
				.get_pdu_outlier(event_id)
				.await
				.is_ok()
			{
				return None;
			}

			// Skip events marked as rejected or soft-failed (e.g.
			// permanently backed off, unfetchable, or failed sig verify)
			if !self.services.pdu_metadata.is_event_accepted(event_id).await {
				return None;
			}

			Some(event_id.clone())
		})
		.collect()
		.await;

	let skipped = all_auth_count.saturating_sub(missing.len());
	if !missing.is_empty() {
		info!(
			total = all_auth_count,
			skipped,
			missing = missing.len(),
			servers = servers.len(),
			"Pre-fetching missing auth chain events"
		);

		let mut fetched = 0_usize;
		let mut active = FuturesUnordered::new();
		let mut queue = missing.into_iter().peekable();

		let concurrency = self.services.server.concurrency_scaled(2);

		loop {
			while active.len() < concurrency && queue.peek().is_some() {
				let event_id = queue.next().expect("peeked");
				let servers = servers.clone();
				active.push(async move {
					for server in servers.iter().take(2) {
						let server: &ruma::ServerName = server;
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
								return (event_id, Some(res.pdu));
							},
							| Err(_) => {
								self.update_peer_stats(server, false, start.elapsed());
							},
						}
					}
					(event_id, None)
				});
			}

			if active.is_empty() {
				break;
			}

			// Check budget
			if started.elapsed() > budget {
				info!(
					elapsed = ?started.elapsed(),
					fetched,
					remaining = active.len().saturating_add(queue.count()),
					"Pre-fetch budget exhausted, proceeding with partial auth chain"
				);
				break;
			}

			let time_left = budget.saturating_sub(started.elapsed());
			if time_left.is_zero() {
				break;
			}

			let Ok(Some((event_id, maybe_pdu))) =
				tokio::time::timeout(time_left, active.next()).await
			else {
				info!("Pre-fetch budget exhausted (timeout during active wait)");
				break;
			};

			if let Some(pdu_raw) = maybe_pdu {
				// We must validate signatures before trusting pre-fetched events.
				// Blindly inserting unverified events allows malicious servers to forge
				// power levels and hijack state resolution.
				if let Ok((eid, value)) = self
					.services
					.server_keys
					.validate_and_add_event_id(&pdu_raw, room_version_id)
					.await
				{
					if eid == event_id {
						self.services
							.outlier
							.add_pdu_outlier(&event_id, &value, Some(room_id));
						fetched = fetched.saturating_add(1);
					}
				} else {
					// Persist as rejected outlier so we don't re-fetch from
					// federation on every auth chain walk
					if let Ok((eid, value)) =
						conduwuit::matrix::event::gen_event_id_canonical_json(
							&pdu_raw,
							room_version_id,
						) {
						if eid == event_id {
							self.services.pdu_metadata.mark_event_rejected(&event_id);
							self.services.outlier.add_pdu_outlier(
								&event_id,
								&value,
								Some(room_id),
							);
							warn!(
								%event_id,
								"Pre-fetched auth event failed signature verification, storing as rejected outlier"
							);
						} else {
							warn!(
								%event_id,
								%eid,
								"Pre-fetched auth event ID mismatch, soft-failing"
							);
							self.services.pdu_metadata.mark_event_soft_failed(&event_id);
						}
					} else {
						warn!(
							%event_id,
							"Pre-fetched auth event failed signature verification; could not parse, soft-failing"
						);
						self.services.pdu_metadata.mark_event_soft_failed(&event_id);
					}
				}
			} else {
				// No server returned this event - mark as soft-failed so we
				// don't waste time re-fetching on every subsequent state_res
				warn!(
					%event_id,
					"Auth chain event unfetchable from all servers, soft-failing"
				);
				self.services.pdu_metadata.mark_event_soft_failed(&event_id);
			}
		}

		if fetched > 0 {
			info!(
				fetched,
				elapsed = ?started.elapsed(),
				"Pre-fetched auth chain events for state resolution"
			);
		}
	} else if skipped > 0 {
		info!(
			total = all_auth_count,
			skipped,
			"All auth chain events already known locally, skipping pre-fetch"
		);
	}

	// Phase 2: Iterative DAG gap filling via POST /get_missing_events.
	//
	// Each round fans out to ALL servers in parallel (FuturesUnordered). As
	// responses arrive, events are inserted as outliers and the gap shrinks.
	// We then recompute both boundaries and run another round until the gap
	// is closed or the budget is exhausted. POST body is immune to URI length
	// limits (unlike GET /backfill). Skips brand-new rooms (no shortstatehash)
	// to avoid hitting Complement mock servers with UnexpectedRequestsAreErrors.
	let has_timeline = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.is_ok();
	if !has_timeline || started.elapsed() >= budget {
		return;
	}

	let mut still_needed: Vec<OwnedEventId> = incoming_state.values().cloned().collect();
	let mut total_filled: usize = 0;
	let mut round: usize = 0;
	const MAX_ROUNDS: usize = 3;

	loop {
		if started.elapsed() >= budget || round >= MAX_ROUNDS {
			break;
		}

		// Filter to IDs still not present locally.
		let mut remaining = Vec::with_capacity(still_needed.len());
		for id in &still_needed {
			if !self.services.timeline.pdu_exists(id).await
				&& self.services.outlier.get_pdu_outlier(id).await.is_err()
			{
				remaining.push(id.clone());
			}
		}
		if remaining.is_empty() {
			break;
		}

		// Recompute local DAG boundary — grows with each filled round.
		let earliest: Vec<OwnedEventId> = self
			.services
			.state
			.get_forward_extremities(room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		round = round.saturating_add(1);

		// Fan out to a capped subset of servers to avoid overwhelming
		// the executor and disk I/O on resource-constrained boxes.
		let server_fanout = self.services.server.concurrency_scaled(1);
		let mut active = FuturesUnordered::new();
		for server in servers.iter().take(server_fanout) {
			let room_id_owned = room_id.to_owned();
			let earliest = earliest.clone();
			let remaining = remaining.clone();
			active.push(async move {
				let t = Instant::now();
				let res = self
					.services
					.sending
					.send_federation_request(server, get_missing_events::v1::Request {
						room_id: room_id_owned,
						earliest_events: earliest,
						latest_events: remaining,
						limit: 50_u32.into(),
						min_depth: 0_u32.into(),
					})
					.await;
				(server, res, t.elapsed())
			});
		}

		let mut round_filled: usize = 0;
		while let Some((server, res, latency)) = active.next().await {
			match res {
				| Ok(response) => {
					self.update_peer_stats(server, true, latency);
					for pdu_raw in &response.events {
						if let Ok((eid, value)) = self
							.services
							.server_keys
							.validate_and_add_event_id(pdu_raw, room_version_id)
							.await
						{
							if let Ok(pdu) = conduwuit::PduEvent::from_id_val(
								&eid,
								value.clone(),
								Some(room_id),
							) {
								if pdu.room_id_or_hash().as_deref() == Some(room_id) {
									if !self.services.timeline.pdu_exists(&eid).await {
										self.services.outlier.add_pdu_outlier(
											&eid,
											&value,
											Some(room_id),
										);
										round_filled = round_filled.saturating_add(1);
									}
								} else {
									warn!(%eid, %server, "get_missing_events returned event for wrong room");
								}
							}
						}
					}
				},
				| Err(_) => {
					self.update_peer_stats(server, false, latency);
				},
			}
		}

		total_filled = total_filled.saturating_add(round_filled);
		if round_filled > 0 {
			info!(
				round,
				round_filled,
				total_filled,
				still_open = remaining.len().saturating_sub(round_filled),
				"Phase 2: get_missing_events round complete"
			);
			// next round will re-filter still_needed
			still_needed = remaining;
		} else {
			// No server returned anything useful — gap won't close further.
			break;
		}

		// Yield between rounds to keep the executor responsive.
		tokio::task::yield_now().await;
	}

	if total_filled > 0 {
		info!(total_filled, rounds = round, "Phase 2: DAG gap filling complete");
	}
}
