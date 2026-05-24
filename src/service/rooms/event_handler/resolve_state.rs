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
	warn,
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

	// Build OwnedEventId -> ShortStateKey reverse map from the fork states BEFORE
	// they are consumed into streams below. After state resolution completes, we
	// use this for O(1) fast-path shortstatehash lookups instead of issuing
	// ~50k concurrent get_or_create_shortstatekey DB calls.
	//
	// State resolution selects its output event_ids exclusively from the input
	// fork states, so every resolved entry will normally hit this fast path.
	// The get_or_create_shortstatekey fallback handles truly new state events
	// (rare -- e.g., a new join that wasn't in either input fork).
	let eid_to_ssk: HashMap<OwnedEventId, u64> = fork_states
		.iter()
		.flat_map(|fs| fs.iter().map(|(&ssk, eid)| (eid.clone(), ssk)))
		.collect();

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
	let n_fork_states: usize = fork_states.iter().map(HashMap::len).sum();
	let n_auth_chain: usize = auth_chain_sets.iter().map(HashSet::len).sum();
	info!(%room_id, n_fork_states, n_auth_chain, "state_res: auth chains loaded, starting resolution");
	let t = std::time::Instant::now();
	let state = self
		.state_resolution(room_id, room_version_id, fork_states.iter(), &auth_chain_sets)
		.boxed()
		.await?;
	info!(%room_id, n_resolved = state.len(), elapsed = ?t.elapsed(), "state_res: resolution complete");

	// Diagnostic: log resolved PL
	for ((ty, sk), eid) in &state {
		if ty.to_string() == "m.room.power_levels" {
			info!("resolve_state RESULT PL ({ty},{sk}) => {eid}");
		}
	}
	trace!("State resolution done.");
	let eid_to_ssk = &eid_to_ssk;
	let state_events: Vec<_> = state
		.iter()
		.stream()
		.wide_then(|((event_type, state_key), event_id)| async move {
			// FAST PATH: ~99.9% of resolved events were in a fork state; their
			// ShortStateKey is already known in memory — no DB call needed.
			if let Some(&ssk) = eid_to_ssk.get(event_id) {
				return (ssk, event_id.clone());
			}
			// SLOW PATH: truly new state event (e.g., a new join member event).
			let ssk = self
				.services
				.short
				.get_or_create_shortstatekey(event_type, state_key)
				.await;
			(ssk, event_id.clone())
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
	let fetch_cache = scc::HashMap::new();
	let fetch_cache_ref = &fetch_cache;

	// Populate pdu.rejected at fetch time so iterative_auth_check can use
	// the synchronous event.rejected() method instead of async DB lookups.
	let event_fetch = |event_id: OwnedEventId| async move {
		if let Some(pdu) = fetch_cache_ref
			.read_async(&event_id, |_, v: &Option<conduwuit_core::PduEvent>| v.clone())
			.await
		{
			return pdu;
		}
		let mut pdu = self.event_fetch(Some(room_id), event_id.clone()).await;

		// Populate rejection flag from pdu_metadata DB, gated by config.
		// This replaces the old event_rejected callback with a single
		// check at fetch time — O(1) field access during state-res instead
		// of O(N×M) async DB lookups.
		if let Some(ref mut p) = pdu {
			let config = &self.services.server.config;
			let meta = &self.services.pdu_metadata;
			p.rejected = (config.state_res_ignore_admin_rejected
				&& meta.is_event_admin_rejected(&event_id).await)
				|| (config.state_res_ignore_rejected && meta.is_event_rejected(&event_id).await)
				|| (config.state_res_ignore_soft_failed
					&& meta.is_event_soft_failed(&event_id).await);
		}

		let _ = fetch_cache_ref.insert_async(event_id, pdu.clone()).await;
		pdu
	};

	let event_batch_fetch = |event_ids: Vec<OwnedEventId>| async move {
		let pdus: Vec<conduwuit_core::PduEvent> = self
			.services
			.timeline
			.multi_get_pdus(Some(room_id), event_ids.into_iter().stream())
			.filter_map(|r| async move { r.ok() })
			.collect()
			.await;

		for pdu in &pdus {
			let _ = fetch_cache_ref
				.insert_async(pdu.event_id.clone(), Some(pdu.clone()))
				.await;
		}

		pdus
	};

	let event_missing_cb = move |missing_events: Vec<OwnedEventId>| {
		// Can't do async federation fetches here (sync callback inside state_res).
		// The ingestion pipeline (handle_outlier_pdu, fetch_prev, fetch_state) is
		// responsible for pre-fetching auth events before we reach this point.
		if !missing_events.is_empty() {
			let formatted_events = if missing_events.len() > 10 {
				format!(
					"{:?}, ... {} more ..., {:?}",
					&missing_events[..5],
					missing_events.len().saturating_sub(10),
					&missing_events[missing_events.len().saturating_sub(5)..]
				)
			} else {
				format!("{missing_events:?}")
			};

			warn!(
				target: "state_res_debug",
				count = missing_events.len(),
				events = %formatted_events,
				"state_res: skipping missing auth chain events (best-effort)"
			);
		}
	};

	state_res::resolve(
		room_version,
		state_sets,
		auth_chain_sets,
		&event_fetch,
		Some(&event_batch_fetch),
		Some(&event_missing_cb),
	)
	.map_err(|e| err!(error!("State resolution failed: {e:?}")))
	.await
}
