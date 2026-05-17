use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	sync::Arc,
};

use conduwuit::{
	Error, Result, err, implement, info,
	state_res::{self, StateMap},
	trace,
	utils::stream::{IterStream, ReadyExt, TryWidebandExt, WidebandExt},
};
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join};
use ruma::{OwnedEventId, RoomId, RoomVersionId};

use crate::rooms::state_compressor::CompressedState;

#[implement(super::Service)]
#[tracing::instrument(name = "resolve", level = "debug", skip_all)]
pub async fn resolve_state(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	incoming_state: HashMap<u64, OwnedEventId>,
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

	// Do NOT fetch from federation here. State resolution must be local-only
	// to avoid blocking. Missing auth chain events cause state_res to skip those
	// subgraph branches — producing a best-effort result with local data. The
	// ingestion pipeline (handle_outlier_pdu, fetch_prev) is responsible for
	// pre-fetching auth events before we reach this point.

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
		.state_resolution(room_id, room_version_id, fork_states.iter(), &auth_chain_sets)
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
) -> Result<StateMap<OwnedEventId>>
where
	StateSets: Iterator<Item = &'a StateMap<OwnedEventId>> + Clone + Send,
{
	let event_fetch = |event_id: OwnedEventId| async move {
		// Local-only: never fetch from federation during state resolution.
		// Federation I/O can block for minutes/hours and stall the entire
		// inbound transaction pipeline. If the event isn't local, state_res
		// handles None gracefully (skips the subgraph branch).
		self.event_fetch(Some(room_id), event_id).await
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
