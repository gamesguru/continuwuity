use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	sync::Arc,
};

use conduwuit::{
	Error, Result, err, implement, info,
	state_res::StateMap,
	trace,
	utils::stream::{IterStream, ReadyExt, WidebandExt},
	warn,
};
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{OwnedEventId, RoomId, RoomVersionId};

use crate::rooms::state_compressor::CompressedState;

/// Pre-loaded event cache to avoid per-event RocksDB lookups during
/// state resolution. Populated once at the start of bulk operations
/// like rebuild_state.
pub(crate) type PduCache =
	Arc<tokio::sync::RwLock<HashMap<OwnedEventId, Arc<conduwuit_core::PduEvent>>>>;

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

	let fork_states = fork_states.await?;

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
	info!(%room_id, n_fork_states, "state_res: fork states loaded, starting resolution");
	let t = std::time::Instant::now();
	let state = self
		.state_resolution(room_id, room_version_id, fork_states.iter(), None)
		.boxed()
		.await?;
	info!(%room_id, n_resolved = state.len(), elapsed = ?t.elapsed(), "state_res: resolution complete");

	// Diagnostic: log resolved PL and JoinRules
	for ((ty, sk), eid) in &state {
		if ty.to_string() == "m.room.power_levels" || ty.to_string() == "m.room.join_rules" {
			info!("resolve_state RESULT ({ty},{sk}) => {eid}");
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
#[tracing::instrument(name = "rezzy", level = "debug", skip_all, fields(%room_id))]
pub async fn state_resolution<'a, StateSets>(
	&'a self,
	room_id: &RoomId,
	room_version: &'a RoomVersionId,
	state_sets: StateSets,
	prefetch_cache: Option<PduCache>,
) -> Result<StateMap<OwnedEventId>>
where
	StateSets: Iterator<Item = &'a StateMap<OwnedEventId>> + Clone + Send,
{
	use std::collections::BTreeMap;

	let state_sets_vec: Vec<&StateMap<OwnedEventId>> = state_sets.collect();
	let num_maps = state_sets_vec.len();

	if num_maps == 0 {
		return Ok(StateMap::new());
	}
	if num_maps == 1 {
		return Ok(state_sets_vec[0].clone());
	}

	// Pre-separate unconflicted/conflicted keys
	let mut counts: HashMap<(String, String, String), usize> = HashMap::new();
	let mut key_to_ids: HashMap<(String, String), HashSet<String>> = HashMap::new();

	for map in &state_sets_vec {
		for ((ty, sk), id) in *map {
			let ty_s = ty.to_string();
			let sk_s = sk.to_string();
			let id_s = id.to_string();
			let entry = counts
				.entry((ty_s.clone(), sk_s.clone(), id_s.clone()))
				.or_insert(0);
			*entry = entry.saturating_add(1);
			key_to_ids.entry((ty_s, sk_s)).or_default().insert(id_s);
		}
	}

	let mut unconflicted: BTreeMap<(String, Option<String>), String> = BTreeMap::new();
	let mut conflicted_keys: HashSet<(String, String)> = HashSet::new();

	for (key, ids) in &key_to_ids {
		if ids.len() == 1 {
			let id = ids.iter().next().unwrap();
			let count = counts
				.get(&(key.0.clone(), key.1.clone(), id.clone()))
				.copied()
				.unwrap_or(0);
			if count == num_maps {
				let state_key_opt = if key.1.is_empty() { None } else { Some(key.1.clone()) };
				unconflicted.insert((key.0.clone(), state_key_opt), id.clone());
				continue;
			}
		}
		conflicted_keys.insert(key.clone());
	}

	// Collect all conflicted event IDs (state map differences)
	let mut conflicted_eids: HashSet<OwnedEventId> = HashSet::new();
	for map in &state_sets_vec {
		for ((ty, sk), id) in *map {
			if conflicted_keys.contains(&(ty.to_string(), sk.to_string())) {
				conflicted_eids.insert(id.clone());
			}
		}
	}

	// Early exit: no conflicts
	if conflicted_eids.is_empty() {
		return Ok(state_sets_vec[0].clone());
	}

	// Map room version early — needed to decide auth chain diff vs subgraph
	let version = match room_version.as_str() {
		| "1" => rezzy::StateResVersion::V1,
		| "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "10" | "11" =>
			rezzy::StateResVersion::V2,
		| "12" => rezzy::StateResVersion::V2_1,
		| _ => rezzy::StateResVersion::V2_1_1,
	};
	let is_v2_1_plus = matches!(
		version,
		rezzy::StateResVersion::V2_1
			| rezzy::StateResVersion::V2_1_1
			| rezzy::StateResVersion::V2_2
	);

	// Fetch auth chains for all state sets. Needed both for:
	// - V2: auth chain diff (adds diff events to conflicted set)
	// - V2_1+: subgraph computation & auth_context for rezzy
	let mut per_set_chains: Vec<HashSet<OwnedEventId>> = Vec::with_capacity(num_maps);
	for map in &state_sets_vec {
		let state_eids: Vec<OwnedEventId> = map.values().cloned().collect();
		let chain: HashSet<OwnedEventId> = self
			.services
			.auth_chain
			.event_ids_iter(room_id, state_eids.iter().map(|id| &**id))
			.try_collect()
			.await
			.unwrap_or_default();
		per_set_chains.push(chain);
	}

	let mut union_auth: HashSet<OwnedEventId> = HashSet::new();
	let mut intersect_auth: HashSet<OwnedEventId> = per_set_chains[0].clone();
	for chain in &per_set_chains {
		union_auth.extend(chain.iter().cloned());
		intersect_auth.retain(|eid| chain.contains(eid));
	}

	// V2 only: auth chain diff events are also conflicted.
	// V2_1+ (MSC4297): uses conflicted state subgraph instead — computed below
	// after we have the full LeanEvent map.
	if !is_v2_1_plus {
		for eid in &union_auth {
			if !intersect_auth.contains(eid) {
				conflicted_eids.insert(eid.clone());
			}
		}
	}

	// Collect all event IDs we need to fetch
	let mut fetch_ids: HashSet<OwnedEventId> = union_auth;
	fetch_ids.extend(conflicted_eids.iter().cloned());
	for map in &state_sets_vec {
		fetch_ids.extend(map.values().cloned());
	}

	// Fetch PDUs (honor prefetch cache)
	let meta = &self.services.pdu_metadata;
	let pdu_map: HashMap<OwnedEventId, conduwuit_core::PduEvent> =
		if let Some(cache) = prefetch_cache {
			let cached = cache.read().await;
			let mut map: HashMap<OwnedEventId, conduwuit_core::PduEvent> = cached
				.iter()
				.filter(|(eid, _)| fetch_ids.contains(*eid))
				.map(|(eid, pdu)| (eid.clone(), (**pdu).clone()))
				.collect();

			// Fetch any missing from DB
			let missing: Vec<OwnedEventId> = fetch_ids
				.iter()
				.filter(|eid| !map.contains_key(*eid))
				.cloned()
				.collect();
			drop(cached);

			if !missing.is_empty() {
				let fetched: Vec<conduwuit_core::PduEvent> = self
					.services
					.timeline
					.multi_get_pdus(Some(room_id), futures::stream::iter(missing))
					.filter_map(|r| async move { r.ok() })
					.collect()
					.await;
				for mut pdu in fetched {
					// Clear stale rejection flags for timeline events
					if meta.is_event_rejected(&pdu.event_id).await
						&& self.services.timeline.pdu_exists(&pdu.event_id).await
					{
						warn!(
							event_id = %pdu.event_id,
							"state_res: clearing stale rejection flag on timeline event"
						);
						meta.unmark_event_rejected(&pdu.event_id);
						pdu.rejected = false;
					}
					map.insert(pdu.event_id.clone(), pdu);
				}
			}
			map
		} else {
			// Build fresh cache
			self.services
				.timeline
				.multi_get_pdus(Some(room_id), fetch_ids.into_iter().stream())
				.filter_map(|r| async move { r.ok() })
				.then(|mut pdu| async move {
					let is_rejected = meta.is_event_rejected(&pdu.event_id).await;
					if is_rejected && self.services.timeline.pdu_exists(&pdu.event_id).await {
						warn!(
							event_id = %pdu.event_id,
							"state_res: clearing stale rejection flag on timeline event"
						);
						meta.unmark_event_rejected(&pdu.event_id);
						pdu.rejected = false;
					}
					(pdu.event_id.clone(), pdu)
				})
				.collect()
				.await
		};

	// Convert PduEvent → LeanEvent
	let to_lean = |pdu: &conduwuit_core::PduEvent| -> rezzy::LeanEvent {
		let content_val: serde_json::Value =
			serde_json::from_str(pdu.content.get()).unwrap_or(serde_json::Value::Null);
		let power_level = content_val
			.get("power_level")
			.and_then(|pl| {
				pl.as_i64()
					.or_else(|| pl.as_str().and_then(|s| s.parse().ok()))
			})
			.unwrap_or(0);
		rezzy::LeanEvent {
			event_id: pdu.event_id.to_string(),
			event_type: pdu.kind.to_string(),
			state_key: pdu.state_key.as_ref().map(ToString::to_string),
			power_level,
			origin_server_ts: pdu.origin_server_ts.into(),
			sender: pdu.sender.to_string(),
			content: content_val,
			prev_events: pdu.prev_events.iter().map(ToString::to_string).collect(),
			auth_events: pdu.auth_events.iter().map(ToString::to_string).collect(),
			depth: u64::from(pdu.depth),
		}
	};

	// Build the full background context ONCE
	let mut auth_context: HashMap<String, rezzy::LeanEvent> = pdu_map
		.iter()
		.map(|(eid, pdu)| (eid.to_string(), to_lean(pdu)))
		.collect();

	// Extract the exact conflicted_events map
	let conflicted_events: HashMap<String, rezzy::LeanEvent> = if is_v2_1_plus {
		// MSC4297 (V2.1+): rezzy computes the exact HashMap we need
		let direct_conflicted: Vec<String> =
			conflicted_eids.iter().map(ToString::to_string).collect();
		let v2_1_conflicted_subgraph =
			rezzy::compute_v2_1_conflicted_subgraph(&auth_context, &direct_conflicted);

		// Remove conflicted events from auth_context (mutually exclusive)
		for id in v2_1_conflicted_subgraph.keys() {
			auth_context.remove(id);
		}

		v2_1_conflicted_subgraph
	} else {
		// V1 or V2: pull known conflicted_eids (state diff + auth chain diff) out
		let mut v2_conflicted_auth_context = HashMap::with_capacity(conflicted_eids.len());
		for eid in &conflicted_eids {
			let id_str = eid.to_string();
			if let Some(lean) = auth_context.remove(&id_str) {
				v2_conflicted_auth_context.insert(id_str, lean);
			}
		}
		v2_conflicted_auth_context
	};

	// Call rezzy (sync -- no async overhead)
	let resolved_lean =
		rezzy::resolve_lean(unconflicted, conflicted_events, &auth_context, version);

	// Convert back to Ruma StateMap
	let mut resolved = StateMap::new();
	for ((ty_str, sk_opt), eid_str) in resolved_lean {
		let ty: ruma::events::StateEventType = ty_str.into();
		let sk: conduwuit_core::matrix::StateKey = sk_opt.unwrap_or_default().into();
		if let Ok(eid) = OwnedEventId::try_from(eid_str.as_str()) {
			resolved.insert((ty, sk), eid);
		}
	}

	Ok(resolved)
}
