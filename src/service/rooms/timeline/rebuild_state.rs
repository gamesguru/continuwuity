use std::{
	collections::{BTreeSet, HashMap, HashSet},
	hash::{Hash, Hasher},
	sync::Arc,
	time::{Duration, Instant},
};

use conduwuit_core::{
	PduEvent, Result, debug, info,
	matrix::{
		event::Event,
		state_res::{self, StateMap},
	},
	warn,
};
use futures::StreamExt;
use ruma::{OwnedEventId, RoomId, RoomVersionId, events::TimelineEventType};

use crate::rooms;

/// Event metadata extracted during Phase 1 streaming.
type EventMeta = (OwnedEventId, Vec<OwnedEventId>, Option<String>, u64);

/// Safe u32 -> usize for Vec indexing of roaring bitmap indices.
#[inline]
fn to_usize(v: u32) -> usize { usize::try_from(v).expect("u32 fits in usize") }

/// Shared context threaded through all phases of rebuild_state.
struct RebuildCtx {
	room_version: RoomVersionId,
	events_meta: Vec<EventMeta>,
	event_cache: HashMap<OwnedEventId, Arc<PduEvent>>,
	event_set: HashSet<OwnedEventId>,
	eid_to_idx: HashMap<OwnedEventId, u32>,
	idx_to_eid: Vec<OwnedEventId>,
	auth_chain_bitmaps: Vec<roaring::RoaringBitmap>,
}

/// A lightweight state group: parent reference + optional single delta entry.
/// Full state maps are reconstructed lazily by walking the chain.
struct StateGroup {
	parent: Option<usize>,
	/// The state delta applied on top of parent (None = fork-resolved full
	/// state).
	delta: Option<(ruma::events::StateEventType, String, OwnedEventId)>,
	/// If this group was created by fork resolution, store the full resolved
	/// state. Otherwise None (reconstruct by walking parent chain).
	full_state: Option<StateMap<OwnedEventId>>,
}

/// Reconstruct the full state map for a group by walking the delta chain.
fn materialize_state(groups: &[StateGroup], group_id: usize) -> StateMap<OwnedEventId> {
	// If this group has a cached full state, use it
	if let Some(ref full) = groups[group_id].full_state {
		return full.clone();
	}

	// Walk the chain to collect deltas
	let mut deltas: Vec<(ruma::events::StateEventType, String, OwnedEventId)> = Vec::new();
	let mut current = group_id;
	loop {
		let group = &groups[current];
		if let Some(ref full) = group.full_state {
			// Found a full-state anchor, apply deltas on top
			let mut state = full.clone();
			for (ty, sk, eid) in deltas.into_iter().rev() {
				state.insert((ty, sk.as_str().into()), eid);
			}
			return state;
		}
		if let Some(ref delta) = group.delta {
			deltas.push(delta.clone());
		}
		match group.parent {
			| Some(p) => current = p,
			| None => break,
		}
	}

	// Reached root (empty state), apply all deltas
	let mut state = StateMap::new();
	for (ty, sk, eid) in deltas.into_iter().rev() {
		state.insert((ty, sk.as_str().into()), eid);
	}
	state
}

impl super::Service {
	/// Rebuilds room state entirely in-memory like ruma-lean, then batch-writes
	/// the result to DB. This avoids per-event RocksDB round-trips during state
	/// resolution, achieving seconds instead of minutes for large DAGs.
	#[tracing::instrument(skip(self), level = "info")]
	pub async fn rebuild_state(&self, room_id: &RoomId) -> Result<()> {
		let original_room_shortstatehash = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.ok();

		// Phase 1: Stream events and collect metadata
		let (events_meta, room_version) = self.rebuild_stream_events(room_id).await;

		// Phase 2: Pre-load ALL events into RAM
		let event_cache = self.rebuild_prefetch_events(room_id, &events_meta).await;
		let event_set: HashSet<OwnedEventId> =
			events_meta.iter().map(|(eid, ..)| eid.clone()).collect();

		// Phase 2b: Pre-compute auth chains bottom-up
		let (eid_to_idx, idx_to_eid, auth_chain_bitmaps) =
			Self::rebuild_auth_chains(&events_meta, &event_cache);

		let ctx = RebuildCtx {
			room_version,
			events_meta,
			event_cache,
			event_set,
			eid_to_idx,
			idx_to_eid,
			auth_chain_bitmaps,
		};

		// Phase 3: In-memory state walk with fork resolution
		let (state_groups, event_group) = Self::rebuild_walk_state(&ctx).await;

		// Phase 4: Batch-write to DB
		let (group_to_ssh, current_shortstatehash) = self
			.rebuild_batch_write(
				room_id,
				&ctx.events_meta,
				&state_groups,
				&event_group,
				&ctx.event_cache,
			)
			.await?;

		// Phase 5: Final multi-head extremity merge
		let current_shortstatehash = self
			.rebuild_merge_extremities(
				room_id,
				&ctx.events_meta,
				&ctx.event_set,
				&event_group,
				&group_to_ssh,
				current_shortstatehash,
			)
			.await?;

		// Phase 6: Apply final state
		let (total_added, total_removed) = self
			.services
			.state_compressor
			.diff_full_state(original_room_shortstatehash.unwrap_or(0), current_shortstatehash)
			.await;

		let state_lock = self.services.state.mutex.lock(room_id).await;
		self.services
			.state
			.force_state_quiet(
				room_id,
				current_shortstatehash,
				total_added,
				total_removed,
				&state_lock,
			)
			.await?;

		Ok(())
	}

	// ── Phase 1: Stream events and collect metadata ──
	async fn rebuild_stream_events(&self, room_id: &RoomId) -> (Vec<EventMeta>, RoomVersionId) {
		info!("rebuild_state: streaming events in topological order...");
		let start = Instant::now();

		let mut events_meta: Vec<EventMeta> = Vec::new();
		let mut room_version = RoomVersionId::V1;
		let mut room_version_found = false;

		let mut stream = std::pin::pin!(self.topo_pdus(room_id, None));
		while let Some(Ok((_pdu_count, pdu))) = stream.next().await {
			let eid = pdu.event_id().to_owned();
			let prev: Vec<OwnedEventId> = pdu.prev_events().map(ToOwned::to_owned).collect();
			let state_key = pdu.state_key().map(ToOwned::to_owned);
			let depth = u64::from(pdu.depth());

			// Timeline events are authoritative; clear any stale rejection
			// flags that would otherwise poison the state resolution below.
			// Soft-fail flags are intentional and must persist.
			self.services.pdu_metadata.unmark_event_rejected(&eid);

			if !room_version_found && *pdu.kind() == TimelineEventType::RoomCreate {
				if let Ok(create_content) = serde_json::from_str::<
					ruma::events::room::create::RoomCreateEventContent,
				>(pdu.content().get())
				{
					room_version = create_content.room_version;
					room_version_found = true;
				}
			}

			events_meta.push((eid, prev, state_key, depth));
		}

		debug!(
			"rebuild_state: loaded {} event metadata in {:?}",
			events_meta.len(),
			start.elapsed(),
		);
		(events_meta, room_version)
	}

	// ── Phase 2: Pre-load ALL events into RAM ──
	async fn rebuild_prefetch_events(
		&self,
		room_id: &RoomId,
		events_meta: &[EventMeta],
	) -> HashMap<OwnedEventId, Arc<PduEvent>> {
		let start = Instant::now();
		let event_cache: HashMap<OwnedEventId, Arc<PduEvent>> = {
			let event_ids_stream =
				futures::stream::iter(events_meta.iter().map(|(eid, ..)| eid.clone()));
			self.multi_get_pdus(Some(room_id), event_ids_stream)
				.filter_map(|r| async move { r.ok() })
				.map(|mut pdu| {
					pdu.rejected = false;
					(pdu.event_id.clone(), Arc::new(pdu))
				})
				.collect()
				.await
		};
		info!(
			"rebuild_state: pre-loaded {} events into RAM in {:?}",
			event_cache.len(),
			start.elapsed(),
		);
		event_cache
	}

	// ── Phase 2b: Pre-compute auth chains bottom-up ──
	// Since events are in topo order, each event's auth chain parents are
	// already computed. Chains are stored as roaring bitmaps indexed by event
	// position for compact O(1) union operations.

	fn rebuild_auth_chains(
		events_meta: &[EventMeta],
		event_cache: &HashMap<OwnedEventId, Arc<PduEvent>>,
	) -> (HashMap<OwnedEventId, u32>, Vec<OwnedEventId>, Vec<roaring::RoaringBitmap>) {
		let start = Instant::now();

		let eid_to_idx: HashMap<OwnedEventId, u32> = events_meta
			.iter()
			.enumerate()
			.map(|(i, (eid, ..))| {
				(eid.clone(), u32::try_from(i).expect("room has > 2^32 (4B) events"))
			})
			.collect();
		let idx_to_eid: Vec<OwnedEventId> =
			events_meta.iter().map(|(eid, ..)| eid.clone()).collect();

		let mut bitmaps: Vec<roaring::RoaringBitmap> = Vec::with_capacity(events_meta.len());
		for (eid, ..) in events_meta {
			let mut chain = roaring::RoaringBitmap::new();
			if let Some(pdu) = event_cache.get(eid) {
				for auth_id in &pdu.auth_events {
					if let Some(&i) = eid_to_idx.get(auth_id) {
						chain.insert(i);
						// Only include transitive chain if bitmap already computed
						// (auth event appears before us in topo order). In busted DAGs,
						// auth events may appear later — we still record the direct ref.
						let idx = to_usize(i);
						if idx < bitmaps.len() {
							chain |= &bitmaps[idx];
						}
					}
				}
			}
			bitmaps.push(chain);
		}

		debug!(
			"rebuild_state: pre-computed {} auth chains in {:?}",
			bitmaps.len(),
			start.elapsed(),
		);
		(eid_to_idx, idx_to_eid, bitmaps)
	}

	// ── Phase 3: In-memory state walk with fork resolution ──
	// Uses a delta-chain representation to avoid cloning full state maps:
	// - state_groups: Vec of (parent_group_id, Option<delta>)
	// - event_group: maps event_id → group_id
	// Full state maps are only materialized on-demand for fork resolution.
	// This reduces memory from O(state_events × state_size) to O(state_events).

	async fn rebuild_walk_state(
		ctx: &RebuildCtx,
	) -> (Vec<StateGroup>, HashMap<OwnedEventId, usize>) {
		let start = Instant::now();
		let mut state_groups: Vec<StateGroup> = Vec::new();
		let mut event_group: HashMap<OwnedEventId, usize> = HashMap::new();
		let mut fork_resolve_count = 0_usize;
		let mut cumulative_resolve_time = Duration::ZERO;
		let mut processed = 0_usize;
		let total_events = ctx.events_meta.len();

		// Group 0 = empty state (for events with no parents)
		state_groups.push(StateGroup {
			parent: None,
			delta: None,
			full_state: Some(StateMap::new()),
		});
		let empty_group: usize = 0;

		for (eid, prev_events, state_key, _depth) in &ctx.events_meta {
			processed = processed.saturating_add(1);

			if processed.is_multiple_of(1000) {
				debug!(
					"rebuild_state: {}/{} events | {} groups | {} forks ({:?}) | elapsed: {:?}",
					processed,
					total_events,
					state_groups.len(),
					fork_resolve_count,
					cumulative_resolve_time,
					start.elapsed(),
				);
			}

			// Collect parent groups (deduplicated)
			let parent_groups: Vec<usize> = prev_events
				.iter()
				.filter(|p| ctx.event_set.contains(*p))
				.filter_map(|p| event_group.get(p).copied())
				.collect::<HashSet<usize>>()
				.into_iter()
				.collect();

			let state_before_group = match parent_groups.len() {
				| 0 => empty_group,
				| 1 => parent_groups[0],
				| _ => {
					let fork_start = Instant::now();
					// Materialize full state for each parent
					let fork_states: Vec<StateMap<OwnedEventId>> = parent_groups
						.iter()
						.map(|&g| materialize_state(&state_groups, g))
						.collect();
					let fork_state_refs: Vec<&StateMap<OwnedEventId>> =
						fork_states.iter().collect();

					let resolved = Self::resolve_fork_with_states(ctx, &fork_state_refs).await;

					let fork_elapsed = fork_start.elapsed();
					fork_resolve_count = fork_resolve_count.saturating_add(1);
					cumulative_resolve_time =
						cumulative_resolve_time.saturating_add(fork_elapsed);

					if fork_elapsed.as_millis() > 50 {
						debug!(
							"rebuild_state: SLOW fork #{} for {} ({} parents) took {:?}",
							fork_resolve_count,
							eid,
							parent_groups.len(),
							fork_elapsed,
						);
					}

					match resolved {
						| Ok(resolved_state) => {
							let gid = state_groups.len();
							state_groups.push(StateGroup {
								parent: None,
								delta: None,
								full_state: Some(resolved_state),
							});
							gid
						},
						| Err(e) => {
							warn!(
								"rebuild_state: fork resolution failed for {}: {} — using first \
								 parent",
								eid, e,
							);
							parent_groups[0]
						},
					}
				},
			};

			// Apply state event delta (lightweight: just store the delta, no clone)
			let group_after = if let Some(sk) = state_key {
				let Some(pdu) = ctx.event_cache.get(eid) else {
					warn!("rebuild_state: state event {eid} missing from cache — skipping");
					event_group.insert(eid.clone(), state_before_group);
					continue;
				};
				let event_type: ruma::events::StateEventType = pdu.kind.to_string().into();

				let gid = state_groups.len();
				state_groups.push(StateGroup {
					parent: Some(state_before_group),
					delta: Some((event_type, sk.clone(), eid.clone())),
					full_state: None,
				});
				gid
			} else {
				state_before_group
			};

			event_group.insert(eid.clone(), group_after);

			if processed.is_multiple_of(5000) {
				tokio::task::yield_now().await;
			}
		}

		info!(
			"rebuild_state: in-memory walk done in {:?} | {} events, {} state groups, {} forks \
			 ({:?})",
			start.elapsed(),
			processed,
			state_groups.len(),
			fork_resolve_count,
			cumulative_resolve_time,
		);

		(state_groups, event_group)
	}

	/// Resolve a fork between multiple parent state sets using state_res.
	/// Uses pruned event cache (only auth-reachable events) and pre-computed
	/// roaring bitmap auth chains for O(1) lookups.
	async fn resolve_fork_with_states(
		ctx: &RebuildCtx,
		fork_states: &[&StateMap<OwnedEventId>],
	) -> Result<StateMap<OwnedEventId>> {
		// Quick check: identify conflicted event IDs for event cache pruning
		let mut all_keys: HashMap<(&ruma::events::StateEventType, &str), HashSet<&OwnedEventId>> =
			HashMap::new();
		for state in fork_states {
			for ((ty, sk), eid) in *state {
				all_keys.entry((ty, sk.as_ref())).or_default().insert(eid);
			}
		}
		let conflicted_eids: HashSet<OwnedEventId> = all_keys
			.values()
			.filter(|eids| eids.len() > 1)
			.flatten()
			.cloned()
			.cloned()
			.collect();

		// If no conflicts at all, return the first fork state (they're identical)
		if conflicted_eids.is_empty() {
			return Ok(fork_states[0].clone());
		}

		// Build pruned event cache: only events transitively reachable from
		// conflicted events via auth_events (ruma-lean-style subgraph pruning).
		// This is the main optimization — reducing the resolver's event lookups
		// from 15k+ to ~50 events.
		let pruned_cache: HashMap<OwnedEventId, Arc<PduEvent>> = {
			let mut reachable = HashSet::new();
			let mut stack: Vec<OwnedEventId> = conflicted_eids.into_iter().collect();
			while let Some(id) = stack.pop() {
				if reachable.insert(id.clone()) {
					if let Some(pdu) = ctx.event_cache.get(&id) {
						for auth_id in &pdu.auth_events {
							if ctx.event_set.contains(auth_id) {
								stack.push(auth_id.clone());
							}
						}
					}
				}
			}
			// Include all events referenced by fork states (for auth context)
			for state in fork_states {
				for eid in state.values() {
					reachable.insert(eid.clone());
				}
			}
			reachable
				.iter()
				.filter_map(|id| ctx.event_cache.get(id).map(|pdu| (id.clone(), pdu.clone())))
				.collect()
		};

		let event_fetch = |id: OwnedEventId| {
			let pdu = pruned_cache.get(&id).cloned();
			async move { pdu }
		};

		let event_batch_fetch = |ids: Vec<OwnedEventId>| {
			let results: Vec<Arc<PduEvent>> = ids
				.iter()
				.filter_map(|id| pruned_cache.get(id).cloned())
				.collect();
			async move { results }
		};

		// Pre-computed auth chain lookup via roaring bitmaps
		let auth_chain_fetch = |events: Vec<OwnedEventId>| {
			let mut combined = roaring::RoaringBitmap::new();
			for id in &events {
				if let Some(&i) = ctx.eid_to_idx.get(id) {
					combined.insert(i);
					combined |= &ctx.auth_chain_bitmaps[to_usize(i)];
				}
			}
			let chain: HashSet<OwnedEventId> = combined
				.iter()
				.map(|i| ctx.idx_to_eid[to_usize(i)].clone())
				.collect();
			async move { chain }
		};

		state_res::resolve(
			&ctx.room_version,
			fork_states.iter().copied(),
			&event_fetch,
			Some(&event_batch_fetch),
			&auth_chain_fetch,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await
		.map_err(|e| conduwuit_core::err!(error!("state_res::resolve failed: {e}")))
	}

	// ── Phase 4: Batch-write to DB ──
	// Pre-caches all short IDs, compresses unique state groups with
	// content-hash deduplication, and writes pdu_shortstatehash for each event.
	async fn rebuild_batch_write(
		&self,
		room_id: &RoomId,
		events_meta: &[EventMeta],
		state_groups: &[StateGroup],
		event_group: &HashMap<OwnedEventId, usize>,
		event_cache: &HashMap<OwnedEventId, Arc<PduEvent>>,
	) -> Result<(HashMap<usize, u64>, u64)> {
		let write_start = Instant::now();
		let mut cork = Some(self.db.db.cork());

		// 4a: Pre-cache ALL short IDs across all groups to avoid serial DB lookups.
		// Collect unique (type, state_key) pairs and event_ids, resolve once.
		let precache_start = Instant::now();
		let mut unique_state_keys: HashSet<(String, String)> = HashSet::new();
		let mut unique_event_ids: HashSet<OwnedEventId> = HashSet::new();
		let referenced_groups: HashSet<usize> = event_group.values().copied().collect();
		for &gid in &referenced_groups {
			let state = materialize_state(state_groups, gid);
			for ((ty, sk), event_id) in &state {
				unique_state_keys.insert((ty.to_string(), sk.to_string()));
				unique_event_ids.insert(event_id.clone());
			}
		}
		// Also collect all event IDs from events_meta for pdu_shortstatehash writes
		for (eid, ..) in events_meta {
			unique_event_ids.insert(eid.clone());
		}

		// Resolve shortstatekeys
		let mut ssk_cache: HashMap<(String, String), u64> =
			HashMap::with_capacity(unique_state_keys.len());
		for (ty, sk) in &unique_state_keys {
			let ssk = self
				.services
				.short
				.get_or_create_shortstatekey(&ty.as_str().into(), sk)
				.await;
			ssk_cache.insert((ty.clone(), sk.clone()), ssk);
		}

		// Resolve shorteventids
		let mut sei_cache: HashMap<OwnedEventId, u64> =
			HashMap::with_capacity(unique_event_ids.len());
		for eid in &unique_event_ids {
			let sei = self.services.short.get_or_create_shorteventid(eid).await;
			sei_cache.insert(eid.clone(), sei);
		}
		debug!(
			"rebuild_state: pre-cached {} shortstatekeys + {} shorteventids in {:?}",
			ssk_cache.len(),
			sei_cache.len(),
			precache_start.elapsed(),
		);

		// 4b: Compress unique groups with content-hash deduplication.
		let empty_group: usize = 0;
		let mut group_to_ssh: HashMap<usize, u64> = HashMap::new();
		let empty_ssh = self
			.services
			.state_compressor
			.save_state(room_id, Arc::new(BTreeSet::new()))
			.await?
			.shortstatehash;
		group_to_ssh.insert(empty_group, empty_ssh);

		// content_hash -> ssh for deduplication across different group_ids
		let mut content_to_ssh: HashMap<u64, u64> = HashMap::new();
		let mut current_shortstatehash = empty_ssh;
		let mut groups_compressed = 0_usize;
		let mut groups_deduped = 0_usize;

		// Pre-compute the shortroomid once
		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;

		for (eid, _prev, state_key, _depth) in events_meta {
			let Some(&group_id) = event_group.get(eid) else {
				continue;
			};

			// Lazily compress state groups as we encounter them
			let ssh = if let Some(&cached_ssh) = group_to_ssh.get(&group_id) {
				cached_ssh
			} else {
				// Compress this state group using pre-cached short IDs
				let state_map = materialize_state(state_groups, group_id);
				let mut compressed = BTreeSet::new();
				for ((ty, sk), event_id) in &state_map {
					let ssk = ssk_cache
						.get(&(ty.to_string(), sk.to_string()))
						.copied()
						.unwrap_or(0);
					let sei = sei_cache.get(event_id).copied().unwrap_or(0);
					compressed.insert(rooms::state_compressor::compress_state_event(ssk, sei));
				}

				// Content-hash dedup: identical compressed states get the same SSH.
				// NOTE: uses SipHash-1-3 (DefaultHasher) — fast and collision-resistant
				// enough for ephemeral in-memory dedup. Not persisted. The state
				// compressor itself uses SHA-256 for durable shortstatehash keys.
				let mut hasher = std::collections::hash_map::DefaultHasher::new();
				for entry in &compressed {
					entry.hash(&mut hasher);
				}
				let content_hash = hasher.finish();

				let ssh = if let Some(&existing_ssh) = content_to_ssh.get(&content_hash) {
					groups_deduped = groups_deduped.saturating_add(1);
					existing_ssh
				} else {
					let result = self
						.services
						.state_compressor
						.save_state(room_id, Arc::new(compressed))
						.await?;
					let ssh = result.shortstatehash;
					content_to_ssh.insert(content_hash, ssh);
					groups_compressed = groups_compressed.saturating_add(1);
					ssh
				};
				group_to_ssh.insert(group_id, ssh);
				ssh
			};

			// Write pdu_shortstatehash for this event
			if state_key.is_some() {
				// State event: compute_state_for_event equivalent
				let (_, mut json) = self.db.get_from_eventid_pdu(eid).await?;
				let pdu_id: conduwuit_core::matrix::pdu::RawPduId =
					conduwuit_core::matrix::pdu::PduId {
						shortroomid,
						shorteventid: conduwuit_core::matrix::pdu::PduCount::Normal(0),
					}
					.into();
				if let Some(pdu) = event_cache.get(eid) {
					let mut ssh_mut = ssh;
					self.compute_state_for_event(pdu, eid, &mut json, &mut ssh_mut, &pdu_id)
						.await;
				}
			} else {
				// Non-state event: just set pdu_shortstatehash
				let shorteventid = sei_cache.get(eid).copied().unwrap_or(0);
				self.services
					.state
					.set_pdu_shortstatehash(shorteventid, ssh);
			}

			current_shortstatehash = ssh;

			if groups_compressed.is_multiple_of(100) && groups_compressed > 0 {
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
			}
		}

		drop(cork.take());

		info!(
			"rebuild_state: batch-write done in {:?} | {} groups compressed, {} deduped",
			write_start.elapsed(),
			groups_compressed,
			groups_deduped,
		);

		Ok((group_to_ssh, current_shortstatehash))
	}

	// ── Phase 5: Final multi-head extremity merge ──
	// Handles rooms with multiple forward extremities by merging their state.

	async fn rebuild_merge_extremities(
		&self,
		room_id: &RoomId,
		events_meta: &[EventMeta],
		event_set: &HashSet<OwnedEventId>,
		event_group: &HashMap<OwnedEventId, usize>,
		group_to_ssh: &HashMap<usize, u64>,
		current_shortstatehash: u64,
	) -> Result<u64> {
		let mut has_children: HashSet<&OwnedEventId> = HashSet::new();
		for (_, prev_events, ..) in events_meta {
			for parent in prev_events {
				if event_set.contains(parent) {
					has_children.insert(parent);
				}
			}
		}

		let extremity_sshs: Vec<u64> = events_meta
			.iter()
			.map(|(eid, ..)| eid)
			.filter(|eid| !has_children.contains(eid))
			.filter_map(|eid| {
				let group = event_group.get(eid)?;
				group_to_ssh.get(group).copied()
			})
			.collect::<HashSet<_>>()
			.into_iter()
			.collect();

		let num_extremities = events_meta
			.iter()
			.map(|(eid, ..)| eid)
			.filter(|eid| !has_children.contains(eid))
			.count();

		if extremity_sshs.len() <= 1 {
			debug!(
				"rebuild_state: all forward extremities share a single SSH — no multi-head \
				 merge needed",
			);
			return Ok(current_shortstatehash);
		}

		debug!(
			"rebuild_state: {} forward extremities with {} unique SSHs — merging disconnected \
			 components...",
			num_extremities,
			extremity_sshs.len(),
		);

		// Load full compressed state for each unique SSH
		let mut all_compressed = BTreeSet::new();
		for &ssh in &extremity_sshs {
			if let Some(full_state) = self.services.state_compressor.get_full_state(ssh).await {
				for entry in full_state.as_ref() {
					all_compressed.insert(*entry);
				}
			}
		}

		// Build ssk -> set of shorteventid values to detect conflicts
		let mut ssk_values: HashMap<u64, HashSet<u64>> = HashMap::new();
		for bytes in &all_compressed {
			let (ssk, sei) = rooms::state_compressor::parse_compressed_state_event(*bytes);
			ssk_values.entry(ssk).or_default().insert(sei);
		}

		let conflicting: Vec<_> = ssk_values
			.iter()
			.filter(|(_, values)| values.len() > 1)
			.map(|(ssk, _)| *ssk)
			.collect();

		if conflicting.is_empty() {
			// No conflicts — trivial union merge
			debug!(
				"rebuild_state: trivial merge of {} state entries from {} components",
				ssk_values.len(),
				extremity_sshs.len(),
			);
			let merged_ssh = self
				.services
				.state_compressor
				.save_state(room_id, Arc::new(all_compressed))
				.await?
				.shortstatehash;
			return Ok(merged_ssh);
		}

		// Conflicting keys exist — pick winners by depth
		debug!(
			"rebuild_state: {} conflicting keys across {} components — resolving...",
			conflicting.len(),
			extremity_sshs.len(),
		);

		// Build ShortEventId -> depth map only for conflicting SEIs
		// using pre-computed depth from events_meta
		let depth_by_eid: HashMap<&OwnedEventId, u64> = events_meta
			.iter()
			.map(|(eid, _, _, depth)| (eid, *depth))
			.collect();
		let mut sei_depth: HashMap<u64, u64> = HashMap::new();
		let conflicting_seis: HashSet<u64> = ssk_values
			.iter()
			.filter(|(_, values)| values.len() > 1)
			.flat_map(|(_, values)| values.iter().copied())
			.collect();
		for &sei in &conflicting_seis {
			if let Ok(eid) = self
				.services
				.short
				.get_eventid_from_short::<OwnedEventId>(sei)
				.await
			{
				if let Some(&depth) = depth_by_eid.get(&eid) {
					sei_depth.insert(sei, depth);
				}
			}
		}

		// Build the final state: for each ssk, if non-conflicting keep it;
		// if conflicting, pick winner by latest depth (matching state_res behavior)
		let mut final_state = BTreeSet::new();
		for (&ssk, values) in &ssk_values {
			if values.len() == 1 {
				// Non-conflicting — keep the only value
				let sei = *values.iter().next().unwrap();
				final_state.insert(rooms::state_compressor::compress_state_event(ssk, sei));
			} else {
				// Conflicting — pick latest depth
				let mut best_sei = 0_u64;
				let mut best_depth = 0_u64;
				for &sei in values {
					let depth = sei_depth.get(&sei).copied().unwrap_or(0);
					if depth > best_depth || best_sei == 0 {
						best_depth = depth;
						best_sei = sei;
					}
				}
				final_state.insert(rooms::state_compressor::compress_state_event(ssk, best_sei));
			}
		}

		debug!("rebuild_state: merged state has {} entries", final_state.len());
		let merged_ssh = self
			.services
			.state_compressor
			.save_state(room_id, Arc::new(final_state))
			.await?
			.shortstatehash;
		Ok(merged_ssh)
	}
}
