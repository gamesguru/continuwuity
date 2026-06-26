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

/// Check if `small` is a subset of `large` (every entry in small exists in
/// large with the same value).
#[inline]
fn is_subset(small: &StateMap<OwnedEventId>, large: &StateMap<OwnedEventId>) -> bool {
	small.len() <= large.len() && small.iter().all(|(k, v)| large.get(k) == Some(v))
}

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

		// Phase 2b: Pre-compute auth chains bottom-up (iterative DFS)
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
	// Uses an iterative post-order DFS with cycle detection to correctly handle
	// busted DAGs where auth events may appear out of order.

	fn rebuild_auth_chains(
		events_meta: &[EventMeta],
		event_cache: &HashMap<OwnedEventId, Arc<PduEvent>>,
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

				let eid = &idx_to_eid[curr];
				let mut all_resolved = true;
				if let Some(pdu) = event_cache.get(eid) {
					for auth_id in &pdu.auth_events {
						if let Some(&auth_idx) = eid_to_idx.get(auth_id) {
							let auth_usize = to_usize(auth_idx);
							if bitmaps[auth_usize].is_none() {
								if visiting[auth_usize] {
									warn!(
										"rebuild_state: auth chain cycle at {} -> {}",
										eid, auth_id,
									);
								} else {
									stack.push(auth_usize);
									all_resolved = false;
								}
							}
						}
					}
				}

				if all_resolved {
					let mut chain = roaring::RoaringBitmap::new();
					if let Some(pdu) = event_cache.get(eid) {
						for auth_id in &pdu.auth_events {
							if let Some(&auth_idx) = eid_to_idx.get(auth_id) {
								let auth_usize = to_usize(auth_idx);
								if let Some(resolved_chain) = &bitmaps[auth_usize] {
									chain.insert(auth_idx);
									chain |= resolved_chain;
								}
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

	// ── Phase 3: In-memory state walk with fork resolution ──
	// Uses Arc pointer sharing + group IDs to avoid cloning full state maps.
	// Message events inherit their parent's group ID (zero allocation).
	// Forks with subset/superset states skip state_res entirely.

	async fn rebuild_walk_state(
		ctx: &RebuildCtx,
	) -> (Vec<Arc<StateMap<OwnedEventId>>>, HashMap<OwnedEventId, usize>) {
		let start = Instant::now();
		let mut state_groups: Vec<Arc<StateMap<OwnedEventId>>> = Vec::new();
		let mut event_group: HashMap<OwnedEventId, usize> = HashMap::new();
		let mut fork_resolve_count = 0_usize;
		let mut fork_skip_count = 0_usize;
		let mut cumulative_resolve_time = Duration::ZERO;
		let mut processed = 0_usize;
		let total_events = ctx.events_meta.len();

		// Group 0 = empty state (for events with no parents)
		state_groups.push(Arc::new(StateMap::new()));
		let empty_group: usize = 0;

		// Cache: content hash of parent states -> resolved group ID.
		// Group IDs are unstable (each state event increments the counter), so we
		// hash the actual state content for cache keys instead.
		let mut fork_cache: HashMap<u64, usize> = HashMap::new();

		for (eid, prev_events, state_key, _depth) in &ctx.events_meta {
			processed = processed.saturating_add(1);

			if processed.is_multiple_of(1000) {
				debug!(
					"rebuild_state: {}/{} events | {} groups | {} forks resolved, {} skipped \
					 ({:?}) | elapsed: {:?}",
					processed,
					total_events,
					state_groups.len(),
					fork_resolve_count,
					fork_skip_count,
					cumulative_resolve_time,
					start.elapsed(),
				);
			}

			// Collect unique parent group IDs
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
					// Deduplicate parents by content equality (not just group ID)
					let mut unique_states: Vec<Arc<StateMap<OwnedEventId>>> = Vec::new();
					let mut unique_groups: Vec<usize> = Vec::new();
					for &g in &parent_groups {
						let state = &state_groups[g];
						if !unique_states
							.iter()
							.any(|s| Arc::ptr_eq(s, state) || **s == **state)
						{
							unique_states.push(state.clone());
							unique_groups.push(g);
						}
					}

					if unique_states.len() == 1 {
						fork_skip_count = fork_skip_count.saturating_add(1);
						unique_groups[0]
					} else {
						// Actually need state resolution
						// Build cache key from content hashes of parent states
						// (group IDs are unstable, but content hashes are stable)
						let cache_key = {
							let mut h = std::collections::hash_map::DefaultHasher::new();
							for s in &unique_states {
								for (k, v) in s.iter() {
									k.hash(&mut h);
									v.hash(&mut h);
								}
								// Separator between states
								0xFFFF_FFFFu32.hash(&mut h);
							}
							h.finish()
						};

						if let Some(&cached_gid) = fork_cache.get(&cache_key) {
							fork_skip_count = fork_skip_count.saturating_add(1);
							cached_gid
						} else {
							// Superset optimization: if one fork's state is a strict
							// superset of all others, use it directly (spec-compliant,
							// covers >99% of busted-DAG forks).
							let mut is_chain = true;
							let mut superset_idx = 0;
							for i in 1..unique_states.len() {
								let superset = &unique_states[superset_idx];
								let current = &unique_states[i];

								if is_subset(current, superset) {
									// current is covered by superset
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
								// Genuine conflict: full spec-compliant resolution
								let fork_start = Instant::now();
								let fork_state_refs: Vec<&StateMap<OwnedEventId>> =
									unique_states.iter().map(|s| &**s).collect();

								let resolved =
									Self::resolve_fork_with_states(ctx, &fork_state_refs).await;

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

								match resolved {
									| Ok(resolved_state) => {
										// Reuse parent group if resolved matches exactly
										if let Some(idx) = unique_states
											.iter()
											.position(|s| **s == resolved_state)
										{
											unique_groups[idx]
										} else {
											let gid = state_groups.len();
											state_groups.push(Arc::new(resolved_state));
											gid
										}
									},
									| Err(e) => {
										warn!(
											"rebuild_state: fork resolution failed for {}: {} — \
											 using first parent",
											eid, e,
										);
										unique_groups[0]
									},
								}
							};

							fork_cache.insert(cache_key, gid);
							gid
						}
					}
				},
			};

			// Apply state event (if applicable), or inherit parent group
			let group_after = if let Some(sk) = state_key {
				let Some(pdu) = ctx.event_cache.get(eid) else {
					warn!("rebuild_state: state event {eid} missing from cache — skipping");
					event_group.insert(eid.clone(), state_before_group);
					continue;
				};
				let event_type: ruma::events::StateEventType = pdu.kind.to_string().into();
				let state_key: conduwuit_core::matrix::StateKey = sk.as_str().into();

				let current_state = &state_groups[state_before_group];
				// Skip deep clone if state event is redundant (no-op)
				if current_state.get(&(event_type.clone(), state_key.clone())) == Some(eid) {
					state_before_group
				} else {
					let mut new_state = (**current_state).clone();
					new_state.insert((event_type, state_key), eid.clone());
					let gid = state_groups.len();
					state_groups.push(Arc::new(new_state));
					gid
				}
			} else {
				// Message event: inherit parent's group (zero allocation)
				state_before_group
			};

			event_group.insert(eid.clone(), group_after);

			if processed.is_multiple_of(5000) {
				tokio::task::yield_now().await;
			}
		}

		info!(
			"rebuild_state: in-memory walk done in {:?} | {} events, {} state groups, {} forks \
			 resolved, {} skipped ({:?})",
			start.elapsed(),
			processed,
			state_groups.len(),
			fork_resolve_count,
			fork_skip_count,
			cumulative_resolve_time,
		);

		(state_groups, event_group)
	}

	/// Resolve a fork between multiple parent state sets using state_res.
	async fn resolve_fork_with_states(
		ctx: &RebuildCtx,
		fork_states: &[&StateMap<OwnedEventId>],
	) -> Result<StateMap<OwnedEventId>> {
		let event_fetch = |id: OwnedEventId| {
			let pdu = ctx.event_cache.get(&id).cloned();
			async move { pdu }
		};

		let event_batch_fetch = |ids: Vec<OwnedEventId>| {
			let results: Vec<Arc<PduEvent>> = ids
				.iter()
				.filter_map(|id| ctx.event_cache.get(id).cloned())
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
	// Lazily compresses state groups as events reference them, with content-hash
	// deduplication. Writes pdu_shortstatehash for each event.

	async fn rebuild_batch_write(
		&self,
		room_id: &RoomId,
		events_meta: &[EventMeta],
		state_groups: &[Arc<StateMap<OwnedEventId>>],
		event_group: &HashMap<OwnedEventId, usize>,
		event_cache: &HashMap<OwnedEventId, Arc<PduEvent>>,
	) -> Result<(HashMap<usize, u64>, u64)> {
		let write_start = Instant::now();
		let mut cork = Some(self.db.db.cork());

		// 4a: Pre-cache ALL short IDs to avoid serial DB lookups.
		let precache_start = Instant::now();
		let mut unique_state_keys: HashSet<(String, String)> = HashSet::new();
		let mut unique_event_ids: HashSet<OwnedEventId> = HashSet::new();

		// Collect unique state entries — deduplicate by content equality
		let referenced_groups: HashSet<usize> = event_group.values().copied().collect();
		for &gid in &referenced_groups {
			let state = &state_groups[gid];
			for ((ty, sk), event_id) in state.iter() {
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

		// content_hash -> ssh for deduplication across different Arc instances
		// with identical content
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
				let state_map = &state_groups[group_id];
				let mut compressed = BTreeSet::new();
				for ((ty, sk), event_id) in state_map.iter() {
					let ssk = ssk_cache
						.get(&(ty.to_string(), sk.to_string()))
						.copied()
						.unwrap_or(0);
					let sei = sei_cache.get(event_id).copied().unwrap_or(0);
					compressed.insert(rooms::state_compressor::compress_state_event(ssk, sei));
				}

				// Content-hash dedup (SipHash-1-3, ephemeral in-memory only)
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
