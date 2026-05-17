use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	time::{Duration, Instant},
};

use conduwuit::{
	implement, info,
	matrix::event::gen_event_id_canonical_json,
	utils::stream::{IterStream, ReadyExt, TryWidebandExt},
};
use futures::{StreamExt, TryStreamExt, stream::FuturesUnordered};
use ruma::{
	OwnedEventId, OwnedServerName, RoomId, RoomVersionId, api::federation::event::get_event,
};

/// Pre-fetch missing auth chain events from federation BEFORE acquiring
/// the room mutex lock. This runs in parallel across multiple servers
/// with a time budget to avoid blocking the pipeline.
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

	// Find events in the auth chain that we don't have locally
	let all_auth_ids: HashSet<&OwnedEventId> = auth_chain_sets.iter().flatten().collect();
	let mut missing: Vec<OwnedEventId> = Vec::new();
	for event_id in &all_auth_ids {
		if !self.services.timeline.pdu_exists(event_id).await {
			missing.push((*event_id).clone());
		}
	}

	if missing.is_empty() {
		return;
	}

	// Build server list in priority order:
	//  1. origin (the server that sent the transaction)
	//  2. trusted/notary servers (from config)
	//  3. room member servers (fan out to peers in the room)
	let mut servers: Vec<OwnedServerName> = Vec::new();
	servers.push(origin.to_owned());
	for s in &self.services.server.config.trusted_servers {
		if !self.services.globals.server_is_ours(s) && !servers.contains(s) {
			servers.push(s.clone());
		}
	}
	// Fan out to room member servers
	let room_servers: Vec<OwnedServerName> = self
		.services
		.state_cache
		.room_servers(room_id)
		.ready_filter(|s| {
			!self.services.globals.server_is_ours(s) && !servers.iter().any(|x| x == s)
		})
		.map(ToOwned::to_owned)
		.take(20)
		.collect()
		.await;
	servers.extend(room_servers);

	info!(
		count = missing.len(),
		servers = servers.len(),
		"Pre-fetching missing auth chain events"
	);

	// Parallel fetch with 50s budget, 32 concurrency
	let started = Instant::now();
	let budget = Duration::from_secs(50);
	let mut fetched = 0_usize;
	let mut active = FuturesUnordered::new();
	let mut queue = missing.into_iter().peekable();

	loop {
		// Fill up to 32 concurrent fetches
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
			if let Ok((_, value)) = gen_event_id_canonical_json(&pdu_raw, room_version_id) {
				self.services
					.outlier
					.add_pdu_outlier(&event_id, &value, Some(room_id));
				fetched = fetched.saturating_add(1);
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
