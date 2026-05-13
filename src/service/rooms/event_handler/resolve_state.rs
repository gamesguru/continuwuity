use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	sync::Arc,
};

use conduwuit::{
	Error, Result, err, implement, info,
	matrix::Event,
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

	// Keep a copy for the post-filter membership regression check
	let current_state_ids_ref = current_state_ids.clone();

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

	trace!("Resolving state");
	let state = self
		.state_resolution(room_id, room_version_id, fork_states.iter(), &auth_chain_sets)
		.boxed()
		.await?;

	trace!("State resolution done.");
	let mut state_events: Vec<_> = state
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

	// FIX: Prevent the "Hotel California" state-res bug where state-res
	// incorrectly resurrects older membership events from stale fork branches.
	//
	// In Matrix state-res v2, when two forks conflict on a membership state key,
	// state-res builds a "base state" from their intersection. If the forks
	// diverged before the user joined, the base state won't contain their join
	// event. When state-res auth-checks the newer leave event against this base,
	// the auth check fails (can't leave a room you're not in), and the leave is
	// dropped — allowing the stale join to win by default.
	//
	// We fix this by post-filtering: if state-res picked a membership event that
	// is older (by origin_server_ts) than the one already in our current state,
	// we override it to keep our current (newer) event.
	let mut membership_overrides = 0_usize;
	for (shortstatekey, event_id) in &mut state_events {
		let Some(current_event_id) = current_state_ids_ref.get(shortstatekey) else {
			continue;
		};

		if current_event_id == event_id {
			continue;
		}

		// Only check membership events — other state types (power levels, etc.) may
		// legitimately have an older event win due to higher power level.
		let Ok((event_type, _)) = self
			.services
			.short
			.get_statekey_from_short(*shortstatekey)
			.await
		else {
			continue;
		};

		if event_type.to_string() != "m.room.member" {
			continue;
		}

		// Compare timestamps: if state-res picked an older event, keep the current one.
		let (Ok(resolved_pdu), Ok(current_pdu)) = (
			self.services.timeline.get_pdu(event_id).await,
			self.services.timeline.get_pdu(current_event_id).await,
		) else {
			continue;
		};

		if resolved_pdu.origin_server_ts() < current_pdu.origin_server_ts() {
			info!(
				"State-res sought to resurrect older membership event {} (ts={}) over {} \
				 (ts={}) in {room_id}, keeping current event to prevent Hotel California \
				 regression",
				event_id,
				resolved_pdu.origin_server_ts().get(),
				current_event_id,
				current_pdu.origin_server_ts().get(),
			);
			*event_id = current_event_id.clone();
			membership_overrides = membership_overrides.saturating_add(1);
		}
	}

	if membership_overrides > 0 {
		info!(
			"Overrode {membership_overrides} stale membership event(s) from state-res in \
			 {room_id}"
		);
	}

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
#[tracing::instrument(name = "ruma", level = "debug", skip_all)]
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
	let event_fetch = |event_id| self.event_fetch(Some(room_id), event_id);
	let event_exists = |event_id| self.event_exists(event_id);
	let event_rejected = |event_id: OwnedEventId| async move {
		self.services
			.pdu_metadata
			.is_event_soft_failed(&event_id)
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
