use std::{
	collections::{BTreeSet, HashMap, HashSet},
	hash::{Hash, Hasher},
	pin::pin,
	sync::Arc,
	time::{Duration, Instant},
};

use conduwuit_core::{
	PduEvent, Result, debug, info,
	matrix::{event::Event, state_res::StateMap},
	warn,
};
use futures::StreamExt;
use ruma::{OwnedEventId, RoomId, RoomVersionId, events::TimelineEventType};

use crate::rooms;

/// Event metadata extracted during Phase 1 streaming.
/// Carries auth_events and (event_type, state_key) so we never need to load
/// full PduEvents into RAM during the walk. Only fork resolution (rare) needs
/// on-demand DB reads.
/// (event_id, prev_events, auth_events, Option<(event_type, state_key)>, depth)
type EventMeta = (
	OwnedEventId,
	Vec<OwnedEventId>,
	Vec<OwnedEventId>,
	Option<(String, String)>,
	u64,
);

/// Safe u32 -> usize for Vec indexing of roaring bitmap indices.
#[inline]
fn to_usize(v: u32) -> usize { usize::try_from(v).expect("u32 fits in usize") }

/// Check if `small` is a subset of `large` (every entry in small exists in
/// large with the same value).
#[inline]
fn is_subset(small: &StateMap<OwnedEventId>, large: &StateMap<OwnedEventId>) -> bool {
	small.len() <= large.len() && small.iter().all(|(k, v)| large.get(k) == Some(v))
}

/// Shared context threaded through all phases of rebuild_state.
/// No event_cache — metadata carries everything needed for the walk;
/// PDUs are fetched on-demand only during fork resolution.
struct RebuildCtx {
	room_version: RoomVersionId,
	events_meta: Vec<EventMeta>,
	event_set: HashSet<OwnedEventId>,
	eid_to_idx: HashMap<OwnedEventId, u32>,
	idx_to_eid: Vec<OwnedEventId>,
	auth_chain_bitmaps: Vec<roaring::RoaringBitmap>,
}

impl super::Service {
	/// Rebuilds room state entirely in-memory, then batch-writes the result to
	/// DB. Memory usage is dominated by metadata vectors and state groups, NOT
	/// by full PduEvent JSON. For a 60K-event room this uses ~50MB instead of
	/// the previous ~4GB.
	#[tracing::instrument(skip(self), level = "info")]
	pub async fn rebuild_state(&self, room_id: &RoomId) -> Result<()> {
		let original_room_shortstatehash = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.ok();

		// Phase 1: Stream events and extract metadata (no heavy JSON cache)
		let (events_meta, room_version) = self.rebuild_stream_events(room_id).await;

		let event_set: HashSet<OwnedEventId> =
			events_meta.iter().map(|(eid, ..)| eid.clone()).collect();

		// Phase 2b: Pre-compute auth chains bottom-up (iterative DFS)
		let (eid_to_idx, idx_to_eid, auth_chain_bitmaps) =
			Self::rebuild_auth_chains(&events_meta);

		let ctx = RebuildCtx {
			room_version,
			events_meta,
			event_set,
			eid_to_idx,
			idx_to_eid,
			auth_chain_bitmaps,
		};

		// Phase 3+4: In-memory state walk with eviction + inline DB writes
		let (event_ssh, current_shortstatehash) =
			self.rebuild_walk_and_write(room_id, &ctx).await?;

		// Phase 5: Final multi-head extremity merge
		let current_shortstatehash = self
			.rebuild_merge_extremities(
				room_id,
				&ctx.events_meta,
				&ctx.event_set,
				&event_ssh,
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
	// Now extracts auth_events and event_type directly, eliminating the need
	// for Phase 2 (prefetch).

	async fn rebuild_stream_events(&self, room_id: &RoomId) -> (Vec<EventMeta>, RoomVersionId) {
		info!("rebuild_state: streaming events in topological order...");
		let start = Instant::now();

		let mut events_meta: Vec<EventMeta> = Vec::new();
		let mut room_version = RoomVersionId::V1;
		let mut room_version_found = false;

		let mut stream = pin!(self.topo_pdus(room_id, None));
		while let Some(Ok((_pdu_count, pdu))) = stream.next().await {
			let eid = pdu.event_id().to_owned();
			let prev: Vec<OwnedEventId> = pdu.prev_events().map(ToOwned::to_owned).collect();
			let auth: Vec<OwnedEventId> = pdu.auth_events().map(ToOwned::to_owned).collect();
			let state_key = pdu
				.state_key()
				.map(|sk| (pdu.kind().to_string(), sk.to_owned()));
			let depth = u64::from(pdu.depth());

			// Timeline events are authoritative; clear any stale rejection flags.
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

			events_meta.push((eid, prev, auth, state_key, depth));
		}

		info!(
			"rebuild_state: streamed {} events in {:?} | room version: {}",
			events_meta.len(),
			start.elapsed(),
			room_version,
		);
		(events_meta, room_version)
	}

	// ── Phase 2b: Pre-compute auth chains bottom-up ──
	// Uses an iterative post-order DFS with cycle detection to correctly handle
	// busted DAGs where auth events may appear out of order.
	// Now reads auth_events directly from EventMeta instead of event_cache.

	fn rebuild_auth_chains(
		events_meta: &[EventMeta],
	) -> (HashMap<OwnedEventId, u32>, Vec<OwnedEventId>, Vec<roaring::RoaringBitmap>) {
		let start = Instant::now();

		// Pass 1: Index all events
		let eid_to_idx: HashMap<OwnedEventId, u32> = events_meta
			.iter()
			.enumerate()
			.map(|(i, (eid, ..))| {
				(eid.clone(), u32::try_from(i).expect("room has > 2^32 (4B) events"))
			})
			.collect();
		let idx_to_eid: Vec<OwnedEventId> =
			events_meta.iter().map(|(eid, ..)| eid.clone()).collect();

		// Pass 2: Iterative post-order traversal on auth DAG for transitive closures.
		// Uses an explicit stack instead of recursion (Rust has no TCO).
		let n = events_meta.len();
		let mut bitmaps: Vec<Option<roaring::RoaringBitmap>> = vec![None; n];
		let mut visiting = vec![false; n]; // Cycle detection

		for i in 0..n {
			let mut stack = vec![i];
			while let Some(&curr) = stack.last() {
				if bitmaps[curr].is_some() {
					stack.pop();
					continue;
				}

				visiting[curr] = true;

				// Read auth_events from metadata (index 2 in the tuple)
				let auth_events = &events_meta[curr].2;
				let mut all_resolved = true;
				for auth_id in auth_events {
					if let Some(&auth_idx) = eid_to_idx.get(auth_id) {
						let auth_usize = to_usize(auth_idx);
						if bitmaps[auth_usize].is_none() {
							if visiting[auth_usize] {
								warn!(
									"rebuild_state: auth chain cycle at {} -> {}",
									idx_to_eid[curr], auth_id,
								);
							} else {
								stack.push(auth_usize);
								all_resolved = false;
							}
						}
					}
				}

				if all_resolved {
					let mut chain = roaring::RoaringBitmap::new();
					for auth_id in auth_events {
						if let Some(&auth_idx) = eid_to_idx.get(auth_id) {
							let auth_usize = to_usize(auth_idx);
							if let Some(resolved_chain) = &bitmaps[auth_usize] {
								chain.insert(auth_idx);
								chain |= resolved_chain;
							}
						}
					}
					bitmaps[curr] = Some(chain);
					visiting[curr] = false;
					stack.pop();
				}
			}
		}

		// Unwrap all Options into final Vec
		let final_bitmaps: Vec<roaring::RoaringBitmap> =
			bitmaps.into_iter().map(Option::unwrap_or_default).collect();

		debug!(
			"rebuild_state: pre-computed {} auth chains in {:?}",
			final_bitmaps.len(),
			start.elapsed(),
		);
		(eid_to_idx, idx_to_eid, final_bitmaps)
	}

	// ── Phase 3+4: Merged state walk + inline DB writes + group eviction ──
	//
	// Combines the state walk (fork resolution, state event application) with
	// inline SSH compression and DB writes. Evicts state groups from memory as
	// soon as all their child events have been processed, keeping only the
	// "live frontier" in RAM (~100-200 groups instead of 10k+).
	//
	// No event_cache needed: state_key and event_type come from EventMeta.
	// PDUs are fetched on-demand only during fork resolution.

	async fn rebuild_walk_and_write(
		&self,
		room_id: &RoomId,
		ctx: &RebuildCtx,
	) -> Result<(HashMap<OwnedEventId, u64>, u64)> {
		let start = Instant::now();
		let mut cork = Some(self.db.db.cork());

		// ── Pre-cache short IDs ──
		let precache_start = Instant::now();
		let mut unique_state_keys: HashSet<(String, String)> = HashSet::new();
		for (_, _, _, state_key, _) in &ctx.events_meta {
			if let Some((ty, sk)) = state_key {
				unique_state_keys.insert((ty.clone(), sk.clone()));
			}
		}

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

		let mut sei_cache: HashMap<OwnedEventId, u64> =
			HashMap::with_capacity(ctx.events_meta.len());
		for (eid, ..) in &ctx.events_meta {
			let sei = self.services.short.get_or_create_shorteventid(eid).await;
			sei_cache.insert(eid.clone(), sei);
		}
		debug!(
			"rebuild_state: pre-cached {} shortstatekeys + {} shorteventids in {:?}",
			ssk_cache.len(),
			sei_cache.len(),
			precache_start.elapsed(),
		);

		// ── Pre-compute group eviction metadata ──
		let mut children_remaining: HashMap<&OwnedEventId, usize> = HashMap::new();
		for (_, prev_events, ..) in &ctx.events_meta {
			for p in prev_events {
				if ctx.event_set.contains(p) {
					let count = children_remaining.entry(p).or_insert(0);
					*count = count.saturating_add(1);
				}
			}
		}

		let mut group_live_refs: HashMap<usize, usize> = HashMap::new();

		// ── State walk + inline write ──
		let mut state_groups: HashMap<usize, Arc<StateMap<OwnedEventId>>> = HashMap::new();
		let mut event_group: HashMap<OwnedEventId, usize> = HashMap::new();
		let mut event_ssh: HashMap<OwnedEventId, u64> = HashMap::new();
		let mut group_to_ssh: HashMap<usize, u64> = HashMap::new();
		let mut fork_cache: HashMap<u64, usize> = HashMap::new();
		let mut content_to_ssh: HashMap<u64, u64> = HashMap::new();
		let mut next_gid: usize = 0;

		let mut fork_resolve_count = 0_usize;
		let mut fork_skip_count = 0_usize;
		let mut cumulative_resolve_time = Duration::ZERO;
		let mut groups_compressed = 0_usize;
		let mut groups_deduped = 0_usize;
		let mut groups_evicted = 0_usize;
		let mut processed = 0_usize;
		let total_events = ctx.events_meta.len();

		// Group 0 = empty state (for events with no parents)
		let empty_group: usize = next_gid;
		state_groups.insert(empty_group, Arc::new(StateMap::new()));
		next_gid = next_gid.saturating_add(1);

		let empty_ssh = self
			.services
			.state_compressor
			.save_state(room_id, Arc::new(BTreeSet::new()))
			.await?
			.shortstatehash;
		group_to_ssh.insert(empty_group, empty_ssh);

		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
		let mut current_shortstatehash = empty_ssh;

		for (eid, prev_events, _, state_key, _depth) in &ctx.events_meta {
			processed = processed.saturating_add(1);

			if processed.is_multiple_of(1000) {
				debug!(
					"rebuild_state: {}/{} events | {} live groups ({} evicted) | {} forks \
					 resolved, {} skipped ({:?}) | elapsed: {:?}",
					processed,
					total_events,
					state_groups.len(),
					groups_evicted,
					fork_resolve_count,
					fork_skip_count,
					cumulative_resolve_time,
					start.elapsed(),
				);
			}

			// ── Resolve parent state ──
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
					// Deduplicate parents by content equality
					let mut unique_states: Vec<Arc<StateMap<OwnedEventId>>> = Vec::new();
					let mut unique_groups: Vec<usize> = Vec::new();
					for &g in &parent_groups {
						let Some(state) = state_groups.get(&g) else {
							continue;
						};
						if !unique_states
							.iter()
							.any(|s| Arc::ptr_eq(s, state) || **s == **state)
						{
							unique_states.push(state.clone());
							unique_groups.push(g);
						}
					}

					if unique_states.len() <= 1 {
						fork_skip_count = fork_skip_count.saturating_add(1);
						unique_groups.first().copied().unwrap_or(empty_group)
					} else {
						// Build order-independent cache key from sorted content hashes
						let cache_key = {
							let mut state_hashes = Vec::with_capacity(unique_states.len());
							for s in &unique_states {
								let mut h = std::collections::hash_map::DefaultHasher::new();
								let mut entries: Vec<_> = s.iter().collect();
								entries.sort_unstable_by_key(|(k, _)| *k);
								for (k, v) in entries {
									k.hash(&mut h);
									v.hash(&mut h);
								}
								state_hashes.push(h.finish());
							}
							// Sort hashes so parent order doesn't matter
							state_hashes.sort_unstable();

							let mut h = std::collections::hash_map::DefaultHasher::new();
							for hash in state_hashes {
								hash.hash(&mut h);
							}
							h.finish()
						};

						if let Some(&cached_gid) = fork_cache.get(&cache_key) {
							fork_skip_count = fork_skip_count.saturating_add(1);
							cached_gid
						} else {
							// Superset optimization
							let mut is_chain = true;
							let mut superset_idx = 0;
							for i in 1..unique_states.len() {
								let superset = &unique_states[superset_idx];
								let current = &unique_states[i];

								if is_subset(current, superset) {
									// current is covered
								} else if is_subset(superset, current) {
									superset_idx = i;
								} else {
									is_chain = false;
									break;
								}
							}

							let gid = if is_chain {
								fork_skip_count = fork_skip_count.saturating_add(1);
								unique_groups[superset_idx]
							} else {
								// Genuine conflict: fetch PDUs on-demand + rezzy
								let fork_start = Instant::now();
								let fork_state_refs: Vec<&StateMap<OwnedEventId>> =
									unique_states.iter().map(|s| &**s).collect();

								let resolved = self
									.resolve_fork_with_states(room_id, ctx, &fork_state_refs)
									.await;

								let fork_elapsed = fork_start.elapsed();
								fork_resolve_count = fork_resolve_count.saturating_add(1);
								cumulative_resolve_time =
									cumulative_resolve_time.saturating_add(fork_elapsed);

								if fork_elapsed.as_millis() > 50 {
									debug!(
										"rebuild_state: SLOW fork #{} for {} ({} unique \
										 parents) took {:?}",
										fork_resolve_count,
										eid,
										unique_states.len(),
										fork_elapsed,
									);
								}

								// Reuse parent group if resolved matches exactly
								if let Some(idx) =
									unique_states.iter().position(|s| **s == resolved)
								{
									unique_groups[idx]
								} else {
									let gid = next_gid;
									next_gid = next_gid.saturating_add(1);
									state_groups.insert(gid, Arc::new(resolved));
									gid
								}
							};

							fork_cache.insert(cache_key, gid);
							gid
						}
					}
				},
			};

			// ── Apply state event or inherit parent group ──
			let group_after = if let Some((event_type_str, state_key_str)) = state_key {
				let event_type: ruma::events::StateEventType = event_type_str.as_str().into();
				let sk_typed: conduwuit_core::matrix::StateKey = state_key_str.as_str().into();

				let current_state = state_groups
					.get(&state_before_group)
					.cloned()
					.unwrap_or_default();

				// Skip deep clone if state event is redundant (no-op)
				if current_state.get(&(event_type.clone(), sk_typed.clone())) == Some(eid) {
					state_before_group
				} else {
					let mut new_state = (*current_state).clone();
					new_state.insert((event_type, sk_typed), eid.clone());
					let gid = next_gid;
					next_gid = next_gid.saturating_add(1);
					state_groups.insert(gid, Arc::new(new_state));
					gid
				}
			} else {
				// Message event: inherit parent's group (zero allocation)
				state_before_group
			};

			event_group.insert(eid.clone(), group_after);

			// ── Track group liveness ──
			if children_remaining.get(eid).copied().unwrap_or(0) > 0 {
				let refs = group_live_refs.entry(group_after).or_insert(0);
				*refs = refs.saturating_add(1);
			}

			// ── Inline SSH compression + write ──
			let ssh = if let Some(&cached_ssh) = group_to_ssh.get(&group_after) {
				cached_ssh
			} else {
				let state_map = state_groups.get(&group_after).cloned().unwrap_or_default();
				let mut compressed = BTreeSet::new();
				for ((ty, sk), event_id) in state_map.iter() {
					let ssk = ssk_cache
						.get(&(ty.to_string(), sk.to_string()))
						.copied()
						.unwrap_or(0);
					let sei = sei_cache.get(event_id).copied().unwrap_or(0);
					compressed.insert(rooms::state_compressor::compress_state_event(ssk, sei));
				}

				// Content-hash dedup
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
				group_to_ssh.insert(group_after, ssh);
				ssh
			};

			// Write pdu_shortstatehash for this event
			if state_key.is_some() {
				if let Ok((pdu, mut json)) = self.db.get_from_eventid_pdu(eid).await {
					let pdu_id: conduwuit_core::matrix::pdu::RawPduId =
						conduwuit_core::matrix::pdu::PduId {
							shortroomid,
							shorteventid: conduwuit_core::matrix::pdu::PduCount::Normal(0),
						}
						.into();
					let mut ssh_mut = ssh;
					self.compute_state_for_event(&pdu, eid, &mut json, &mut ssh_mut, &pdu_id)
						.await;
				}
			} else {
				let shorteventid = sei_cache.get(eid).copied().unwrap_or(0);
				self.services
					.state
					.set_pdu_shortstatehash(shorteventid, ssh);
			}

			event_ssh.insert(eid.clone(), ssh);
			current_shortstatehash = ssh;

			// ── Evict dead parent groups ──
			for p in prev_events {
				if let Some(remaining) = children_remaining.get_mut(p) {
					*remaining = remaining.saturating_sub(1);
					if *remaining == 0 {
						if let Some(&parent_gid) = event_group.get(p) {
							if let Some(refs) = group_live_refs.get_mut(&parent_gid) {
								*refs = refs.saturating_sub(1);
								if *refs == 0 {
									state_groups.remove(&parent_gid);
									group_live_refs.remove(&parent_gid);
									groups_evicted = groups_evicted.saturating_add(1);
								}
							}
						}
					}
				}
			}

			if groups_compressed.is_multiple_of(100) && groups_compressed > 0 {
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
			}
		}

		drop(cork.take());

		info!(
			"rebuild_state: walk+write done in {:?} | {} events, {} forks resolved, {} skipped \
			 ({:?}) | {} groups compressed, {} deduped, {} evicted, {} still live",
			start.elapsed(),
			processed,
			fork_resolve_count,
			fork_skip_count,
			cumulative_resolve_time,
			groups_compressed,
			groups_deduped,
			groups_evicted,
			state_groups.len(),
		);

		Ok((event_ssh, current_shortstatehash))
	}

	/// Resolve a fork between multiple parent state sets using on-demand PDU
	/// fetches and `rezzy`. Pre-separates unconflicted/conflicted, computes
	/// auth difference via roaring bitmaps, then fetches only the needed PDUs
	/// from RocksDB (typically ~200 events, <5ms).
	async fn resolve_fork_with_states(
		&self,
		room_id: &RoomId,
		ctx: &RebuildCtx,
		fork_states: &[&StateMap<OwnedEventId>],
	) -> StateMap<OwnedEventId> {
		// 1. Pre-separate into unconflicted and conflicted keys
		let num_maps = fork_states.len();
		let mut counts: HashMap<(String, String, String), usize> = HashMap::new();
		let mut key_to_ids: HashMap<(String, String), HashSet<String>> = HashMap::new();

		for map in fork_states {
			for ((ty, sk), id) in *map {
				let ty_s = ty.to_string();
				let sk_s = sk.to_string();
				let id_s = id.to_string();
				let count = counts
					.entry((ty_s.clone(), sk_s.clone(), id_s.clone()))
					.or_insert(0);
				*count = count.saturating_add(1);
				key_to_ids.entry((ty_s, sk_s)).or_default().insert(id_s);
			}
		}

		let mut unconflicted = std::collections::BTreeMap::new();
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

		let mut conflicted_eids: HashSet<OwnedEventId> = HashSet::new();
		for map in fork_states {
			for ((ty, sk), id) in *map {
				if conflicted_keys.contains(&(ty.to_string(), sk.to_string())) {
					conflicted_eids.insert(id.clone());
				}
			}
		}

		// Early exit: no conflicts means all states agree
		if conflicted_eids.is_empty() {
			return fork_states[0].clone();
		}

		// 2. Map room version early — needed to decide auth chain diff vs subgraph
		let version = match ctx.room_version.as_str() {
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

		// 3. Compute auth chains (needed for both V2 auth diff and V2_1+ context)
		let mut union_auth = roaring::RoaringBitmap::new();
		let mut intersect_auth = roaring::RoaringBitmap::new();
		let mut first = true;

		for map in fork_states {
			let mut chain = roaring::RoaringBitmap::new();
			for eid in map.values() {
				if let Some(&idx) = ctx.eid_to_idx.get(eid) {
					chain.insert(idx);
					chain |= &ctx.auth_chain_bitmaps[to_usize(idx)];
				}
			}
			if first {
				union_auth.clone_from(&chain);
				intersect_auth = chain;
				first = false;
			} else {
				union_auth |= &chain;
				intersect_auth &= &chain;
			}
		}

		// V2 only: auth chain diff events are also conflicted.
		// V2_1+ (MSC4297): uses conflicted state subgraph instead.
		if !is_v2_1_plus {
			let auth_diff = std::ops::Sub::sub(&union_auth, &intersect_auth);
			for idx in auth_diff {
				conflicted_eids.insert(ctx.idx_to_eid[to_usize(idx)].clone());
			}
		}

		// 4. Collect all event IDs we need to fetch for resolution
		let mut auth_context_eids: HashSet<OwnedEventId> = HashSet::new();
		for idx in union_auth {
			auth_context_eids.insert(ctx.idx_to_eid[to_usize(idx)].clone());
		}
		for state in fork_states {
			for eid in state.values() {
				auth_context_eids.insert(eid.clone());
			}
		}

		// 5. Fetch PDUs on-demand from RocksDB (typically ~200 events, <5ms)
		let mut fetch_ids: HashSet<OwnedEventId> = auth_context_eids.clone();
		fetch_ids.extend(conflicted_eids.iter().cloned());
		let fetch_ids_vec: Vec<OwnedEventId> = fetch_ids.into_iter().collect();

		let pdus = self
			.multi_get_pdus(Some(room_id), futures::stream::iter(fetch_ids_vec))
			.collect::<Vec<_>>()
			.await;
		let mut pdu_map: HashMap<OwnedEventId, PduEvent> = HashMap::new();
		for pdu in pdus.into_iter().flatten() {
			pdu_map.insert(pdu.event_id.clone(), pdu);
		}

		// 6. Convert PduEvent -> LeanEvent
		let to_lean = |pdu: &PduEvent| -> rezzy::LeanEvent {
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

		// 7. Build full context ONCE, then extract conflicted via remove()
		let mut auth_context: HashMap<String, rezzy::LeanEvent> = pdu_map
			.iter()
			.map(|(eid, pdu)| (eid.to_string(), to_lean(pdu)))
			.collect();

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

		// 8. Call rezzy's resolve_lean directly
		let resolved_lean =
			rezzy::resolve_lean(unconflicted, conflicted_events, &auth_context, version);

		// 9. Convert back to Ruma StateMap
		let mut resolved = StateMap::new();
		for ((ty_str, sk_opt), eid_str) in resolved_lean {
			let ty: ruma::events::StateEventType = ty_str.into();
			let sk: conduwuit_core::matrix::StateKey = sk_opt.unwrap_or_default().into();
			if let Ok(eid) = OwnedEventId::try_from(eid_str.as_str()) {
				resolved.insert((ty, sk), eid);
			}
		}

		resolved
	}

	// ── Phase 5: Final multi-head extremity merge ──
	// Handles rooms with multiple forward extremities by merging their state.

	async fn rebuild_merge_extremities(
		&self,
		room_id: &RoomId,
		events_meta: &[EventMeta],
		event_set: &HashSet<OwnedEventId>,
		event_ssh: &HashMap<OwnedEventId, u64>,
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
			.filter_map(|eid| event_ssh.get(eid).copied())
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
				"rebuild_state: all {} forward extremities share a single SSH — no multi-head \
				 merge needed",
				num_extremities,
			);
			return Ok(current_shortstatehash);
		}

		debug!(
			"rebuild_state: {} forward extremities with {} unique SSHs — merging...",
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
			"rebuild_state: {} conflicting keys across {} components — resolving by depth...",
			conflicting.len(),
			extremity_sshs.len(),
		);

		// Build ShortEventId -> depth map for conflicting SEIs
		let depth_by_eid: HashMap<&OwnedEventId, u64> = events_meta
			.iter()
			.map(|(eid, _, _, _, depth)| (eid, *depth))
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

		// Each ssk: non-conflicting keeps only value; conflicting picks latest depth
		let mut final_state = BTreeSet::new();
		for (&ssk, values) in &ssk_values {
			if values.len() == 1 {
				// Non-conflicting — keep the only value
				let sei = *values.iter().next().expect("non-empty set");
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
