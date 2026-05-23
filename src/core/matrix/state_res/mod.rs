#![cfg_attr(test, allow(warnings))]

pub(crate) mod error;
pub mod event_auth;
mod power_levels;
mod room_version;

#[cfg(test)]
mod test_utils;

#[cfg(test)]
mod benches;

use std::{
	borrow::Borrow,
	cmp::{Ordering, Reverse},
	collections::{BinaryHeap, HashMap, HashSet},
	hash::{BuildHasher, Hash},
	sync::Arc,
};

use dashmap::DashMap;
use futures::{Future, FutureExt, Stream, StreamExt, TryStreamExt, future};
use ruma::{
	EventId, Int, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomVersionId,
	events::{
		StateEventType, TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	int, uint,
};
use serde_json::from_str as from_json_str;
use smallvec::SmallVec;
use tokio::sync::OnceCell;

use self::power_levels::PowerLevelsContentFields;
pub use self::{
	error::Error,
	event_auth::{auth_check, auth_types_for_event},
	room_version::RoomVersion,
};
use crate::{
	debug, debug_error, info,
	matrix::{Event, StateKey},
	state_res::room_version::StateResolutionVersion,
	trace,
	utils::stream::{BroadbandExt, IterStream, ReadyExt, TryWidebandExt, WidebandExt},
	warn,
};

/// A mapping of event type and state_key to some value `T`, usually an
/// `EventId`.
pub type StateMap<T> = HashMap<TypeStateKey, T>;
pub type StateMapItem<T> = (TypeStateKey, T);
pub type TypeStateKey = (StateEventType, StateKey);

type Result<T, E = Error> = crate::Result<T, E>;

/// Resolve sets of state events as they come in.
///
/// Internally `StateResolution` builds a graph and an auth chain to allow for
/// state conflict resolution.
///
/// ## Arguments
///
/// * `state_sets` - The incoming state to resolve. Each `StateMap` represents a
///   possible fork in the state of a room.
///
/// * `auth_chain_sets` - The full recursive set of `auth_events` for each event
///   in the `state_sets`.
///
/// * `event_fetch` - Any event not found in the `event_map` will defer to this
///   closure to find the event.
///
/// ## Invariants
///
/// The caller of `resolve` must ensure that all the events are from the same
/// room. Although this function takes a `RoomId` it does not check that each
/// event is part of the same room.
//#[tracing::instrument(level = "debug", skip(state_sets, auth_chain_sets,
//#[tracing::instrument(level event_fetch))]
#[allow(clippy::cognitive_complexity)]
pub async fn resolve<'a, Pdu, Sets, SetIter, Hasher, Fetch, FetchFut, Cb>(
	room_version: &RoomVersionId,
	state_sets: Sets,
	auth_chain_sets: &'a [HashSet<OwnedEventId, Hasher>],
	event_fetch: &Fetch,
	event_missing_cb: Option<&Cb>,
) -> Result<StateMap<OwnedEventId>>
where
	Fetch: Fn(OwnedEventId) -> FetchFut + Sync,
	FetchFut: Future<Output = Option<Pdu>> + Send,
	Cb: Fn(Vec<OwnedEventId>) + Sync,
	Sets: IntoIterator<IntoIter = SetIter> + Send,
	SetIter: Iterator<Item = &'a StateMap<OwnedEventId>> + Clone + Send,
	Hasher: BuildHasher + Send + Sync,
	Pdu: Event + Clone + Send + Sync,
	for<'b> &'b Pdu: Event + Send,
{
	use RoomVersionId::*;
	let stateres_version = match room_version {
		| V1 => StateResolutionVersion::V1,
		| V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 | V11 => StateResolutionVersion::V2,
		| _ => StateResolutionVersion::V2_1,
	};
	debug!(version = ?stateres_version, "State resolution starting");
	let fetch_cache: Arc<DashMap<OwnedEventId, Arc<OnceCell<Option<Pdu>>>>> =
		Arc::new(DashMap::new());
	let _parsed_pl_cache: Arc<DashMap<OwnedEventId, Arc<PowerLevelsContentFields>>> =
		Arc::new(DashMap::new());

	let cached_fetch = |id: OwnedEventId| {
		let cache = Arc::clone(&fetch_cache);
		async move {
			if let Some(cell) = cache.get(&id) {
				return cell
					.get_or_init(|| async { event_fetch(id.clone()).await })
					.await
					.clone();
			}
			let cell = cache
				.entry(id.clone())
				.or_insert_with(|| Arc::new(OnceCell::new()))
				.value()
				.clone();
			cell.get_or_init(|| async { event_fetch(id).await })
				.await
				.clone()
		}
	};

	// Split non-conflicting and conflicting state
	let (mut unconflicted, mut conflicting) = separate(state_sets.into_iter());

	debug!(count = unconflicted.len(), "non-conflicting events");
	trace!(map = ?unconflicted, "non-conflicting events");

	if conflicting.is_empty() {
		debug!("no conflicting state found");
		return Ok(unconflicted);
	}

	if stateres_version == StateResolutionVersion::V2_1 {
		// MSC4297: For room versions > 11, the "clean" state is the empty set,
		// and the "conflicting" state is the set of all state events in the set of
		// states to resolve.
		for (k, v) in unconflicted.drain() {
			conflicting.insert(k, vec![v]);
		}
	}

	debug!(count = conflicting.len(), "conflicting events");
	trace!(map = ?conflicting, "conflicting events");
	let (conflicted_state_subgraph, initial_state) =
		if stateres_version == StateResolutionVersion::V2_1 {
			let (csg, missing) = calculate_conflicted_subgraph(&conflicting, &cached_fetch)
				.await
				.ok_or_else(|| {
					Error::InvalidPdu("Failed to calculate conflicted subgraph".to_owned())
				})?;
			debug!(count = csg.len(), "conflicted subgraph");
			trace!(set = ?csg, "conflicted subgraph");
			if !missing.is_empty() {
				if let Some(cb) = event_missing_cb {
					cb(missing);
				}
			}
			(csg, HashMap::new())
		} else {
			(HashSet::new(), unconflicted.clone())
		};

	// `all_conflicted` contains unique items
	// synapse says `full_set = {eid for eid in full_conflicted_set if eid in
	// event_map}`
	// Hydra: Also consider the conflicted state subgraph
	let auth_diff_stream = get_auth_chain_diff(auth_chain_sets).boxed();

	let all_conflicted_ids: HashSet<_> = auth_diff_stream
		.chain(conflicting.into_values().flatten().stream())
		.chain(conflicted_state_subgraph.into_iter().stream())
		.collect()
		.await;

	let all_conflicted: HashSet<_> = all_conflicted_ids
		.into_iter()
		.stream()
		// Filter out non-existent events and non-state events in a single fetch.
		// event_fetch returns None for missing events (same as event_exists would),
		// and we need the event body anyway to check state_key — doing two separate
		// broad passes would double the DB round-trips (up to 2× |all_conflicted|
		// lookups). Auth chains and prev_events subgraph traversals can pull in
		// non-state events (e.g. m.room.message) which lack a state_key; these must
		// be excluded since iterative_auth_check requires all events to have one.
		.broad_filter_map(async |id| {
			let ev = event_fetch(id.clone()).await?;
			ev.state_key().is_some().then_some(id)
		})
		.collect()
		.await;

	debug!(count = all_conflicted.len(), "full conflicted set");
	trace!(set = ?all_conflicted, "full conflicted set");

	let total_auth_chain: usize = auth_chain_sets.iter().map(HashSet::len).sum();
	if total_auth_chain > 10_000 {
		warn!(
			total_auth_chain,
			num_sets = auth_chain_sets.len(),
			"Auth chain exceeds 10k events — possible DAG bloat or amplification attack"
		);
	}
	if all_conflicted.len() > 5_000 {
		warn!(
			count = all_conflicted.len(),
			"Conflicted set exceeds 5k events — state resolution may be slow"
		);
	}

	// We used to check that all events are events from the correct room
	// this is now a check the caller of `resolve` must make.

	let room_version = RoomVersion::new(room_version)?;

	// -- Isolate ALL power/foundation events for sub-resolution. --
	//    V2.1 starts from empty state, so the PL auth check requires:
	//    1. m.room.create - to establish the room
	//    2. m.room.join_rules - needed for join auth checks
	//    3. m.room.member - sender's join must be in state for PL auth
	//    4. m.room.power_levels - the actual PL events
	//    This matches Synapse/ruma-lean behavior where ALL member events
	//    go through the Kahn-sorted power event path.
	let conflicted_pl_events: Vec<_> = all_conflicted
		.iter()
		.stream()
		.wide_filter_map(async |id| {
			let ev = event_fetch(id.clone()).await?;
			let dominated = matches!(
				ev.event_type(),
				TimelineEventType::RoomPowerLevels
					| TimelineEventType::RoomCreate
					| TimelineEventType::RoomJoinRules
					| TimelineEventType::RoomMember
			);
			dominated.then_some(id.clone())
		})
		.collect()
		.await;

	debug!(
		"PL sub-resolution: found {} power/foundation events in conflicted set \
		 (all_conflicted={})",
		conflicted_pl_events.len(),
		all_conflicted.len()
	);

	// -- Sub-resolve the PL+create events using the 100/0 bootstrap --
	//    (global_pl_context = None)
	let sorted_pl_events = reverse_topological_power_sort(
		conflicted_pl_events,
		&all_conflicted,
		&event_fetch,
		None, // Bootstrap mode
	)
	.await?;

	let partially_resolved_pl_state = iterative_auth_check(
		&room_version,
		sorted_pl_events.iter().stream().map(AsRef::as_ref),
		initial_state.clone(),
		&event_fetch,
	)
	.await?;

	debug!(entries = partially_resolved_pl_state.len(), "partially resolved PL state");

	// -- Extract the authoritative global power level context --
	let mut global_pl_context = None;
	let power_levels_ty_sk = (StateEventType::RoomPowerLevels, StateKey::new());
	if let Some(pl_event_id) = partially_resolved_pl_state.get(&power_levels_ty_sk) {
		debug!(%pl_event_id, "selected global PL event");
		if let Some(pl_event) = event_fetch(pl_event_id.clone()).await {
			if let Ok(c) = from_json_str::<PowerLevelsContentFields>(pl_event.content().get()) {
				global_pl_context = Some(c);
			} else {
				warn!(%pl_event_id, "failed to parse global PL event content");
			}
		} else {
			warn!(%pl_event_id, "failed to fetch global PL event");
		}
	} else {
		warn!("no global PL event found in partially resolved PL state");
	}

	// Get only the control events with a state_key: "" or ban/kick event (sender !=
	// state_key)
	let control_events: Vec<_> = all_conflicted
		.iter()
		.stream()
		.wide_filter_map(async |id| {
			is_power_event_id(id, &event_fetch)
				.await
				.then_some(id.clone())
		})
		.collect()
		.await;

	// -- Sort the control events based on power_level/clock/event_id and --
	// outgoing/incoming edges, using the global context
	let sorted_control_levels = reverse_topological_power_sort(
		control_events,
		&all_conflicted,
		&event_fetch,
		global_pl_context.as_ref(),
	)
	.await?;

	debug!(count = sorted_control_levels.len(), "power events");
	if sorted_control_levels.len() <= 10 {
		info!(
			"using {} sorted power events: {:?}",
			sorted_control_levels.len(),
			sorted_control_levels
		);
	} else {
		info!(
			"using {} sorted power events: {:?} ... {:?}",
			sorted_control_levels.len(),
			&sorted_control_levels[..5],
			&sorted_control_levels[sorted_control_levels.len().saturating_sub(5)..],
		);
	}
	trace!(list = ?sorted_control_levels, "sorted power events");

	// Sequentially auth check each control event.
	let resolved_control = iterative_auth_check(
		&room_version,
		sorted_control_levels.iter().stream().map(AsRef::as_ref),
		initial_state,
		&event_fetch,
	)
	.await?;

	debug!(count = resolved_control.len(), "resolved power events");
	trace!(map = ?resolved_control, "resolved power events");

	// At this point the control_events have been resolved we now have to
	// sort the remaining events using the mainline of the resolved power level.
	let deduped_power_ev: HashSet<_> = sorted_control_levels.into_iter().collect();

	debug!(count = deduped_power_ev.len(), "deduped power events");
	trace!(set = ?deduped_power_ev, "deduped power events");

	// This removes the control events that passed auth and more importantly those
	// that failed auth
	let events_to_resolve: Vec<_> = all_conflicted
		.iter()
		.filter(|&id| !deduped_power_ev.contains(id))
		.cloned()
		.collect();

	debug!(count = events_to_resolve.len(), "events left to resolve");
	trace!(list = ?events_to_resolve, "events left to resolve");

	// This "epochs" power level event
	let power_levels_ty_sk = (StateEventType::RoomPowerLevels, StateKey::new());
	let power_event = resolved_control.get(&power_levels_ty_sk);

	trace!(event_id = ?power_event, "power event");

	let sorted_left_events =
		mainline_sort(&events_to_resolve, power_event.cloned(), &event_fetch).await?;

	trace!(list = ?sorted_left_events, "events left, sorted, running iterative auth check");

	let mut resolved_state = iterative_auth_check(
		&room_version,
		sorted_left_events.iter().stream().map(AsRef::as_ref),
		resolved_control, // The control events are added to the final resolved state
		&event_fetch,
	)
	.await?;

	// Ensure unconflicting state is in the final state
	resolved_state.extend(unconflicted);

	debug!("state resolution finished");
	trace!( map = ?resolved_state, "final resolved state" );

	Ok(resolved_state)
}

/// Split the events that have no conflicts from those that are conflicting.
///
/// The return tuple looks like `(unconflicted, conflicted)`.
///
/// State is determined to be conflicting if for the given key (StateEventType,
/// StateKey) there is not exactly one event ID. This includes missing events,
/// if one state_set includes an event that none of the other have this is a
/// conflicting event.
fn separate<'a, Id>(
	state_sets_iter: impl Iterator<Item = &'a StateMap<Id>>,
) -> (StateMap<Id>, StateMap<Vec<Id>>)
where
	Id: Clone + Eq + Hash + 'a,
{
	let mut state_set_count: usize = 0;
	let mut occurrences = HashMap::<_, HashMap<_, _>>::new();

	let state_sets_iter =
		state_sets_iter.inspect(|_| state_set_count = state_set_count.saturating_add(1));

	for (k, v) in state_sets_iter.flatten() {
		occurrences
			.entry(k)
			.or_default()
			.entry(v)
			.and_modify(|x: &mut usize| *x = x.saturating_add(1))
			.or_insert(1);
	}

	let mut unconflicted_state = StateMap::new();
	let mut conflicted_state = StateMap::new();

	for (k, v) in occurrences {
		for (id, occurrence_count) in v {
			if occurrence_count == state_set_count {
				unconflicted_state.insert((k.0.clone(), k.1.clone()), id.clone());
			} else {
				conflicted_state
					.entry((k.0.clone(), k.1.clone()))
					.and_modify(|x: &mut Vec<_>| x.push(id.clone()))
					.or_insert_with(|| vec![id.clone()]);
			}
		}
	}

	(unconflicted_state, conflicted_state)
}

/// Calculate the conflicted subgraph
pub(crate) async fn calculate_conflicted_subgraph<F, Fut, E>(
	conflicted: &StateMap<Vec<OwnedEventId>>,
	fetch_event: &F,
) -> Option<(HashSet<OwnedEventId>, Vec<OwnedEventId>)>
where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send + Sync,
{
	let conflicted_events: HashSet<_> = conflicted.values().flatten().cloned().collect();

	let mut min_depth = ruma::UInt::MAX;
	for event_id in &conflicted_events {
		if let Some(evt) = fetch_event(event_id.clone()).await {
			if evt.depth() < min_depth {
				min_depth = evt.depth();
			}
		}
	}

	let mut subgraph: HashSet<OwnedEventId> = HashSet::new();
	let mut stack: Vec<Vec<OwnedEventId>> =
		vec![conflicted_events.iter().cloned().collect::<Vec<_>>()];
	let mut path: Vec<OwnedEventId> = Vec::new();
	let mut seen: HashSet<OwnedEventId> = HashSet::new();
	let mut missing: Vec<OwnedEventId> = Vec::new();
	let next_event = |stack: &mut Vec<Vec<_>>, path: &mut Vec<_>| {
		while stack.last().is_some_and(Vec::is_empty) {
			stack.pop();
			path.pop();
		}
		stack.last_mut().and_then(Vec::pop)
	};
	while let Some(event_id) = next_event(&mut stack, &mut path) {
		path.push(event_id.clone());
		if subgraph.contains(&event_id) {
			if path.len() > 1 {
				subgraph.extend(path.iter().cloned());
			}
			path.pop();
			continue;
		}
		if conflicted_events.contains(&event_id) && path.len() > 1 {
			subgraph.extend(path.iter().cloned());
			path.pop();
			continue;
		}
		if seen.contains(&event_id) {
			path.pop();
			continue;
		}
		trace!(event_id = event_id.as_str(), "fetching event for its prev events");
		let evt = fetch_event(event_id.clone()).await;
		if evt.is_none() {
			missing.push(event_id.clone());
			seen.insert(event_id);
			path.pop();
			continue;
		}

		let evt = evt.expect("checked");
		if evt.depth() < min_depth {
			seen.insert(event_id.clone());
			path.pop();
			continue;
		}

		stack.push(evt.prev_events().map(ToOwned::to_owned).collect());
		seen.insert(event_id);
	}
	if !missing.is_empty() {
		info!(
			n_missing = missing.len(),
			n_seen = seen.len(),
			n_subgraph = subgraph.len(),
			"conflicted subgraph has missing prev_events (DAG holes)"
		);
		for (i, eid) in missing.iter().enumerate() {
			if i < 25 {
				info!(event_id = %eid, "missing prev_event dependency");
			}
		}
		if missing.len() > 25 {
			info!("... and {} more missing prev_events", missing.len().saturating_sub(25));
		}
	}
	Some((subgraph, missing))
}

/// Returns a Vec of deduped EventIds that appear in some chains but not others.
#[allow(clippy::arithmetic_side_effects)]
fn get_auth_chain_diff<'a, Id, Hasher>(
	auth_chain_sets: &'a [HashSet<Id, Hasher>],
) -> impl Stream<Item = Id> + Send + use<'a, Id, Hasher>
where
	Id: Clone + Eq + Hash + Send + Sync + 'a,
	Hasher: BuildHasher + Send + Sync,
{
	let num_sets = auth_chain_sets.len();
	let mut id_counts: HashMap<&'a Id, usize> = HashMap::new(); // Borrowed!
	for id in auth_chain_sets.iter().flatten() {
		let count = id_counts.entry(id).or_default();
		*count = count.saturating_add(1);
	}

	id_counts
		.into_iter()
		.filter(move |&(_id, count)| count < num_sets)
		.map(|(id, _count)| id.clone())
		.stream()
}

/// Events are sorted from "earliest" to "latest".
///
/// They are compared using the negative power level (reverse topological
/// ordering), the origin server timestamp and in case of a tie the `EventId`s
/// are compared lexicographically.
///
/// The power level is negative because a higher power level is equated to an
/// earlier (further back in time) origin server timestamp.
#[tracing::instrument(level = "debug", skip_all)]
async fn reverse_topological_power_sort<E, F, Fut>(
	events_to_sort: Vec<OwnedEventId>,
	auth_diff: &HashSet<OwnedEventId>,
	fetch_event: &F,
	global_pl_context: Option<&PowerLevelsContentFields>,
) -> Result<Vec<OwnedEventId>>
where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send + Sync + Clone,
{
	debug!("reverse topological sort of power events");

	let mut graph = HashMap::new();
	for event_id in events_to_sort {
		add_event_and_auth_chain_to_graph(&mut graph, event_id, auth_diff, fetch_event).await;
	}

	let parsed_pl_cache: Arc<DashMap<OwnedEventId, Arc<PowerLevelsContentFields>>> =
		Arc::new(DashMap::new());

	// This is used in the `key_fn` passed to the lexico_topo_sort fn
	let sender_pl_cache: Arc<DashMap<(ruma::OwnedUserId, Option<OwnedEventId>), Int>> =
		Arc::new(DashMap::new());
	let event_to_pl: HashMap<_, _> = graph
		.keys()
		.cloned()
		.stream()
		.broad_filter_map(async |event_id| {
			let pl = get_power_level_for_sender(
				&event_id,
				fetch_event,
				global_pl_context,
				&parsed_pl_cache,
				&sender_pl_cache,
			)
			.await;

			Some((event_id, pl))
		})
		.inspect(|(event_id, pl)| {
			debug!(
				event_id = event_id.as_str(),
				power_level = i64::from(*pl),
				"found the power level of an event's sender",
			);
		})
		.collect()
		.boxed()
		.await;

	let fetcher = async |event_id: OwnedEventId| {
		let pl = *event_to_pl
			.get(&event_id)
			.ok_or_else(|| Error::NotFound(String::new()))?;

		let ev = fetch_event(event_id)
			.await
			.ok_or_else(|| Error::NotFound(String::new()))?;

		Ok((pl, ev.origin_server_ts()))
	};

	lexicographical_topological_sort(&graph, &fetcher).await
}

/// Sorts the event graph based on number of outgoing/incoming edges.
///
/// `key_fn` is used as to obtain the power level and age of an event for
/// breaking ties (together with the event ID).
#[tracing::instrument(level = "debug", skip_all)]
pub async fn lexicographical_topological_sort<Id, F, Fut, Hasher, S>(
	graph: &HashMap<Id, HashSet<Id, Hasher>, S>,
	key_fn: &F,
) -> Result<Vec<Id>>
where
	F: Fn(Id) -> Fut + Sync,
	Fut: Future<Output = Result<(Int, MilliSecondsSinceUnixEpoch)>> + Send,
	Id: Borrow<EventId> + Clone + Eq + Hash + Ord + Send + Sync,
	Hasher: BuildHasher + Default + Clone + Send + Sync,
	S: BuildHasher + Clone + Send + Sync,
{
	#[derive(PartialEq, Eq)]
	struct TieBreaker<'a, Id> {
		power_level: Int,
		origin_server_ts: MilliSecondsSinceUnixEpoch,
		event_id: &'a Id,
	}

	impl<Id> Ord for TieBreaker<'_, Id>
	where
		Id: Ord,
	{
		fn cmp(&self, other: &Self) -> Ordering {
			// NOTE: the power level comparison is "backwards" intentionally.
			// See the "Mainline ordering" section of the Matrix specification
			// around where it says the following:
			//
			// > for events `x` and `y`, `x < y` if [...]
			//
			// <https://spec.matrix.org/v1.12/rooms/v11/#definitions>
			other
				.power_level
				.cmp(&self.power_level)
				.then(self.origin_server_ts.cmp(&other.origin_server_ts))
				.then(self.event_id.cmp(other.event_id))
		}
	}

	impl<Id> PartialOrd for TieBreaker<'_, Id>
	where
		Id: Ord,
	{
		fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
	}

	debug!("starting lexicographical topological sort");

	// NOTE: an event that has no incoming edges happened most recently,
	// and an event that has no outgoing edges happened least recently.

	// NOTE: this is basically Kahn's algorithm except we look at nodes with no
	// outgoing edges, c.f.
	// https://en.wikipedia.org/wiki/Topological_sorting#Kahn's_algorithm

	// outdegree_map is an event referring to the events before it, the
	// more outdegree's the more recent the event.
	let mut outdegree_map = graph.clone();

	// The number of events that depend on the given event (the EventId key)
	// How many events reference this event in the DAG as a parent
	let mut reverse_graph: HashMap<_, HashSet<_, Hasher>> = HashMap::new();

	// Vec of nodes that have zero out degree, least recent events.
	let mut zero_outdegree = Vec::new();

	for (node, edges) in graph {
		if edges.is_empty() {
			let (power_level, origin_server_ts) = key_fn(node.clone()).await?;
			// The `Reverse` is because rusts `BinaryHeap` sorts largest -> smallest we need
			// smallest -> largest
			zero_outdegree.push(Reverse(TieBreaker {
				power_level,
				origin_server_ts,
				event_id: node,
			}));
		}

		reverse_graph.entry(node).or_default();
		for edge in edges {
			reverse_graph.entry(edge).or_default().insert(node);
		}
	}

	let mut heap = BinaryHeap::from(zero_outdegree);

	// We remove the oldest node (most incoming edges) and check against all other
	let mut sorted = vec![];
	// Destructure the `Reverse` and take the smallest `node` each time
	while let Some(Reverse(item)) = heap.pop() {
		let node = item.event_id;

		for &parent in reverse_graph
			.get(node)
			.expect("EventId in heap is also in reverse_graph")
		{
			// The number of outgoing edges this node has
			let out = outdegree_map
				.get_mut(parent.borrow())
				.expect("outdegree_map knows of all referenced EventIds");

			// Only push on the heap once older events have been cleared
			out.remove(node.borrow());
			if out.is_empty() {
				let (power_level, origin_server_ts) = key_fn(parent.clone()).await?;
				heap.push(Reverse(TieBreaker {
					power_level,
					origin_server_ts,
					event_id: parent,
				}));
			}
		}

		// synapse yields we push then return the vec
		sorted.push(node.clone());
	}

	Ok(sorted)
}

/// Find the power level for the sender of `event_id` or return a default value
/// of zero.
///
/// Do NOT use this any where but topological sort, we find the power level for
/// the eventId at the eventId's generation (we walk backwards to `EventId`s
/// most recent previous power level event).
async fn get_power_level_for_sender<E, F, Fut>(
	event_id: &EventId,
	fetch_event: &F,
	global_pl_context: Option<&PowerLevelsContentFields>,
	parsed_pl_cache: &DashMap<OwnedEventId, Arc<PowerLevelsContentFields>>,
	sender_pl_cache: &DashMap<(ruma::OwnedUserId, Option<OwnedEventId>), Int>,
) -> Int
where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send + Clone,
{
	debug!("fetch event ({event_id}) senders power level");

	let event = fetch_event(event_id.to_owned()).await;
	let sender_str = event.as_ref().map(|e| Event::sender(e).to_owned());

	if let Some(global_context) = global_pl_context {
		if let Some(s) = &sender_str {
			if let Some(&user_level) = global_context.get_user_power(s) {
				return user_level;
			}
			return global_context.users_default;
		}
		return int!(0);
	}

	if let Some(ev) = event {
		if is_type_and_key(&ev, &TimelineEventType::RoomCreate, "") {
			return Int::MAX;
		}

		let mut creator_event: Option<E> = None;
		let mut pl_event: Option<E> = None;

		// 1-HOP STRICT SCAN (Removes the polynomial BFS trap)
		for aid in ev.auth_events() {
			if let Some(aev) = fetch_event(aid.to_owned()).await {
				if is_type_and_key(&aev, &TimelineEventType::RoomCreate, "") {
					creator_event = Some(aev);
				} else if is_type_and_key(&aev, &TimelineEventType::RoomPowerLevels, "") {
					pl_event = Some(aev);
				}
			}
		}

		if let Some(pl_ev) = pl_event {
			let pl_id = pl_ev.event_id().to_owned();
			let parsed_pl = parsed_pl_cache
				.entry(pl_id.clone())
				.or_insert_with(|| {
					Arc::new(
						from_json_str::<PowerLevelsContentFields>(pl_ev.content().get())
							.unwrap_or_else(|_| PowerLevelsContentFields {
								users_default: int!(0),
								users: Vec::new(),
							}),
					)
				})
				.value()
				.clone();

			if let Some(s) = &sender_str {
				let cache_key = (s.clone(), Some(pl_id));
				if let Some(cached_pl) = sender_pl_cache.get(&cache_key) {
					return *cached_pl;
				}

				if let Some(&user_level) = parsed_pl.get_user_power(s) {
					sender_pl_cache.insert(cache_key, user_level);
					return user_level;
				}
				sender_pl_cache.insert(cache_key, parsed_pl.users_default);
				return parsed_pl.users_default;
			}
		} else if let Some(creator_ev) = creator_event {
			let mut is_creator = creator_ev.sender() == ev.sender();
			if let Ok(create_content) = from_json_str::<
				ruma::events::room::create::RoomCreateEventContent,
			>(creator_ev.content().get())
			{
				#[allow(deprecated)]
				if let Some(creator_user) = create_content.creator {
					is_creator = creator_user == ev.sender();
				}
			}
			if is_creator {
				return Int::MAX;
			}
		}
	}
	int!(0)
}

/// Check the that each event is authenticated based on the events before it.
///
/// ## Returns
///
/// The `unconflicted_state` combined with the newly auth'ed events. So any
/// event that fails the `event_auth::auth_check` will be excluded from the
/// returned state map.
///
/// For each `events_to_check` event we gather the events needed to auth it from
/// the the `fetch_event` closure and verify each event using the
/// `event_auth::auth_check` function.
#[tracing::instrument(level = "trace", skip_all)]
async fn iterative_auth_check<'a, E, F, Fut, S>(
	room_version: &RoomVersion,
	events_to_check: S,
	unconflicted_state: StateMap<OwnedEventId>,
	fetch_event: &F,
) -> Result<StateMap<OwnedEventId>>
where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	S: Stream<Item = &'a EventId> + Send + 'a,
	E: Event + Clone + Send + Sync,
	for<'b> &'b E: Event + Send,
{
	debug!("starting iterative auth check");

	let events_to_check: Vec<_> = events_to_check
		.map(Result::Ok)
		// NOTE: wide_and_then (ordered) is required here, NOT broad_and_then.
		// The input stream is topologically sorted; broad_and_then uses
		// buffer_unordered which destroys that ordering, causing the wrong
		// power-level event to win during state resolution.
		.wide_and_then(async |event_id| {
			fetch_event(event_id.to_owned())
				.await
				.ok_or_else(|| Error::NotFound(format!("Failed to find {event_id}")))
		})
		// SYNAPSE CHECK 1: Pre-filter rejected events synchronously via trait
		.wide_filter_map(|res| async {
			match res {
				| Ok(event) if event.rejected() => {
					info!(
						target: "state_res",
						event_id = event.event_id().as_str(),
						"skipping previously rejected event"
					);
					None
				},
				| Ok(event) => Some(Ok(event)),
				| Err(e) => Some(Err(e)),
			}
		})
		.try_collect()
		.boxed()
		.await?;

	trace!(list = ?events_to_check, "events to check");
	if events_to_check.len() > 5_000 {
		warn!(
			count = events_to_check.len(),
			"iterative_auth_check processing >5k events — possible fork storm"
		);
	}
	if events_to_check.is_empty() {
		debug!("no events to check, returning unconflicted state");
		return Ok(unconflicted_state);
	}

	let auth_event_ids: HashSet<OwnedEventId> = events_to_check
		.iter()
		.flat_map(|event: &E| event.auth_events().map(ToOwned::to_owned))
		.collect();

	trace!(set = ?auth_event_ids, "auth event IDs to fetch");

	let auth_events: HashMap<OwnedEventId, E> = auth_event_ids
		.into_iter()
		.stream()
		.broad_filter_map(fetch_event)
		// SYNAPSE CHECK 2: filter rejected auth events
		.broad_filter_map(|auth_event| async {
			if auth_event.rejected() {
				trace!(
					target: "state_res",
					event_id = auth_event.event_id().as_str(),
					"skipping rejected auth event"
				);
				None
			} else {
				Some((auth_event.event_id().to_owned(), auth_event))
			}
		})
		.collect()
		.boxed()
		.await;

	trace!(map = ?auth_events.keys().collect::<Vec<_>>(), "fetched auth events");

	let auth_events = &auth_events;
	// NOTE: in state resolution v2.1, auth checks should start with an empty state
	// map. It is the caller's job to do this. Previously, this function would
	// force an empty state map in this case, and this resulted in power events
	// going missing from the resolved state as they'd be discarded here.
	let mut resolved_state = unconflicted_state;
	for event in events_to_check {
		let mut local_create_event: Option<E> = None;
		trace!(event_id = event.event_id().as_str(), "checking event");
		let Some(state_key) = event.state_key() else {
			warn!("event {} failed the authentication check (no state key)", event.event_id());
			continue;
		};

		let auth_types = match auth_types_for_event(
			event.event_type(),
			event.sender(),
			Some(state_key),
			event.content(),
			room_version,
		) {
			| Ok(types) => types,
			| Err(e) => {
				warn!(
					"event {} failed the authentication check (invalid auth_types: {e})",
					event.event_id()
				);
				continue;
			},
		};

		trace!(list = ?auth_types, event_id = event.event_id().as_str(), "auth types for event");

		// SmallVec<[_; 4]> — stack-allocated for the common case (≤4 auth
		// events: create, join_rules, power_levels, sender_membership).
		// Eliminates per-iteration HashMap allocation.
		let mut auth_state: SmallVec<[(TypeStateKey, E); 4]> = SmallVec::new();
		if room_version.room_ids_as_hashes {
			if *event.event_type() == TimelineEventType::RoomCreate {
				auth_state.push((event.event_type().with_state_key(""), event.clone()));
			} else {
				if local_create_event.is_none() {
					trace!("room version uses hashed IDs, deriving create event from room_id");
					if let Some(room_id) = event.room_id_or_hash() {
						let create_event_id_raw = room_id.as_str().replacen('!', "$", 1);
						if let Ok(create_event_id) = EventId::parse(&create_event_id_raw) {
							local_create_event = fetch_event(create_event_id.into()).await;
						}
					}
				}
				if let Some(create_event) = &local_create_event {
					auth_state.push((
						create_event.event_type().with_state_key(""),
						create_event.clone(),
					));
				} else {
					warn!(
						"event {} failed the authentication check (missing create event)",
						event.event_id()
					);
					continue;
				}
			}
		}
		for aid in event.auth_events() {
			if let Some(ev) = auth_events.get(aid) {
				// Skip rejected events (Synapse parity: checks rejected flag)
				if ev.rejected() {
					trace!(event_id = aid.as_str(), "skipping rejected auth event");
					continue;
				}
				trace!(event_id = aid.as_str(), "found auth event");
				let key = ev
					.event_type()
					.with_state_key(ev.state_key().ok_or_else(|| {
						Error::InvalidPdu("State event had no state key".to_owned())
					})?);
				auth_state.push((key, ev.clone()));
			} else {
				warn!(event_id = aid.as_str(), "missing auth event");
			}
		}

		// In V2.1 (MSC4297), each event authenticates purely against its own
		// auth_events chain. The supplemental merge from resolved_state is
		// skipped because V2.1 starts from the empty set precisely so that
		// events are evaluated against their own auth chain, preventing
		// cascading auth failures when conflicting state (e.g. join_rules)
		// contaminates the accumulated resolved_state.
		//
		// For V2, resolved_state OVERWRITES auth_events — this forces events
		// to authenticate against authoritative state, preventing auth-bypass
		// attacks where a malicious event claims old power levels.
		if room_version.state_res != StateResolutionVersion::V2_1 {
			let supplemental: Vec<_> = auth_types
				.iter()
				.stream()
				.ready_filter_map(|key| Some((key, resolved_state.get(key)?)))
				.filter_map(|(key, ev_id)| async move {
					// Exclude rejected events from resolved_state (Synapse parity)
					// Fetch the event to check its rejected flag
					if let Some(event) = auth_events.get(ev_id) {
						if event.rejected() {
							return None;
						}
						Some((key.to_owned(), event.clone()))
					} else {
						let fetched = fetch_event(ev_id.clone()).await?;
						if fetched.rejected() {
							return None;
						}
						Some((key.to_owned(), fetched))
					}
				})
				.collect()
				.await;

			for (key, event) in supplemental {
				auth_state.push((key, event));
			}
		}

		// Sort + dedup: binary search requires ascending order, and duplicates
		// from overlapping auth_events/resolved_state must be collapsed.
		// Supplemental entries are pushed AFTER auth_events, so reverse+dedup
		// keeps supplemental (matching old HashMap overwrite semantics), then
		// re-sort ascending for binary_search.
		auth_state.sort_by(|a, b| a.0.cmp(&b.0));
		auth_state.reverse();
		auth_state.dedup_by(|a, b| a.0.eq(&b.0));
		auth_state.sort_by(|a, b| a.0.cmp(&b.0));

		trace!(
			keys = ?auth_state.iter().map(|(k, _)| k).collect::<Vec<_>>(),
			event_id = event.event_id().as_str(),
			"auth state for event"
		);

		if *event.event_type() == TimelineEventType::RoomPowerLevels {
			info!(
				event_id = event.event_id().as_str(),
				sender = %event.sender(),
				"iterative_auth_check: about to auth PL event"
			);
		}
		debug!(event_id = event.event_id().as_str(), "Running auth checks");

		// Binary search for third party invite in the sorted auth_state
		let current_third_party = auth_state.iter().find_map(|(_, pdu)| {
			(*pdu.event_type() == TimelineEventType::RoomThirdPartyInvite).then_some(pdu)
		});

		// Binary search closure: O(log n) lookup instead of HashMap hashing.
		// For ≤4 elements this is 1-2 comparisons.
		let fetch_state = |ty: &StateEventType, key: &str| {
			let needle = ty.with_state_key(key);
			future::ready(
				auth_state
					.binary_search_by(|(k, _)| k.cmp(&needle))
					.ok()
					.map(|i| auth_state[i].1.clone()),
			)
		};

		// If the event IS the create event, use it directly; otherwise use
		// the memoized create event (or discover it once from auth_state).
		// This avoids redundant auth_state lookups on every iteration.
		let create_event = if *event.event_type() == TimelineEventType::RoomCreate {
			event.clone()
		} else if let Some(ref ce) = local_create_event {
			ce.clone()
		} else {
			match fetch_state(&StateEventType::RoomCreate, "").await {
				| Some(ce) => ce,
				| None => {
					warn!(
						"event {} failed the authentication check (missing create event)",
						event.event_id()
					);
					continue;
				},
			}
		};

		let auth_result =
			auth_check(room_version, &event, current_third_party, fetch_state, &create_event)
				.await;

		match auth_result {
			| Ok(true) => {
				// add event to resolved state map
				if *event.event_type() == TimelineEventType::RoomPowerLevels {
					info!(
						event_id = event.event_id().as_str(),
						"iterative_auth_check: PL event PASSED auth, adding to resolved state"
					);
				}
				trace!(
					event_id = event.event_id().as_str(),
					"event passed the authentication check, adding to resolved state"
				);
				resolved_state.insert(
					event.event_type().with_state_key(state_key),
					event.event_id().to_owned(),
				);
				trace!(map = ?resolved_state, "new resolved state");
			},
			| Ok(false) => {
				// synapse passes here on AuthError. We do not add this event to resolved_state.
				if *event.event_type() == TimelineEventType::RoomPowerLevels {
					warn!(
						event_id = event.event_id().as_str(),
						"iterative_auth_check: PL event FAILED auth check!"
					);
				}
				warn!("event {} failed the authentication check", event.event_id());
			},
			| Err(e) => {
				debug_error!("event {} failed the authentication check: {e}", event.event_id());
				return Err(e);
			},
		}
	}
	trace!(map = ?resolved_state, "final resolved state from iterative auth check");
	debug!("iterative auth check finished");
	Ok(resolved_state)
}

/// Returns the sorted `to_sort` list of `EventId`s based on a mainline sort
/// using the depth of `resolved_power_level`, the server timestamp, and the
/// eventId.
async fn mainline_sort<E, F, Fut>(
	to_sort: &[OwnedEventId],
	resolved_power_level: Option<OwnedEventId>,
	fetch_event: &F,
) -> Result<Vec<OwnedEventId>>
where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Clone + Send + Sync,
{
	debug!("mainline sort of events");

	// There are no EventId's to sort, bail.
	if to_sort.is_empty() {
		return Ok(vec![]);
	}

	let fetch_cache: Arc<DashMap<OwnedEventId, Arc<OnceCell<Option<E>>>>> =
		Arc::new(DashMap::new());
	let cached_fetch = |id: OwnedEventId| {
		let cache: Arc<DashMap<OwnedEventId, Arc<OnceCell<Option<E>>>>> =
			Arc::clone(&fetch_cache);
		let cell: Arc<OnceCell<Option<E>>> = cache
			.entry(id.clone())
			.or_insert_with(|| Arc::new(OnceCell::new()))
			.value()
			.clone();
		async move {
			let res: &Option<E> = cell.get_or_init(|| async { fetch_event(id).await }).await;
			res.clone()
		}
	};

	// Step 1: Walk the mainline (the chain of power level events starting from the
	// resolved power level) and assign each a position. Position 0 = most recent
	// (highest priority). This is O(M) where M is the length of the mainline.
	//
	// NOTE: The mainline is a LINEAR CHAIN (each PL event references at most one
	// predecessor PL event in its auth_events). We do NOT use LCA-to-RMQ here
	// because Matrix auth chains are DAGs, not trees — the Euler tour required
	// for RMQ is only valid on trees. We use a simple HashMap for O(1) lookups.
	let mut mainline_depth: HashMap<OwnedEventId, usize> = HashMap::new();
	let mut pl = resolved_power_level;
	let mut position: usize = 0;
	while let Some(p) = pl {
		mainline_depth.insert(p.clone(), position);
		position = position.saturating_add(1);

		let Some(event) = cached_fetch(p).await else {
			break;
		};

		pl = None;
		for aid in event.auth_events() {
			let Some(aev) = cached_fetch(aid.to_owned()).await else {
				continue;
			};
			if is_type_and_key(&aev, &TimelineEventType::RoomPowerLevels, "") {
				pl = Some(aid.to_owned());
				break;
			}
		}
	}

	// Step 2: For each event to sort, find its associated power level event,
	// then look up its mainline position in O(1).
	//
	// Depth semantics (matching the original ruma state-res algorithm):
	//   - None  -> event has no PL in its auth chain; lowest priority, applied
	//     first, loses
	//   - Some(0) -> event's PL IS the resolved PL; highest priority, applied last,
	//     wins
	//   - Some(N) -> event's PL is N hops from the resolved PL; intermediate
	//     priority
	//
	// We use Option<usize> so that None < Some(0) in Rust's natural Ord ordering.
	let mut event_to_mainline: HashMap<&OwnedEventId, Option<usize>> = HashMap::new();
	let mut event_ts: HashMap<&OwnedEventId, MilliSecondsSinceUnixEpoch> = HashMap::new();

	for ev_id in to_sort {
		let Some(event) = cached_fetch(ev_id.clone()).await else {
			continue;
		};
		event_ts.insert(ev_id, event.origin_server_ts());

		let depth: Option<usize> =
			if is_type_and_key(&event, &TimelineEventType::RoomPowerLevels, "") {
				// The event IS a power level event — look itself up directly
				mainline_depth.get(ev_id).copied()
			} else {
				// 1. Find the 1-hop PL event
				let mut current_pl = None;
				for aid in event.auth_events() {
					let Some(aev) = cached_fetch(aid.to_owned()).await else {
						continue;
					};
					if is_type_and_key(&aev, &TimelineEventType::RoomPowerLevels, "") {
						current_pl = Some(aid.to_owned());
						break;
					}
				}

				// 2. Recursively walk the chain of PL events
				let mut found_depth = None;
				while let Some(c_pl) = current_pl {
					if let Some(&d) = mainline_depth.get(&c_pl) {
						found_depth = Some(d);
						break;
					}
					let Some(c_pl_ev) = cached_fetch(c_pl).await else {
						break;
					};
					current_pl = None;
					for aid in c_pl_ev.auth_events() {
						let Some(aev) = cached_fetch(aid.to_owned()).await else {
							continue;
						};
						if is_type_and_key(&aev, &TimelineEventType::RoomPowerLevels, "") {
							current_pl = Some(aid.to_owned());
							break;
						}
					}
				}
				found_depth
			};

		event_to_mainline.insert(ev_id, depth);
	}

	// Step 3: Sort by mainline position then ts/id. Applied left->right, last wins.
	//
	// None (no mainline connection) -> worst -> FIRST -> loses.
	// For events with a connection: LARGER position = FARTHER from resolved PL =
	// worse = comes FIRST.  Position 0 (closest to current PL) is LAST -> wins.
	//
	// This mirrors spec §6.6.3.3: "x < y if x.position > y.position" where ∞
	// beats all finite positions (None events have position ∞).
	let mut sort_event_ids: Vec<_> = event_to_mainline.keys().map(|&k| k.clone()).collect();

	sort_event_ids.sort_by(|a, b| {
		let da = event_to_mainline.get(a).copied().flatten();
		let db = event_to_mainline.get(b).copied().flatten();
		let ta = event_ts
			.get(a)
			.copied()
			.unwrap_or_else(|| MilliSecondsSinceUnixEpoch(uint!(0)));
		let tb = event_ts
			.get(b)
			.copied()
			.unwrap_or_else(|| MilliSecondsSinceUnixEpoch(uint!(0)));
		match (da, db) {
			// Both have no PL ancestor -> tiebreak by ts ascending then id ascending.
			| (None, None) => ta.cmp(&tb).then(a.as_str().cmp(b.as_str())),
			// No-PL events are worst (first).
			| (None, Some(_)) => Ordering::Less,
			| (Some(_), None) => Ordering::Greater,
			// Both have a PL ancestor: DESCENDING position (farther from PL first).
			// Then ascending ts (earlier ts -> loses; later ts -> wins).
			| (Some(pa), Some(pb)) => pb
				.cmp(&pa)
				.then(ta.cmp(&tb))
				.then(a.as_str().cmp(b.as_str())),
		}
	});

	Ok(sort_event_ids)
}

async fn add_event_and_auth_chain_to_graph<E, F, Fut>(
	graph: &mut HashMap<OwnedEventId, HashSet<OwnedEventId>>,
	event_id: OwnedEventId,
	auth_diff: &HashSet<OwnedEventId>,
	fetch_event: &F,
) where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send + Sync,
{
	let mut state = vec![event_id];
	while let Some(eid) = state.pop() {
		graph.entry(eid.clone()).or_default();
		let event = fetch_event(eid.clone()).await;
		let auth_events = event.as_ref().map(Event::auth_events).into_iter().flatten();

		// Prefer the store to event as the store filters dedups the events
		for aid in auth_events {
			if auth_diff.contains(aid) {
				if !graph.contains_key(aid) {
					state.push(aid.to_owned());
				}

				graph
					.get_mut(&eid)
					.expect("We just inserted this at the start of the while loop")
					.insert(aid.to_owned());
			}
		}
	}
}

async fn is_power_event_id<E, F, Fut>(event_id: &EventId, fetch: &F) -> bool
where
	F: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Option<E>> + Send,
	E: Event + Send,
{
	match fetch(event_id.to_owned()).await.as_ref() {
		| Some(state) => is_power_event(state),
		| _ => false,
	}
}

fn is_type_and_key(ev: &impl Event, ev_type: &TimelineEventType, state_key: &str) -> bool {
	ev.event_type() == ev_type && ev.state_key() == Some(state_key)
}

fn is_power_event(event: &impl Event) -> bool {
	match event.event_type() {
		| TimelineEventType::RoomPowerLevels
		| TimelineEventType::RoomJoinRules
		| TimelineEventType::RoomCreate => event.state_key() == Some(""),
		| TimelineEventType::RoomMember => {
			if let Ok(content) = from_json_str::<RoomMemberEventContent>(event.content().get()) {
				if [MembershipState::Leave, MembershipState::Ban].contains(&content.membership) {
					return Some(event.sender().as_str()) != event.state_key();
				}
			}

			false
		},
		| _ => false,
	}
}

/// Convenience trait for adding event type plus state key to state maps.
pub trait EventTypeExt {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey);
}

impl EventTypeExt for StateEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self, state_key.into())
	}
}

impl EventTypeExt for TimelineEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self.into(), state_key.into())
	}
}

impl<T> EventTypeExt for &T
where
	T: EventTypeExt + Clone,
{
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		self.to_owned().with_state_key(state_key)
	}
}

#[cfg(test)]
mod tests {
	use std::collections::{HashMap, HashSet};

	use maplit::{hashmap, hashset};
	use rand::seq::SliceRandom;
	use ruma::{
		MilliSecondsSinceUnixEpoch, OwnedEventId, RoomVersionId,
		events::{
			StateEventType, TimelineEventType,
			room::join_rules::{JoinRule, RoomJoinRulesEventContent},
		},
		int, uint,
	};
	use serde_json::{json, value::to_raw_value as to_raw_json_value};

	use super::{
		StateMap, is_power_event,
		room_version::RoomVersion,
		test_utils::{
			INITIAL_EVENTS, TestStore, alice, bob, charlie, do_check, ella, event_id,
			member_content_ban, member_content_join, member_content_leave, room_id,
			to_init_pdu_event, to_pdu_event, zara,
		},
	};
	use crate::{
		debug,
		matrix::{Event, EventTypeExt, Pdu as PduEvent},
		state_res::room_version::StateResolutionVersion,
		utils::stream::IterStream,
	};

	async fn test_event_sort() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);
		let events = INITIAL_EVENTS();

		let event_map = events
			.values()
			.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.clone()))
			.collect::<StateMap<_>>();

		let auth_chain: HashSet<OwnedEventId> = HashSet::new();

		let power_events = event_map
			.values()
			.filter(|&pdu| is_power_event(&*pdu))
			.map(|pdu| pdu.event_id.clone())
			.collect::<Vec<_>>();

		let fetcher = |id| ready(events.get(&id).cloned());
		let sorted_power_events =
			super::reverse_topological_power_sort(power_events, &auth_chain, &fetcher, None)
				.await
				.unwrap();

		let resolved_power = super::iterative_auth_check(
			&RoomVersion::V6,
			sorted_power_events.iter().map(AsRef::as_ref).stream(),
			HashMap::new(), // unconflicted events
			&fetcher,
		)
		.await
		.expect("iterative auth check failed on resolved events");

		// don't remove any events so we know it sorts them all correctly
		let mut events_to_sort = events.keys().cloned().collect::<Vec<_>>();

		events_to_sort.shuffle(&mut rand::rng());

		let power_level = resolved_power
			.get(&(StateEventType::RoomPowerLevels, "".into()))
			.cloned();

		let sorted_event_ids = super::mainline_sort(&events_to_sort, power_level, &fetcher)
			.await
			.unwrap();

		assert_eq!(
			vec![
				// No PL in auth chain -> None depth -> sort first (lowest priority, lose)
				"$CREATE:foo",
				"$IMA:foo",
				"$START:foo",
				"$END:foo",
				// PL in auth chain -> Some(0) -> sort last (highest priority, win)
				"$IPOWER:foo",
				"$IJR:foo",
				"$IMB:foo",
				"$IMC:foo",
			],
			sorted_event_ids
				.iter()
				.map(|id| id.to_string())
				.collect::<Vec<_>>()
		);
	}

	#[tokio::test]
	async fn test_sort() {
		for _ in 0..20 {
			// since we shuffle the eventIds before we sort them introducing randomness
			// seems like we should test this a few times
			test_event_sort().await;
		}
	}

	#[tokio::test]
	async fn ban_vs_power_level() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let events = &[
			to_init_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"MA",
				alice(),
				TimelineEventType::RoomMember,
				Some(alice().to_string().as_str()),
				member_content_join(),
			),
			to_init_pdu_event(
				"MB",
				alice(),
				TimelineEventType::RoomMember,
				Some(bob().to_string().as_str()),
				member_content_ban(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
		];

		let edges = vec![vec!["END", "MB", "MA", "PA", "START"], vec!["END", "PA", "PB"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["PA", "MA", "MB"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn topic_basic() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let events = &[
			to_init_pdu_event(
				"T1",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA1",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T2",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA2",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 0 } })).unwrap(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T3",
				bob(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
		];

		let edges =
			vec![vec!["END", "PA2", "T2", "PA1", "T1", "START"], vec!["END", "T3", "PB", "PA1"]]
				.into_iter()
				.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
				.collect::<Vec<_>>();

		let expected_state_ids = vec!["PA2", "T2"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn topic_reset() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let events = &[
			to_init_pdu_event(
				"T1",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T2",
				bob(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"MB",
				alice(),
				TimelineEventType::RoomMember,
				Some(bob().to_string().as_str()),
				member_content_ban(),
			),
		];

		let edges = vec![vec!["END", "MB", "T2", "PA", "T1", "START"], vec!["END", "T1"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["T1", "MB", "PA"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn join_rule_evasion() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let events = &[
			to_init_pdu_event(
				"JR",
				alice(),
				TimelineEventType::RoomJoinRules,
				Some(""),
				to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Private)).unwrap(),
			),
			to_init_pdu_event(
				"ME",
				ella(),
				TimelineEventType::RoomMember,
				Some(ella().to_string().as_str()),
				member_content_join(),
			),
		];

		let edges = vec![vec!["END", "JR", "START"], vec!["END", "ME", "START"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec![event_id("JR")];

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn offtopic_power_level() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let events = &[
			to_init_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(
					&json!({ "users": { alice(): 100, bob(): 50, charlie(): 50 } }),
				)
				.unwrap(),
			),
			to_init_pdu_event(
				"PC",
				charlie(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50, charlie(): 0 } }))
					.unwrap(),
			),
		];

		let edges = vec![vec!["END", "PC", "PB", "PA", "START"], vec!["END", "PA"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["PC"].into_iter().map(event_id).collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn topic_setting() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let events = &[
			to_init_pdu_event(
				"T1",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA1",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T2",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"PA2",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 0 } })).unwrap(),
			),
			to_init_pdu_event(
				"PB",
				bob(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
			),
			to_init_pdu_event(
				"T3",
				bob(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"MZ1",
				zara(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
			to_init_pdu_event(
				"T4",
				alice(),
				TimelineEventType::RoomTopic,
				Some(""),
				to_raw_json_value(&json!({})).unwrap(),
			),
		];

		let edges = vec![vec!["END", "T4", "MZ1", "PA2", "T2", "PA1", "T1", "START"], vec![
			"END", "MZ1", "T3", "PB", "PA1",
		]]
		.into_iter()
		.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
		.collect::<Vec<_>>();

		let expected_state_ids = vec!["T4", "PA2"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(events, edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn test_event_map_none() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let mut store = TestStore::<PduEvent>(hashmap! {});

		// build up the DAG
		let (state_at_bob, state_at_charlie, expected) = store.set_up();

		let ev_map = store.0.clone();
		let fetcher = |id| ready(ev_map.get(&id).cloned());

		let exists = |id: OwnedEventId| ready(ev_map.get(&*id).is_some());

		let state_sets = [state_at_bob, state_at_charlie];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let resolved = match super::resolve(
			&RoomVersionId::V2,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		assert_eq!(expected, resolved);
	}

	#[tokio::test]
	async fn test_lexicographical_sort() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let graph = hashmap! {
			event_id("l") => hashset![event_id("o")],
			event_id("m") => hashset![event_id("n"), event_id("o")],
			event_id("n") => hashset![event_id("o")],
			event_id("o") => hashset![], // "o" has zero outgoing edges but 4 incoming edges
			event_id("p") => hashset![event_id("o")],
		};

		let res = super::lexicographical_topological_sort(&graph, &|_id| async {
			Ok((int!(0), MilliSecondsSinceUnixEpoch(uint!(0))))
		})
		.await
		.unwrap();

		assert_eq!(
			vec!["o", "l", "n", "m", "p"],
			res.iter()
				.map(ToString::to_string)
				.map(|s| s.replace('$', "").replace(":foo", ""))
				.collect::<Vec<_>>()
		);
	}

	/// Ported from ruma-state-res `state_res::tests::test_mainline_sort`.
	/// Events connected to the mainline PL sort AFTER events with no PL
	/// ancestor.
	#[tokio::test]
	async fn ruma_test_mainline_sort() {
		use futures::future::ready;

		let events = INITIAL_EVENTS();
		let fetcher = |id| ready(events.get(&id).cloned());

		// Only the room-setup events (no disconnected START/END message events)
		let mut to_sort: Vec<OwnedEventId> = events
			.keys()
			.filter(|id| {
				let s = id.to_string();
				!s.contains("START") && !s.contains("END")
			})
			.cloned()
			.collect();

		for _ in 0..20 {
			to_sort.shuffle(&mut rand::rng());
			let power_level = events
				.iter()
				.find(|(_, ev)| {
					ev.event_type() == &ruma::events::TimelineEventType::RoomPowerLevels
				})
				.map(|(id, _)| id.clone());

			let sorted = super::mainline_sort(&to_sort, power_level, &fetcher)
				.await
				.unwrap();
			let names: Vec<String> = sorted
				.iter()
				.map(|id| id.to_string().replace("$", "").replace(":foo", ""))
				.collect();

			// No-PL-ancestor events (CREATE, IMA) come FIRST (lowest priority, lose).
			// PL-connected events (IPOWER, IJR, IMB, IMC) come LAST (win).
			assert_eq!(
				names,
				["CREATE", "IMA", "IPOWER", "IJR", "IMB", "IMC"],
				"ruma_test_mainline_sort: wrong order on iteration"
			);
		}
	}

	/// Ported from ruma-state-res
	/// `state_res::tests::test_mainline_sort_no_pl_ancestor_sorts_first`.
	/// Per spec §6.6.3.3: an event with i=∞ (no mainline ancestor) sorts BEFORE
	/// all chain-rooted events.  Directly validates our `Option<usize>`
	/// sentinel.
	#[tokio::test]
	async fn ruma_test_mainline_sort_no_pl_ancestor_sorts_first() {
		use futures::future::ready;

		let events = INITIAL_EVENTS();
		let fetcher = |id| ready(events.get(&id).cloned());

		// IMA  -> auth=[$CREATE]          -> no PL -> no mainline anchor (sorts first)
		// IJR  -> auth=[$CREATE,$IMA,$IPOWER] -> PL=$IPOWER -> mainline depth 0
		// IPOWER -> IS the PL               -> mainline depth 0 (closest, wins)
		let to_sort: Vec<OwnedEventId> = ["IMA", "IJR", "IPOWER"]
			.iter()
			.map(|s| {
				<&ruma::EventId>::try_from(format!("${s}:foo").as_str())
					.unwrap()
					.to_owned()
			})
			.collect();

		let power_level = events
			.iter()
			.find(|(_, ev)| ev.event_type() == &ruma::events::TimelineEventType::RoomPowerLevels)
			.map(|(id, _)| id.clone());

		let sorted = super::mainline_sort(&to_sort, power_level, &fetcher)
			.await
			.unwrap();
		let names: Vec<String> = sorted
			.iter()
			.map(|id| id.to_string().replace("$", "").replace(":foo", ""))
			.collect();

		// IMA (None) first. IPOWER (ts=2) and IJR (ts=3) both at depth=Some(0);
		// ascending ts within equal depth -> IPOWER before IJR -> IJR last -> IJR wins.
		assert_eq!(
			names,
			["IMA", "IPOWER", "IJR"],
			"no-PL-ancestor event must sort before mainline-connected events"
		);
	}

	/// Ported from ruma-state-res
	/// `state_res::tests::test_reverse_topological_power_sort`.
	#[tokio::test]
	async fn ruma_test_reverse_topological_power_sort() {
		let eid = |s: &str| -> OwnedEventId {
			<&ruma::EventId>::try_from(format!("${s}:foo").as_str())
				.unwrap()
				.to_owned()
		};
		let graph = [
			(eid("l"), [eid("o")].into()),
			(eid("m"), [eid("n"), eid("o")].into()),
			(eid("n"), [eid("o")].into()),
			(eid("o"), std::collections::HashSet::new()),
			(eid("p"), [eid("o")].into()),
		]
		.into();

		let sorted = super::lexicographical_topological_sort(&graph, &|_id| async {
			Ok((int!(0), MilliSecondsSinceUnixEpoch(uint!(0))))
		})
		.await
		.unwrap();

		let names: Vec<String> = sorted
			.iter()
			.map(|id| id.to_string().replace("$", "").replace(":foo", ""))
			.collect();

		assert_eq!(names, ["o", "l", "n", "m", "p"]);
	}

	#[tokio::test]
	async fn ban_with_auth_chains() {
		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);
		let ban = BAN_STATE_SET();

		let edges = vec![vec!["END", "MB", "PA", "START"], vec!["END", "IME", "MB"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["PA", "MB"]
			.into_iter()
			.map(event_id)
			.collect::<Vec<_>>();

		do_check(&ban.values().cloned().collect::<Vec<_>>(), edges, expected_state_ids).await;
	}

	#[tokio::test]
	async fn ban_with_auth_chains2() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);
		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();

		let mut inner = init.clone();
		inner.extend(ban);
		let store = TestStore(inner.clone());

		let state_set_a = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("MB")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		let state_set_b = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("IME")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		let ev_map = &store.0;
		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());
		let resolved = match super::resolve(
			&RoomVersionId::V6,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		debug!(
			resolved = ?resolved
				.iter()
				.map(|((ty, key), id)| format!("(({ty}{key:?}), {id})"))
				.collect::<Vec<_>>(),
				"resolved state",
		);

		let expected = [
			"$CREATE:foo",
			"$IJR:foo",
			"$PA:foo",
			"$IMA:foo",
			"$IMB:foo",
			"$IMC:foo",
			"$MB:foo",
		];

		for id in expected.iter().map(|i| event_id(i)) {
			// make sure our resolved events are equal to the expected list
			assert!(resolved.values().any(|eid| eid == &id) || init.contains_key(&id), "{id}");
		}
		assert_eq!(expected.len(), resolved.len());
	}

	/// Verify that rejected events are excluded from state resolution.
	/// Marks Ella's join ($IME) as rejected; she should not appear in resolved
	/// state since her join was the only membership event and it's rejected.
	#[tokio::test]
	async fn rejected_event_excluded_from_resolution() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);
		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();

		let mut inner = init.clone();
		inner.extend(ban);
		let mut store = TestStore(inner.clone());

		// State set A: has MB (ban of ella) and PA
		let state_set_a = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("MB")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		// State set B: has IME (ella's join) and PA
		let state_set_b = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("IME")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		// Mark IME (Ella's join) as rejected via the Pdu field
		store.0.get_mut(&event_id("IME")).unwrap().rejected = true;
		let ev_map = &store.0;
		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());
		let resolved = match super::resolve(
			&RoomVersionId::V6,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		// IME was rejected, so it should NOT appear in resolved state.
		// MB (the ban) should win for ella's membership slot.
		let ella_key = (StateEventType::RoomMember, ella().to_string().into());
		let ella_event = resolved.get(&ella_key);
		assert!(
			ella_event.is_none() || ella_event.unwrap() == &event_id("MB"),
			"Ella's rejected join should not appear; got {:?}",
			ella_event
		);
	}

	/// Verify that rejecting a power-level event changes the resolution
	/// outcome. Without rejection, PA wins. With PB rejected, PA should
	/// definitely win.
	#[tokio::test]
	async fn rejected_event_changes_resolution_outcome() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);
		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();

		let mut inner = init.clone();
		inner.extend(ban);
		let mut store = TestStore(inner.clone());

		let state_set_a = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("MB")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		let state_set_b = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("IME")).unwrap(),
			inner.get(&event_id("PB")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		// Mark PB as rejected via the Pdu field — PA should be the sole power level
		// winner
		store.0.get_mut(&event_id("PB")).unwrap().rejected = true;
		let ev_map = &store.0;
		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());
		let resolved = match super::resolve(
			&RoomVersionId::V6,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		let pl_key = (StateEventType::RoomPowerLevels, "".into());
		let pl_event = resolved
			.get(&pl_key)
			.expect("power levels must be in resolved state");
		assert_eq!(
			pl_event,
			&event_id("PA"),
			"With PB rejected, PA must win the power levels slot"
		);
	}

	/// The state reset loop scenario: when a stale join is NOT rejected, it
	/// can survive alongside a ban from a different fork. This proves that
	/// marking events as rejected is critical for convergence.
	#[tokio::test]
	async fn unrejected_join_survives_in_resolution() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);
		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();

		let mut inner = init.clone();
		inner.extend(ban);
		let store = TestStore(inner.clone());

		// State set A: has MB (ban of ella)
		let state_set_a = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("MB")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		// State set B: has IME (ella's join)
		let state_set_b = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("IME")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		let ev_map = &store.0;
		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());
		// Nothing rejected — both IME and MB participate
		let resolved = match super::resolve(
			&RoomVersionId::V6,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		// Without rejection, state-res picks a winner between IME and MB
		// based on auth rules. The key insight: *some* event fills ella's
		// slot. When the "wrong" one wins, that's the state reset loop.
		let ella_key = (StateEventType::RoomMember, ella().to_string().into());
		assert!(
			resolved.contains_key(&ella_key),
			"ella must have a membership entry when nothing is rejected"
		);
	}

	/// Verifies that rejecting ALL conflicting membership events for a user
	/// removes them from resolved state entirely — the nuclear option for
	/// membership cleanup.
	#[tokio::test]
	async fn reject_all_membership_events_removes_user() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);
		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();

		let mut inner = init.clone();
		inner.extend(ban);
		let mut store = TestStore(inner.clone());

		let state_set_a = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("MB")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		let state_set_b = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("IME")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		// Reject BOTH ella's join AND her ban — nuclear cleanup
		store.0.get_mut(&event_id("IME")).unwrap().rejected = true;
		store.0.get_mut(&event_id("MB")).unwrap().rejected = true;
		let ev_map = &store.0;
		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());
		let resolved = match super::resolve(
			&RoomVersionId::V6,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		{
			| Ok(state) => state,
			| Err(e) => panic!("{e}"),
		};

		// With both events rejected, ella should have no membership entry
		let ella_key = (StateEventType::RoomMember, ella().to_string().into());
		assert!(
			!resolved.contains_key(&ella_key),
			"ella should have no membership when all her events are rejected; got {:?}",
			resolved.get(&ella_key)
		);
	}

	#[tokio::test]
	async fn join_rule_with_auth_chain() {
		let join_rule = JOIN_RULE();

		let edges = vec![vec!["END", "JR", "START"], vec!["END", "IMZ", "START"]]
			.into_iter()
			.map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
			.collect::<Vec<_>>();

		let expected_state_ids = vec!["JR"].into_iter().map(event_id).collect::<Vec<_>>();

		do_check(&join_rule.values().cloned().collect::<Vec<_>>(), edges, expected_state_ids)
			.await;
	}

	/// Regression test for the v2.1 conflicted subgraph bug.
	/// MSC4297 mandates traversing prev_events (DAG timeline), not auth_events,
	/// when computing the conflicted state subgraph. Using auth_events produced
	/// an incorrect subgraph which caused state resolution to output garbage.
	///
	/// This test runs the same ban-vs-join scenario through v2.1 (room version
	/// > V11) and verifies the ban wins, proving the subgraph is correctly
	/// built from the DAG timeline rather than the auth chain.
	#[tokio::test]
	async fn v2_1_conflicted_subgraph_uses_prev_events() {
		use futures::future::ready;

		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();
		let mut inner = init;
		inner.extend(ban);

		// Build conflicted state: MB (ban) vs IME (join) for ella
		let ella_key = (StateEventType::RoomMember, ella().to_string().into());
		let conflicted: StateMap<Vec<OwnedEventId>> =
			[(ella_key, vec![event_id("MB"), event_id("IME")])]
				.into_iter()
				.collect();

		let ev_map = &inner;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());

		let subgraph = super::calculate_conflicted_subgraph(&conflicted, &fetcher)
			.await
			.expect("subgraph calculation must succeed");

		// MB has prev_events = ["START"], IME has prev_events = ["IMC"]
		assert!(subgraph.0.contains(&event_id("MB")), "must contain MB");
		assert!(subgraph.0.contains(&event_id("IME")), "must contain IME");

		// IPOWER is only reachable via auth_events, never via prev_events.
		// If present, we are crawling auth_events (the old bug).
		assert!(
			!subgraph.0.contains(&event_id("IPOWER")),
			"must NOT contain IPOWER (auth chain only, not prev_events)"
		);
	}

	/// Regression test: non-state events (e.g. m.room.message) that appear in
	/// auth chains or subgraph traversals must be filtered out of the
	/// conflicted set before iterative_auth_check, which requires all events
	/// to have a state_key.
	///
	/// Without the filter, this crashes with:
	///   InvalidPdu("State event had no state key")
	#[tokio::test]
	async fn non_state_events_in_auth_chain_dont_crash_resolution() {
		use std::collections::HashSet;

		use futures::future::ready;

		let init = INITIAL_EVENTS();
		let mut ev_map: HashMap<OwnedEventId, PduEvent> = init.clone();

		// Insert a non-state event (m.room.message, no state_key) that will
		// appear in the auth chain. In real federation, auth chains can
		// contain non-state events due to DAG traversal.
		let msg = to_pdu_event(
			"MSG1",
			alice(),
			TimelineEventType::RoomMessage,
			None, // <-- no state_key, this is NOT a state event
			to_raw_json_value(&json!({ "body": "hello", "msgtype": "m.text" })).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["START"],
		);
		ev_map.insert(msg.event_id.clone(), msg);

		// Create two conflicting topic events
		let t1 = to_pdu_event(
			"T1",
			alice(),
			TimelineEventType::RoomTopic,
			Some(""),
			to_raw_json_value(&json!({ "topic": "topic A" })).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["START"],
		);
		let t2 = to_pdu_event(
			"T2",
			alice(),
			TimelineEventType::RoomTopic,
			Some(""),
			to_raw_json_value(&json!({ "topic": "topic B" })).unwrap(),
			&["CREATE", "IMA", "IPOWER"],
			&["START"],
		);
		ev_map.insert(t1.event_id.clone(), t1);
		ev_map.insert(t2.event_id.clone(), t2);

		let topic_key = StateEventType::RoomTopic.with_state_key("");

		// State set 1: topic = T1
		let mut state1: StateMap<OwnedEventId> = HashMap::new();
		for ev in init.values().filter(|e| e.state_key().is_some()) {
			state1.insert(
				ev.event_type().with_state_key(ev.state_key().unwrap()),
				ev.event_id().to_owned(),
			);
		}
		state1.insert(topic_key.clone(), event_id("T1"));

		// State set 2: topic = T2
		let mut state2 = state1.clone();
		state2.insert(topic_key.clone(), event_id("T2"));

		let state_sets = vec![state1, state2];

		// Auth chain includes the non-state event MSG1 — this is the
		// scenario that triggered the crash.
		let auth_chain: HashSet<OwnedEventId> = ev_map.keys().cloned().collect();
		let auth_chain_sets = vec![auth_chain.clone(), auth_chain];

		let fetch = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.contains_key(&id));

		// This must not panic with "State event had no state key"
		let result = super::resolve(
			&RoomVersionId::V6,
			state_sets.iter(),
			&auth_chain_sets,
			&fetch,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await;

		assert!(
			result.is_ok(),
			"resolve() must not crash when non-state events are in the auth chain: {:?}",
			result.err()
		);

		// The resolved state must contain a topic event (T1 or T2)
		let resolved = result.unwrap();
		assert!(resolved.contains_key(&topic_key), "resolved state must contain the topic key");
	}

	#[allow(non_snake_case)]
	fn BAN_STATE_SET() -> HashMap<OwnedEventId, PduEvent> {
		vec![
			to_pdu_event(
				"PA",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
				&["CREATE", "IMA", "IPOWER"], // auth_events
				&["START"],                   // prev_events
			),
			to_pdu_event(
				"PB",
				alice(),
				TimelineEventType::RoomPowerLevels,
				Some(""),
				to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
				&["CREATE", "IMA", "IPOWER"],
				&["END"],
			),
			to_pdu_event(
				"MB",
				alice(),
				TimelineEventType::RoomMember,
				Some(ella().as_str()),
				member_content_ban(),
				&["CREATE", "IMA", "PB"],
				&["PA"],
			),
			to_pdu_event(
				"IME",
				ella(),
				TimelineEventType::RoomMember,
				Some(ella().as_str()),
				member_content_join(),
				&["CREATE", "IJR", "PA"],
				&["MB"],
			),
		]
		.into_iter()
		.map(|ev| (ev.event_id.clone(), ev))
		.collect()
	}

	#[allow(non_snake_case)]
	fn JOIN_RULE() -> HashMap<OwnedEventId, PduEvent> {
		vec![
			to_pdu_event(
				"JR",
				alice(),
				TimelineEventType::RoomJoinRules,
				Some(""),
				to_raw_json_value(&json!({ "join_rule": "invite" })).unwrap(),
				&["CREATE", "IMA", "IPOWER"],
				&["START"],
			),
			to_pdu_event(
				"IMZ",
				zara(),
				TimelineEventType::RoomPowerLevels,
				Some(zara().as_str()),
				member_content_join(),
				&["CREATE", "JR", "IPOWER"],
				&["START"],
			),
		]
		.into_iter()
		.map(|ev| (ev.event_id.clone(), ev))
		.collect()
	}

	macro_rules! state_set {
        ($($kind:expr_2021 => $key:expr_2021 => $id:expr_2021),* $(,)?) => {{
            #[allow(unused_mut)]
            let mut x = StateMap::new();
            $(
                x.insert(($kind, $key.into()), $id);
            )*
            x
        }};
    }

	#[test]
	fn separate_unique_conflicted() {
		let (unconflicted, conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@b:hs1" => 1],
				state_set![StateEventType::RoomMember => "@c:hs1" => 2],
			]
			.iter(),
		);

		assert_eq!(unconflicted, StateMap::new());
		assert_eq!(conflicted, state_set![
			StateEventType::RoomMember => "@a:hs1" => vec![0],
			StateEventType::RoomMember => "@b:hs1" => vec![1],
			StateEventType::RoomMember => "@c:hs1" => vec![2],
		],);
	}

	#[test]
	fn separate_conflicted() {
		let (unconflicted, mut conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@a:hs1" => 1],
				state_set![StateEventType::RoomMember => "@a:hs1" => 2],
			]
			.iter(),
		);

		// HashMap iteration order is random, so sort this before asserting on it
		for v in conflicted.values_mut() {
			v.sort_unstable();
		}

		assert_eq!(unconflicted, StateMap::new());
		assert_eq!(conflicted, state_set![
			StateEventType::RoomMember => "@a:hs1" => vec![0, 1, 2],
		],);
	}

	#[test]
	fn separate_unconflicted() {
		let (unconflicted, conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
			]
			.iter(),
		);

		assert_eq!(unconflicted, state_set![
			StateEventType::RoomMember => "@a:hs1" => 0,
		],);
		assert_eq!(conflicted, StateMap::new());
	}

	#[test]
	fn separate_mixed() {
		let (unconflicted, conflicted) = super::separate(
			[
				state_set![StateEventType::RoomMember => "@a:hs1" => 0],
				state_set![
					StateEventType::RoomMember => "@a:hs1" => 0,
					StateEventType::RoomMember => "@b:hs1" => 1,
				],
				state_set![
					StateEventType::RoomMember => "@a:hs1" => 0,
					StateEventType::RoomMember => "@c:hs1" => 2,
				],
			]
			.iter(),
		);

		assert_eq!(unconflicted, state_set![
			StateEventType::RoomMember => "@a:hs1" => 0,
		],);
		assert_eq!(conflicted, state_set![
			StateEventType::RoomMember => "@b:hs1" => vec![1],
			StateEventType::RoomMember => "@c:hs1" => vec![2],
		],);
	}

	/// Validates that the `is_ascii_graphic` check correctly filters room IDs.
	/// This is a regression test for the zero-copy stream UAF that produced
	/// corrupted room IDs like `!D0yPVK3zb8Y4svzltl:nutra.tked\nGg▒[\x7f]`.
	/// Verify that if a power level event is rejected, it is excluded from
	/// the resolved state even when both forks contain it.
	#[tokio::test]
	async fn rejected_power_level_excluded_from_state() {
		use futures::future::ready;

		let _ = tracing::subscriber::set_default(
			tracing_subscriber::fmt().with_test_writer().finish(),
		);

		let init = INITIAL_EVENTS();
		let ban = BAN_STATE_SET();
		let mut inner = init.clone();
		inner.extend(ban);
		let mut store = TestStore(inner.clone());

		// State set A: has IPOWER + PA
		let state_set_a = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("PA")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		// State set B: has PB (conflicts on power_levels with PA)
		let state_set_b = [
			inner.get(&event_id("CREATE")).unwrap(),
			inner.get(&event_id("IJR")).unwrap(),
			inner.get(&event_id("IMA")).unwrap(),
			inner.get(&event_id("IMB")).unwrap(),
			inner.get(&event_id("IMC")).unwrap(),
			inner.get(&event_id("IME")).unwrap(),
			inner.get(&event_id("PB")).unwrap(),
		]
		.iter()
		.map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
		.collect::<StateMap<_>>();

		// Mark PA as rejected via the Pdu field — only the unconflicted IPOWER should
		// remain
		store.0.get_mut(&event_id("PA")).unwrap().rejected = true;
		let ev_map = &store.0;
		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(room_id(), map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		let resolved = super::resolve(
			&RoomVersionId::V6,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.unwrap();

		let pl_key = (StateEventType::RoomPowerLevels, "".into());
		// PA was rejected, so it must not appear in resolved state
		assert!(
			resolved.get(&pl_key) != Some(&event_id("PA")),
			"PA was rejected and must not appear in resolved state; got {:?}",
			resolved.get(&pl_key)
		);
	}

	mod room_id_validation {
		/// Simulates the validation logic from `monitor.rs::check_room`
		fn is_valid_room_id(s: &str) -> bool {
			s.bytes().all(|b| b.is_ascii_graphic()) && <&ruma::RoomId>::try_from(s).is_ok()
		}

		#[test]
		fn valid_standard_room_id() {
			assert!(is_valid_room_id("!abc123:matrix.org"));
		}

		#[test]
		fn valid_v4_opaque_room_id() {
			// v4+ room IDs have no server_name, just an opaque hash
			assert!(is_valid_room_id("!c10y-fNiMx5ijtgGFibzPUfNs9hpQvnJYPTV-fD2KPk"));
		}

		#[test]
		fn reject_newline_injection() {
			// The nutra.tked UAF scenario: buffer overlap produces \n in the ID
			assert!(!is_valid_room_id("!abc123:nutra.tked\nGg"));
		}

		#[test]
		fn reject_del_byte() {
			assert!(!is_valid_room_id("!abc123:server\x7f.org"));
		}

		#[test]
		fn reject_escape_sequence() {
			assert!(!is_valid_room_id("!abc123:server\x1b[0m.org"));
		}

		#[test]
		fn reject_null_byte() {
			assert!(!is_valid_room_id("!abc123:server\0.org"));
		}

		#[test]
		fn reject_space() {
			assert!(!is_valid_room_id("!abc123:server .org"));
		}

		#[test]
		fn reject_tab() {
			assert!(!is_valid_room_id("!abc123:server\t.org"));
		}

		#[test]
		fn reject_empty() {
			assert!(!is_valid_room_id(""));
		}
	}

	/// Ported from Synapse
	/// tests/state/test_v21.py::test_state_reset_replay_conflicted_subgraph
	///
	/// Tests that when an event cites OLD auth events but indirectly references
	/// NEW ones, the v2.1 subgraph traversal correctly replays events in the
	/// right power-level epoch, preventing state resets.
	///
	/// DAG:
	///   create -> alice_join -> power1 -> join_rules -> bob_join, charlie_join
	///   power1 -> power2 (Alice promotes Bob)
	///   power2 -> power3 (Bob promotes Charlie)
	///   power3 -> eve_join1
	///   eve_join1 -> eve_join2 (cites OLD power1 — DODGY)
	///   power3 -> zara_join
	#[tokio::test]
	async fn synapse_v21_state_reset_replay_conflicted_subgraph() {
		use futures::future::ready;
		use ruma::{EventId, OwnedEventId, OwnedRoomId};

		use super::test_utils::*;

		// V12 derives create event ID from room ID: !X -> $X
		// Must be 43 url-safe base64 chars for Ruma to parse as V4+ Room ID
		let v12_room_id: OwnedRoomId = "!S21Create123456789012345678901234567890123"
			.try_into()
			.unwrap();
		let create_id_str = "$S21Create123456789012345678901234567890123";
		let create_id: OwnedEventId = create_id_str.try_into().unwrap();

		let mut e1_create = to_pdu_event::<&str>(
			create_id_str,
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice(), "room_version": "12" })).unwrap(),
			&[],
			&[],
		);
		e1_create.room_id = None; // V12: create event has no room_id

		let e2_ma = to_pdu_event(
			"S21_MA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&[], // V12: no create event in auth_events
			&[create_id_str],
		);

		let e3_power1 = to_pdu_event(
			"S21_PL1",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": {} })).unwrap(),
			&["S21_MA"],
			&["S21_MA"],
		);

		let e4_jr = to_pdu_event(
			"S21_JR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&["S21_MA", "S21_PL1"],
			&["S21_PL1"],
		);

		let e5_mb = to_pdu_event(
			"S21_MB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&["S21_PL1", "S21_JR"],
			&["S21_JR"],
		);

		let e6_mc = to_pdu_event(
			"S21_MC",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&["S21_PL1", "S21_JR"],
			&["S21_JR"],
		);

		let e7_power2 = to_pdu_event(
			"S21_PL2",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { bob(): 50 } })).unwrap(),
			&["S21_MA", "S21_PL1"],
			&["S21_PL1"],
		);

		let e8_power3 = to_pdu_event(
			"S21_PL3",
			bob(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { bob(): 50, charlie(): 50 } })).unwrap(),
			&["S21_MB", "S21_PL2"],
			&["S21_PL2"],
		);

		let e9_me1 = to_pdu_event(
			"S21_ME1",
			ella(),
			TimelineEventType::RoomMember,
			Some(ella().as_str()),
			member_content_join(),
			&["S21_PL3", "S21_JR"],
			&["S21_PL3"],
		);

		let e10_me2 = to_pdu_event(
			"S21_ME2",
			ella(),
			TimelineEventType::RoomMember,
			Some(ella().as_str()),
			member_content_join(),
			&["S21_PL1", "S21_JR", "S21_ME1"],
			&["S21_ME1"],
		);

		let e11_mz = to_pdu_event(
			"S21_MZ",
			zara(),
			TimelineEventType::RoomMember,
			Some(zara().as_str()),
			member_content_join(),
			&["S21_PL3", "S21_JR"],
			&["S21_PL3"],
		);

		let all_events = vec![
			&e1_create, &e2_ma, &e3_power1, &e4_jr, &e5_mb, &e6_mc, &e7_power2, &e8_power3,
			&e9_me1, &e10_me2, &e11_mz,
		];

		let store = TestStore(
			all_events
				.iter()
				.map(|ev| {
					let mut ev = (*ev).clone();
					if ev.event_id != create_id {
						ev.room_id = Some(v12_room_id.clone());
					}
					(ev.event_id.clone(), ev)
				})
				.collect(),
		);

		let dodgy_state: StateMap<OwnedEventId> =
			[&e1_create, &e2_ma, &e5_mb, &e6_mc, &e10_me2, &e3_power1, &e4_jr]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		let sensible_state: StateMap<OwnedEventId> =
			[&e1_create, &e2_ma, &e5_mb, &e6_mc, &e11_mz, &e8_power3, &e4_jr]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		let state_sets = [dodgy_state, sensible_state];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(&v12_room_id, map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let ev_map = &store.0;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		// Dispatch normally through V12 (no test-hack needed)
		let resolved = super::resolve(
			&RoomVersionId::V12,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.expect("v2.1 resolution should succeed");

		let pl_key = (StateEventType::RoomPowerLevels, "".into());
		assert_eq!(
			resolved.get(&pl_key),
			Some(&event_id("S21_PL3")),
			"v2.1 must pick newer power levels PL3, not dodgy PL1; got {:?}",
			resolved.get(&pl_key)
		);

		let ella_key = (StateEventType::RoomMember, ella().as_str().into());
		assert!(
			resolved.contains_key(&ella_key),
			"Ella/Eve membership must be present in resolved state"
		);
	}

	/// Ported from Synapse
	/// tests/state/test_v21.py::test_state_reset_start_empty_set
	///
	/// DAG:
	///   create -> alice_join -> power -> join_rules_public -> bob_join
	///   power -> join_rules_invite
	///   join_rules_invite -> alice_leave
	#[tokio::test]
	async fn synapse_v21_state_reset_start_empty_set() {
		use futures::future::ready;
		use ruma::{EventId, OwnedEventId, OwnedRoomId};

		use super::test_utils::*;

		let v12_room_id: OwnedRoomId = "!S21bCreate12345678901234567890123456789012"
			.try_into()
			.unwrap();
		let create_id_str = "$S21bCreate12345678901234567890123456789012";
		let create_id: OwnedEventId = create_id_str.try_into().unwrap();

		let mut e1_create = to_pdu_event::<&str>(
			create_id_str,
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice(), "room_version": "12" })).unwrap(),
			&[],
			&[],
		);
		e1_create.room_id = None;

		let e2_ma1 = to_pdu_event(
			"S21B_MA1",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&[],
			&[create_id_str],
		);

		// Alice makes Bob an admin
		let e3_power = to_pdu_event(
			"S21B_PL",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { bob(): 100 } })).unwrap(),
			&["S21B_MA1"],
			&["S21B_MA1"],
		);

		// Public join rules
		let e4_jr1 = to_pdu_event(
			"S21B_JR1",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&["S21B_MA1", "S21B_PL"],
			&["S21B_PL"],
		);

		// Bob joins
		let e5_mb = to_pdu_event(
			"S21B_MB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&["S21B_PL", "S21B_JR1"],
			&["S21B_JR1"],
		);

		// Alice sets join rules to invite
		let e6_jr2 = to_pdu_event(
			"S21B_JR2",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Invite)).unwrap(),
			&["S21B_MA1", "S21B_PL"],
			&["S21B_PL"],
		);

		// Alice leaves
		let e7_ma2 = to_pdu_event(
			"S21B_MA2",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_leave(),
			&["S21B_PL", "S21B_MA1"],
			&["S21B_MA1"],
		);

		let all_events = vec![&e1_create, &e2_ma1, &e3_power, &e4_jr1, &e5_mb, &e6_jr2, &e7_ma2];
		let store = TestStore(
			all_events
				.iter()
				.map(|ev| {
					let mut ev = (*ev).clone();
					if ev.event_id != create_id {
						ev.room_id = Some(v12_room_id.clone());
					}
					(ev.event_id.clone(), ev)
				})
				.collect(),
		);

		let correct_state: StateMap<OwnedEventId> =
			[&e1_create, &e7_ma2, &e5_mb, &e3_power, &e6_jr2]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		let incorrect_state: StateMap<OwnedEventId> =
			[&e1_create, &e7_ma2, &e5_mb, &e3_power, &e4_jr1]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		let state_sets = [correct_state, incorrect_state];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(&v12_room_id, map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let ev_map = &store.0;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		let resolved = super::resolve(
			&RoomVersionId::V12,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.expect("v2.1 resolution should succeed");

		let jr_key = (StateEventType::RoomJoinRules, "".into());
		assert_eq!(
			resolved.get(&jr_key),
			Some(&event_id("S21B_JR2")),
			"v2.1 must pick newer invite-only join rules JR2; got {:?}",
			resolved.get(&jr_key)
		);

		// Bob must survive in the resolved state. Without the V2.1
		// supplemental merge fix, resolved_state accumulates join_rules=invite
		// from the control pass and overwrites bob's own auth chain (which had
		// join_rules=public when he joined). This causes bob_join to fail auth
		// with "not invited to invite-only room", dropping him from state.
		let bob_key = (StateEventType::RoomMember, bob().to_string().into());
		assert_eq!(
			resolved.get(&bob_key),
			Some(&event_id("S21B_MB")),
			"v2.1 supplemental merge must not clobber bob's auth chain; bob_join should \
			 survive. If this fails, iterative_auth_check is overriding event auth_events with \
			 resolved_state (the V2 behavior) instead of using the event's own auth chain (V2.1 \
			 behavior). Got {:?}",
			resolved.get(&bob_key)
		);
	}

	/// Ported from Complement
	/// TestMSC4297StateResolutionV2_1_includes_conflicted_subgraph
	///
	/// DAG:
	///   create -> alice_join -> power1 -> join_rules -> bob_join
	///                                                -> charlie_join
	///                                 -> power2(bob:50)
	///                                 -> power3(bob:50,charlie:50) ->
	/// zara_join   power1 -> ella_join  (dodgy: cites old PL in auth)
	///
	/// Two state forks: dodgy (with ella, PL1) vs correct (with zara, PL3).
	/// Resolution must pick PL3 (bob:50, charlie:50), not regress to PL1.
	#[tokio::test]
	async fn synapse_v21_conflicted_subgraph_preserves_power_levels() {
		use futures::future::ready;
		use ruma::{EventId, OwnedEventId, OwnedRoomId};

		use super::test_utils::*;

		let v12_room_id: OwnedRoomId = "!S21cCreate12345678901234567890123456789012"
			.try_into()
			.unwrap();
		let create_id_str = "$S21cCreate12345678901234567890123456789012";
		let create_id: OwnedEventId = create_id_str.try_into().unwrap();

		let mut e1_create = to_pdu_event::<&str>(
			create_id_str,
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice(), "room_version": "12" })).unwrap(),
			&[],
			&[],
		);
		e1_create.room_id = None;

		// Alice joins
		let e2_ma = to_pdu_event(
			"S21C_MA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&[],
			&[create_id_str],
		);

		// Initial power levels (alice is creator, implicit PL 100 in V12)
		let e3_power1 = to_pdu_event(
			"S21C_PL1",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": {} })).unwrap(),
			&["S21C_MA"],
			&["S21C_MA"],
		);

		// Join rules = public
		let e4_jr = to_pdu_event(
			"S21C_JR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&["S21C_MA", "S21C_PL1"],
			&["S21C_PL1"],
		);

		// Bob joins
		let e5_mb = to_pdu_event(
			"S21C_MB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&["S21C_PL1", "S21C_JR"],
			&["S21C_JR"],
		);

		// Charlie joins
		let e6_mc = to_pdu_event(
			"S21C_MC",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&["S21C_PL1", "S21C_JR"],
			&["S21C_MB"],
		);

		// Alice promotes Bob to PL 50
		let e7_power2 = to_pdu_event(
			"S21C_PL2",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { bob(): 50 } })).unwrap(),
			&["S21C_MA", "S21C_PL1"],
			&["S21C_MC"],
		);

		// Bob promotes Charlie to PL 50
		let e8_power3 = to_pdu_event(
			"S21C_PL3",
			bob(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { bob(): 50, charlie(): 50 } })).unwrap(),
			&["S21C_MB", "S21C_PL2"],
			&["S21C_PL2"],
		);

		// Zara joins citing PL3 (correct)
		let e9_mz = to_pdu_event(
			"S21C_MZ",
			zara(),
			TimelineEventType::RoomMember,
			Some(zara().as_str()),
			member_content_join(),
			&["S21C_PL3", "S21C_JR"],
			&["S21C_PL3"],
		);

		// Ella joins citing PL1 (DODGY — old power levels)
		let e10_me = to_pdu_event(
			"S21C_ME",
			ella(),
			TimelineEventType::RoomMember,
			Some(ella().as_str()),
			member_content_join(),
			&["S21C_PL1", "S21C_JR"],
			&["S21C_MZ"],
		);

		let all_events = vec![
			&e1_create, &e2_ma, &e3_power1, &e4_jr, &e5_mb, &e6_mc, &e7_power2, &e8_power3,
			&e9_mz, &e10_me,
		];
		let store = TestStore(
			all_events
				.iter()
				.map(|ev| {
					let mut ev = (*ev).clone();
					if ev.event_id != create_id {
						ev.room_id = Some(v12_room_id.clone());
					}
					(ev.event_id.clone(), ev)
				})
				.collect(),
		);

		// Dodgy state fork: has ella with old PL1
		let dodgy_state: StateMap<OwnedEventId> =
			[&e1_create, &e2_ma, &e5_mb, &e6_mc, &e10_me, &e3_power1, &e4_jr]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		// Correct state fork: has zara with PL3
		let correct_state: StateMap<OwnedEventId> =
			[&e1_create, &e2_ma, &e5_mb, &e6_mc, &e9_mz, &e8_power3, &e4_jr]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		let state_sets = [dodgy_state, correct_state];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(&v12_room_id, map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let ev_map = &store.0;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		let resolved = super::resolve(
			&RoomVersionId::V12,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.expect("v2.1 resolution should succeed");

		// PL3 must win over PL1 — resolution must pick the latest power levels
		let pl_key = (StateEventType::RoomPowerLevels, "".into());
		assert_eq!(
			resolved.get(&pl_key),
			Some(&event_id("S21C_PL3")),
			"v2.1 must pick PL3 (bob:50, charlie:50) over PL1 (empty users); got {:?}",
			resolved.get(&pl_key)
		);

		// Both zara and ella must be present in resolved state
		let zara_key = (StateEventType::RoomMember, zara().to_string().into());
		assert_eq!(
			resolved.get(&zara_key),
			Some(&event_id("S21C_MZ")),
			"zara must be in resolved state; got {:?}",
			resolved.get(&zara_key)
		);

		let ella_key = (StateEventType::RoomMember, ella().to_string().into());
		assert_eq!(
			resolved.get(&ella_key),
			Some(&event_id("S21C_ME")),
			"ella must be in resolved state; got {:?}",
			resolved.get(&ella_key)
		);
	}

	/// Regression test for the Complement
	/// TestMSC4297StateResolutionV2_1_includes_conflicted_subgraph failure.
	///
	/// Root cause: In V12 rooms, check_power_levels rejected PL events that
	/// included the room creator in content.users with a non-Int::MAX value
	/// (e.g. {alice: 100}). During V2.1 state resolution, ALL events go
	/// through iterative_auth_check, and the creator's PL entry caused the
	/// entire PL event to be rejected — dropping Alice's power level and
	/// making subsequent events (like promoting Bob) return 403 Forbidden.
	///
	/// This test verifies that a PL event with the creator in content.users
	/// survives V2.1 state resolution.
	///
	/// DAG:
	///   create -> alice_join -> PL1(users:{}) -> PL2(users:{alice:100})
	///   PL2 must survive resolution, not be rejected.
	#[tokio::test]
	async fn v12_pl_with_creator_in_users_survives_resolution() {
		use futures::future::ready;
		use ruma::{EventId, OwnedEventId, OwnedRoomId};

		use super::test_utils::*;

		let v12_room_id: OwnedRoomId = "!S21dCreate12345678901234567890123456789012"
			.try_into()
			.unwrap();
		let create_id_str = "$S21dCreate12345678901234567890123456789012";
		let create_id: OwnedEventId = create_id_str.try_into().unwrap();

		let mut e1_create = to_pdu_event::<&str>(
			create_id_str,
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice(), "room_version": "12" })).unwrap(),
			&[],
			&[],
		);
		e1_create.room_id = None;

		// Alice joins
		let e2_ma = to_pdu_event(
			"S21D_MA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&[],
			&[create_id_str],
		);

		// PL1: default power levels (creator omitted from users, as V12 requires)
		let e3_pl1 = to_pdu_event(
			"S21D_PL1",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": {} })).unwrap(),
			&["S21D_MA"],
			&["S21D_MA"],
		);

		// PL2: creator sends PL with herself in content.users at 100.
		// This is what the Complement test does and what federation
		// partners may send. Must NOT be rejected by check_power_levels.
		let e4_pl2 = to_pdu_event(
			"S21D_PL2",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { alice(): 100 } })).unwrap(),
			&["S21D_MA", "S21D_PL1"],
			&["S21D_PL1"],
		);

		// Join rules = public
		let e5_jr = to_pdu_event(
			"S21D_JR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&["S21D_MA", "S21D_PL2"],
			&["S21D_PL2"],
		);

		// Bob joins
		let e6_mb = to_pdu_event(
			"S21D_MB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&["S21D_PL2", "S21D_JR"],
			&["S21D_JR"],
		);

		let all_events = vec![&e1_create, &e2_ma, &e3_pl1, &e4_pl2, &e5_jr, &e6_mb];
		let store = TestStore(
			all_events
				.iter()
				.map(|ev| {
					let mut ev = (*ev).clone();
					if ev.event_id != create_id {
						ev.room_id = Some(v12_room_id.clone());
					}
					(ev.event_id.clone(), ev)
				})
				.collect(),
		);

		// Two identical state forks (simulating federation join where both
		// sides agree on state — the resolution should be a no-op)
		let state_set_a: StateMap<OwnedEventId> = [&e1_create, &e2_ma, &e4_pl2, &e5_jr, &e6_mb]
			.iter()
			.map(|ev| {
				(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
			})
			.collect();

		// Second fork: same state but without bob (as if remote server
		// hasn't seen bob yet). This forces PL2 through iterative_auth_check.
		let state_set_b: StateMap<OwnedEventId> = [&e1_create, &e2_ma, &e4_pl2, &e5_jr]
			.iter()
			.map(|ev| {
				(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
			})
			.collect();

		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(&v12_room_id, map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let ev_map = &store.0;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		let resolved = super::resolve(
			&RoomVersionId::V12,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.expect("v2.1 resolution should succeed");

		// PL2 (with alice:100 in content.users) must survive resolution.
		// Before the fix, check_power_levels rejected PL2 because the
		// creator appeared in content.users with a non-Int::MAX value,
		// causing it to be dropped from resolved state.
		let pl_key = (StateEventType::RoomPowerLevels, "".into());
		assert_eq!(
			resolved.get(&pl_key),
			Some(&event_id("S21D_PL2")),
			"PL2 (with creator in content.users) must survive V2.1 resolution; got {:?}. If \
			 this fails, check_power_levels is rejecting PL events that include the room \
			 creator in content.users — V12 creators have implicit Int::MAX power, so their \
			 presence in content.users should be a no-op, not a rejection.",
			resolved.get(&pl_key)
		);

		// Bob must also survive
		let bob_key = (StateEventType::RoomMember, bob().to_string().into());
		assert_eq!(
			resolved.get(&bob_key),
			Some(&event_id("S21D_MB")),
			"bob must be in resolved state; got {:?}",
			resolved.get(&bob_key)
		);
	}

	#[tokio::test]
	async fn v12_missing_create_event_does_not_panic() {
		use futures::future::ready;
		use ruma::{EventId, OwnedEventId, OwnedRoomId};

		use super::test_utils::*;

		let v12_room_id: OwnedRoomId = "!MissingCreate12345678901234567890123456789"
			.try_into()
			.unwrap();

		let mut e1_ma = to_pdu_event::<&str>(
			"S21_MA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&[],
			&[],
		);
		e1_ma.room_id = Some(v12_room_id.clone());

		let all_events = vec![&e1_ma];
		let store = TestStore(
			all_events
				.iter()
				.map(|ev| (ev.event_id.clone(), (*ev).clone()))
				.collect(),
		);

		let state_set_a: StateMap<OwnedEventId> = [(&e1_ma)]
			.iter()
			.map(|ev| {
				(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
			})
			.collect();

		let state_set_b: StateMap<OwnedEventId> = HashMap::new();

		let state_sets = [state_set_a, state_set_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(&v12_room_id, map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let ev_map = &store.0;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		let resolved = super::resolve(
			&RoomVersionId::V12,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await;

		assert!(resolved.is_ok());
	}

	/// Mirrors complement
	/// TestMSC4297StateResolutionV2_1_includes_conflicted_subgraph
	///
	/// Tests that a V12 room creator can update power levels AFTER other users
	/// have joined. The complement test creates a V12 room, sets power levels,
	/// joins bob+charlie, then sets power levels again. The second PL update
	/// was returning 403 because iterative_auth_check couldn't verify the
	/// creator's authority.
	///
	/// This test exercises the auth chain through iterative_auth_check directly
	/// to ensure the create event is found via the room_id->event_id derivation
	/// and the creator retains power level authority.
	#[tokio::test]
	async fn v12_power_levels_update_after_joins() {
		use futures::future::ready;
		use ruma::{EventId, OwnedEventId, OwnedRoomId};

		use super::test_utils::*;

		let v12_room_id: OwnedRoomId = "!V12PLAfterJoin234567890123456789012345678901"
			.try_into()
			.unwrap();
		let create_id_str = "$V12PLAfterJoin234567890123456789012345678901";
		let create_id: OwnedEventId = create_id_str.try_into().unwrap();

		// 1. Create event
		let mut e1_create = to_pdu_event::<&str>(
			create_id_str,
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice(), "room_version": "12" })).unwrap(),
			&[],
			&[],
		);
		e1_create.room_id = None; // V12: no room_id on create

		// 2. Creator joins
		let e2_ma = to_pdu_event(
			"PLAJ_MA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&[], // V12: no create in auth_events
			&[create_id_str],
		);

		// 3. First power levels (creator sets PL)
		let e3_pl1 = to_pdu_event(
			"PLAJ_PL1",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({
				"users": {},
				"users_default": 0
			}))
			.unwrap(),
			&["PLAJ_MA"],
			&["PLAJ_MA"],
		);

		// 4. Join rules (public)
		let e4_jr = to_pdu_event(
			"PLAJ_JR",
			alice(),
			TimelineEventType::RoomJoinRules,
			Some(""),
			to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Public)).unwrap(),
			&["PLAJ_MA", "PLAJ_PL1"],
			&["PLAJ_PL1"],
		);

		// 5. Bob joins
		let e5_mb = to_pdu_event(
			"PLAJ_MB",
			bob(),
			TimelineEventType::RoomMember,
			Some(bob().as_str()),
			member_content_join(),
			&["PLAJ_PL1", "PLAJ_JR"],
			&["PLAJ_JR"],
		);

		// 6. Charlie joins
		let e6_mc = to_pdu_event(
			"PLAJ_MC",
			charlie(),
			TimelineEventType::RoomMember,
			Some(charlie().as_str()),
			member_content_join(),
			&["PLAJ_PL1", "PLAJ_JR"],
			&["PLAJ_JR"],
		);

		// 7. Creator updates power levels AFTER joins (this is the one that was 403ing)
		let e7_pl2 = to_pdu_event(
			"PLAJ_PL2",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({
				"users": { bob().as_str(): 50 },
				"users_default": 0
			}))
			.unwrap(),
			&["PLAJ_MA", "PLAJ_PL1"],
			&["PLAJ_MC"],
		);

		let all_events = vec![&e1_create, &e2_ma, &e3_pl1, &e4_jr, &e5_mb, &e6_mc, &e7_pl2];

		let store = TestStore(
			all_events
				.iter()
				.map(|ev| {
					let mut ev = (*ev).clone();
					if ev.event_id != create_id {
						ev.room_id = Some(v12_room_id.clone());
					}
					(ev.event_id.clone(), ev)
				})
				.collect(),
		);

		// State before the PL2 event: create, alice join, PL1, JR, bob, charlie
		let state_a: StateMap<OwnedEventId> =
			[&e1_create, &e2_ma, &e3_pl1, &e4_jr, &e5_mb, &e6_mc]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		// State including the PL2 update
		let state_b: StateMap<OwnedEventId> =
			[&e1_create, &e2_ma, &e7_pl2, &e4_jr, &e5_mb, &e6_mc]
				.iter()
				.map(|ev| {
					(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
				})
				.collect();

		let state_sets = [state_a, state_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(&v12_room_id, map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let ev_map = &store.0;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		let resolved = super::resolve(
			&RoomVersionId::V12,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.expect("V12 power levels update after joins must succeed");

		let pl_key = (StateEventType::RoomPowerLevels, "".into());
		assert_eq!(
			resolved.get(&pl_key),
			Some(&event_id("PLAJ_PL2")),
			"V12 must accept the creator's PL update after joins; got {:?}",
			resolved.get(&pl_key)
		);
	}

	/// Tests that V12 iterative_auth_check correctly derives the create event
	/// from the room ID when processing power events. This catches the
	/// regression where the create event cache fails to find the create event
	/// because room_id_or_hash() returns None.
	#[tokio::test]
	async fn v12_iterative_auth_check_finds_create_event() {
		use futures::future::ready;
		use ruma::{EventId, OwnedEventId, OwnedRoomId};

		use super::test_utils::*;

		let v12_room_id: OwnedRoomId = "!V12AuthCreate2345678901234567890123456789012"
			.try_into()
			.unwrap();
		let create_id_str = "$V12AuthCreate2345678901234567890123456789012";
		let create_id: OwnedEventId = create_id_str.try_into().unwrap();

		let mut e1_create = to_pdu_event::<&str>(
			create_id_str,
			alice(),
			TimelineEventType::RoomCreate,
			Some(""),
			to_raw_json_value(&json!({ "creator": alice(), "room_version": "12" })).unwrap(),
			&[],
			&[],
		);
		e1_create.room_id = None;

		let e2_ma = to_pdu_event(
			"IAC_MA",
			alice(),
			TimelineEventType::RoomMember,
			Some(alice().as_str()),
			member_content_join(),
			&[],
			&[create_id_str],
		);

		// Power levels event — this is the one that needs create event lookup
		let e3_pl = to_pdu_event(
			"IAC_PL",
			alice(),
			TimelineEventType::RoomPowerLevels,
			Some(""),
			to_raw_json_value(&json!({ "users": { alice().as_str(): 100 } })).unwrap(),
			&["IAC_MA"],
			&["IAC_MA"],
		);

		let all_events = vec![&e1_create, &e2_ma, &e3_pl];
		let store = TestStore(
			all_events
				.iter()
				.map(|ev| {
					let mut ev = (*ev).clone();
					if ev.event_id != create_id {
						ev.room_id = Some(v12_room_id.clone());
					}
					(ev.event_id.clone(), ev)
				})
				.collect(),
		);

		let state_a: StateMap<OwnedEventId> = [&e1_create, &e2_ma]
			.iter()
			.map(|ev| {
				(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
			})
			.collect();

		let state_b: StateMap<OwnedEventId> = [&e1_create, &e2_ma, &e3_pl]
			.iter()
			.map(|ev| {
				(ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone())
			})
			.collect();

		let state_sets = [state_a, state_b];
		let auth_chain: Vec<_> = state_sets
			.iter()
			.map(|map| {
				store
					.auth_event_ids(&v12_room_id, map.values().cloned().collect())
					.unwrap()
			})
			.collect();

		let ev_map = &store.0;
		let fetcher = |id: OwnedEventId| ready(ev_map.get(&id).cloned());
		let exists = |id: OwnedEventId| ready(ev_map.get(&id).is_some());

		let resolved = super::resolve(
			&RoomVersionId::V12,
			&state_sets,
			&auth_chain,
			&fetcher,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.expect("V12 iterative_auth_check must find create event via room ID derivation");

		let pl_key = (StateEventType::RoomPowerLevels, "".into());
		assert!(resolved.contains_key(&pl_key), "Power levels must be in resolved state");
	}
}
