use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	sync::Arc,
};

use conduwuit::{
	Error, Result, err, implement, info,
	matrix::event::gen_event_id_canonical_json,
	state_res::{self, StateMap},
	trace,
	utils::stream::{IterStream, ReadyExt, TryWidebandExt, WidebandExt},
	warn,
};
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join};
use ruma::{
	OwnedEventId, OwnedServerName, RoomId, RoomVersionId, api::federation::event::get_event,
};

use crate::rooms::state_compressor::CompressedState;

#[implement(super::Service)]
#[tracing::instrument(name = "resolve", level = "debug", skip_all)]
pub async fn resolve_state(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	incoming_state: HashMap<u64, OwnedEventId>,
	fetch_servers: Option<&[OwnedServerName]>,
) -> Result<Arc<CompressedState>> {
	trace!("Loading current room state ids");
	let current_sstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.map_err(|e| err!(Database(error!("No state for {room_id:?}: {e:?}"))))
		.await?;

	let current_state_ids: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(current_sstatehash)
		.collect()
		.await;

	trace!("Loading fork states");
	let fork_states = [current_state_ids, incoming_state];
	let auth_chain_sets = fork_states
		.iter()
		.try_stream()
		.wide_and_then(|state| {
			self.services
				.auth_chain
				.event_ids_iter(room_id, state.values().map(Borrow::borrow))
				.try_collect()
		})
		.try_collect::<Vec<HashSet<OwnedEventId>>>();

	let fork_states = fork_states
		.iter()
		.stream()
		.wide_then(|fork_state| {
			let shortstatekeys = fork_state.keys().copied().stream();
			let event_ids = fork_state.values().cloned().stream();
			self.services
				.short
				.multi_get_statekey_from_short(shortstatekeys)
				.zip(event_ids)
				.ready_filter_map(|(ty_sk, id)| Some((ty_sk.ok()?, id)))
				.collect()
		})
		.map(Ok::<_, Error>)
		.try_collect::<Vec<StateMap<OwnedEventId>>>();

	let (fork_states, auth_chain_sets) = try_join(fork_states, auth_chain_sets).await?;

	// Pre-fetch missing auth chain events from federation before state
	// resolution. calculate_conflicted_subgraph silently drops entire DAG
	// branches when events are missing locally, producing wrong results.
	let all_auth_ids: HashSet<&OwnedEventId> = auth_chain_sets.iter().flatten().collect();
	let mut missing: Vec<OwnedEventId> = Vec::new();
	for event_id in &all_auth_ids {
		if !self.event_exists((*event_id).clone()).await {
			missing.push((*event_id).clone());
		}
	}

	// Build server list: fetch_servers first, then trusted notaries
	let mut servers_to_try: Vec<OwnedServerName> = Vec::new();
	if let Some(servers) = fetch_servers {
		servers_to_try.extend_from_slice(servers);
	}
	for s in &self.services.server.config.trusted_servers {
		if !self.services.globals.server_is_ours(s) && !servers_to_try.contains(s) {
			servers_to_try.push(s.clone());
		}
	}

	if !missing.is_empty() {
		info!(
			count = missing.len(),
			"Pre-fetching missing auth chain events before state resolution"
		);

		let mut fetched = 0_usize;

		'next_event: for event_id in &missing {
			for server in &servers_to_try {
				let server: &ruma::ServerName = server;
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
						if let Ok((_, value)) =
							gen_event_id_canonical_json(&res.pdu, room_version_id)
						{
							// Persist as outlier so event_fetch finds it during
							// state resolution
							self.services.outlier.add_pdu_outlier(
								event_id,
								&value,
								Some(room_id),
							);
							fetched = fetched.saturating_add(1);
							continue 'next_event;
						}
					},
					| Err(_) => continue,
				}
			}
			warn!(%event_id, "Failed to pre-fetch auth chain event from any server");
		}

		if fetched > 0 {
			info!(
				fetched,
				total_missing = missing.len(),
				"Pre-fetched auth chain events for state resolution"
			);
		}
	}

	// Diagnostic: log PL events in each fork state
	for (i, fork) in fork_states.iter().enumerate() {
		for ((ty, sk), eid) in fork {
			if ty.to_string() == "m.room.power_levels" {
				info!("resolve_state fork[{i}] PL ({ty},{sk}) => {eid}");
			}
		}
	}

	trace!("Resolving state");
	let state = self
		.state_resolution(
			room_id,
			room_version_id,
			fork_states.iter(),
			&auth_chain_sets,
			&servers_to_try,
		)
		.boxed()
		.await?;

	// Diagnostic: log resolved PL
	for ((ty, sk), eid) in &state {
		if ty.to_string() == "m.room.power_levels" {
			info!("resolve_state RESULT PL ({ty},{sk}) => {eid}");
		}
	}
	trace!("State resolution done.");
	let state_events: Vec<_> = state
		.iter()
		.stream()
		.wide_then(|((event_type, state_key), event_id)| {
			self.services
				.short
				.get_or_create_shortstatekey(event_type, state_key)
				.map(move |shortstatekey| (shortstatekey, event_id.clone()))
		})
		.collect()
		.await;

	trace!("Compressing state...");
	let new_room_state: CompressedState = self
		.services
		.state_compressor
		.compress_state_events(state_events.iter().map(|(ssk, eid)| (ssk, eid.borrow())))
		.collect()
		.await;

	Ok(Arc::new(new_room_state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "ruma", level = "debug", skip_all, fields(%room_id))]
pub async fn state_resolution<'a, StateSets>(
	&'a self,
	room_id: &RoomId,
	room_version: &'a RoomVersionId,
	state_sets: StateSets,
	auth_chain_sets: &'a [HashSet<OwnedEventId>],
	fetch_servers: &'a [OwnedServerName],
) -> Result<StateMap<OwnedEventId>>
where
	StateSets: Iterator<Item = &'a StateMap<OwnedEventId>> + Clone + Send,
{
	let event_fetch = |event_id: OwnedEventId| async move {
		// Try local first
		if let Some(pdu) = self.event_fetch(Some(room_id), event_id.clone()).await {
			return Some(pdu);
		}
		// Try federation fallback
		for server in fetch_servers {
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
				if let Ok((_, value)) = gen_event_id_canonical_json(&res.pdu, room_version) {
					self.services
						.outlier
						.add_pdu_outlier(&event_id, &value, Some(room_id));
					if let Some(pdu) = self.event_fetch(Some(room_id), event_id.clone()).await {
						return Some(pdu);
					}
				}
			}
		}
		None
	};
	let event_exists = |event_id: OwnedEventId| async move { self.event_exists(event_id).await };
	let event_rejected = |event_id: OwnedEventId| async move {
		// Synapse parity: only hard-rejected events are excluded from state
		// resolution. Soft-failed events must still participate to heal state
		// forks correctly (they are valid per the DAG auth chain, just
		// out-of-order relative to current state).
		self.services
			.pdu_metadata
			.is_event_rejected(&event_id)
			.await
	};

	state_res::resolve(
		room_version,
		state_sets,
		auth_chain_sets,
		&event_fetch,
		&event_exists,
		&event_rejected,
	)
	.map_err(|e| err!(error!("State resolution failed: {e:?}")))
	.await
}
