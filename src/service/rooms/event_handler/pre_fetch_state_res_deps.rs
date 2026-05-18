use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	time::{Duration, Instant},
};

use conduwuit::{
	implement, info,
	utils::stream::{IterStream, TryWidebandExt},
	warn,
};
use futures::{StreamExt, TryStreamExt, stream::FuturesUnordered};
use ruma::{
	OwnedEventId, RoomId, RoomVersionId,
	api::federation::{self, event::get_event},
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
	let budget = Duration::from_secs(50);

	// Phase 1: Fetch individually missing auth chain events
	let all_auth_ids: HashSet<&OwnedEventId> = auth_chain_sets.iter().flatten().collect();
	let mut missing: Vec<OwnedEventId> = Vec::new();
	for event_id in &all_auth_ids {
		if !self.services.timeline.pdu_exists(event_id).await {
			missing.push((*event_id).clone());
		}
	}

	if !missing.is_empty() {
		info!(
			count = missing.len(),
			servers = servers.len(),
			"Pre-fetching missing auth chain events"
		);

		let mut fetched = 0_usize;
		let mut active = FuturesUnordered::new();
		let mut queue = missing.into_iter().peekable();

		loop {
			while active.len() < 32 && queue.peek().is_some() {
				let event_id = queue.next().expect("peeked");
				let servers = servers.clone();
				active.push(async move {
					for server in &servers {
						let server: &ruma::ServerName = server;
						if let Ok(res) = self
							.services
							.sending
							.send_federation_request(server, get_event::v1::Request {
								event_id: event_id.clone(),
								include_unredacted_content: None,
							})
							.await
						{
							return (event_id, Some(res.pdu));
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

			let Some((event_id, maybe_pdu)) = active.next().await else {
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
					warn!(
						%event_id,
						"Pre-fetched auth event failed signature verification, dropping"
					);
				}
			}
		}

		if fetched > 0 {
			info!(
				fetched,
				elapsed = ?started.elapsed(),
				"Pre-fetched auth chain events for state resolution"
			);
		}
	}

	// Phase 2: Backfill ~100 recent PDUs to fill prev_events gaps that
	// state_res needs for conflicted subgraph walks. Always runs regardless
	// of auth chain completeness. Tries multiple servers until one succeeds.
	if started.elapsed() < budget {
		let latest_ids: Vec<OwnedEventId> = incoming_state.values().cloned().collect();
		if !latest_ids.is_empty() {
			for server in &servers {
				if let Ok(response) = self
					.services
					.sending
					.send_federation_request(
						server,
						federation::backfill::get_backfill::v1::Request {
							room_id: room_id.to_owned(),
							v: latest_ids.clone(),
							limit: 100_u32.into(),
						},
					)
					.await
				{
					let mut backfilled = 0_usize;
					for pdu_raw in &response.pdus {
						if let Ok((eid, value)) = self
							.services
							.server_keys
							.validate_and_add_event_id(pdu_raw, room_version_id)
							.await
						{
							if !self.services.timeline.pdu_exists(&eid).await {
								self.services.outlier.add_pdu_outlier(
									&eid,
									&value,
									Some(room_id),
								);
								backfilled = backfilled.saturating_add(1);
							}
						}
					}
					if backfilled > 0 {
						info!(
							backfilled,
							total_received = response.pdus.len(),
							%server,
							"Pre-fetched DAG history via backfill for state resolution"
						);
					}
					// Got data from at least one server, done
					break;
				}
			}
		}
	}
}
