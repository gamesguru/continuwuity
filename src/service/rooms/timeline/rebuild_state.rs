use std::{
	collections::{BTreeSet, HashMap, HashSet},
	sync::Arc,
};

use conduwuit_core::{
	Result, debug, info,
	matrix::{
		event::Event,
		state_res::{self, StateMap},
	},
	warn,
};
use futures::StreamExt;
use ruma::{OwnedEventId, RoomId, RoomVersionId, events::TimelineEventType};

use crate::rooms;

/// Safe u32 -> usize for Vec indexing of roaring bitmap indices.
#[inline]
fn to_usize(v: u32) -> usize { usize::try_from(v).expect("u32 fits in usize") }

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

		// ── Phase 1: Stream events and collect metadata ──
		info!("rebuild_state: streaming events in topological order...");
		let stream_start = std::time::Instant::now();

		let mut events_meta: Vec<(OwnedEventId, Vec<OwnedEventId>, Option<String>, u64)> =
			Vec::new();
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
			stream_start.elapsed(),
		);

		// ── Phase 2: Pre-load ALL events into RAM ──
		let prefetch_start = std::time::Instant::now();
		let event_cache: HashMap<OwnedEventId, Arc<conduwuit_core::PduEvent>> = {
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
			prefetch_start.elapsed(),
		);

		// Build event set for filtering
		let event_set: HashSet<&OwnedEventId> = events_meta.iter().map(|(eid, ..)| eid).collect();

		// ── Phase 2b: Pre-compute auth chains bottom-up ──
		// Since events are in topo order, each event's auth chain parents are
		// already computed. We store chains as roaring bitmaps indexed by event
		// position for compact O(1) union operations.
		let auth_chain_start = std::time::Instant::now();
		let eid_to_idx: HashMap<&OwnedEventId, u32> = events_meta
			.iter()
			.enumerate()
			.map(|(i, (eid, ..))| (eid, u32::try_from(i).expect("room has > 2^32 (4B) events")))
			.collect();
		let idx_to_eid: Vec<&OwnedEventId> = events_meta.iter().map(|(eid, ..)| eid).collect();

		let mut auth_chain_bitmaps: Vec<roaring::RoaringBitmap> =
			Vec::with_capacity(events_meta.len());
		for (eid, ..) in &events_meta {
			let mut chain = roaring::RoaringBitmap::new();
			if let Some(pdu) = event_cache.get(eid) {
				for auth_id in &pdu.auth_events {
					if let Some(&i) = eid_to_idx.get(auth_id) {
						chain.insert(i);
						// Only include transitive chain if bitmap already computed
						// (auth event appears before us in topo order). In busted DAGs,
						// auth events may appear later — we still record the direct ref.
						let idx = to_usize(i);
						if idx < auth_chain_bitmaps.len() {
							chain |= &auth_chain_bitmaps[idx];
						}
					}
				}
			}
			auth_chain_bitmaps.push(chain);
		}
		debug!(
			"rebuild_state: pre-computed {} auth chains in {:?}",
			auth_chain_bitmaps.len(),
			auth_chain_start.elapsed(),
		);

		// ── Phase 3: In-memory state walk ──
		// Track state per event using state group deduplication:
		// - state_groups: Vec of unique state maps (indexed by group_id)
		// - event_group: maps event_id → group_id
		// Non-state single-parent events share their parent's group (no clone).
		let rebuild_start = std::time::Instant::now();
		let mut state_groups: Vec<StateMap<OwnedEventId>> = Vec::new();
		let mut event_group: HashMap<OwnedEventId, usize> = HashMap::new();
		let mut fork_resolve_count = 0_usize;
		let mut cumulative_resolve_time = std::time::Duration::ZERO;
		let mut processed = 0_usize;
		let total_events = events_meta.len();

		// Group 0 = empty state (for events with no parents)
		state_groups.push(StateMap::new());
		let empty_group: usize = 0;

		for (eid, prev_events, state_key, _depth) in &events_meta {
			processed = processed.saturating_add(1);

			if processed.is_multiple_of(1000) {
				debug!(
					"rebuild_state: {}/{} events | {} groups | {} forks ({:?}) | elapsed: {:?}",
					processed,
					total_events,
					state_groups.len(),
					fork_resolve_count,
					cumulative_resolve_time,
					rebuild_start.elapsed(),
				);
			}

			// Collect parent groups (deduplicated)
			let parent_groups: Vec<usize> = prev_events
				.iter()
				.filter(|p| event_set.contains(p))
				.filter_map(|p| event_group.get(p).copied())
				.collect::<HashSet<usize>>()
				.into_iter()
				.collect();

			let state_before_group = match parent_groups.len() {
				| 0 => empty_group,
				| 1 => parent_groups[0],
				| _ => {
					// Fork: resolve in-memory
					let fork_start = std::time::Instant::now();
					let fork_states: Vec<&StateMap<OwnedEventId>> =
						parent_groups.iter().map(|&g| &state_groups[g]).collect();

					let event_fetch = |id: OwnedEventId| {
						let pdu = event_cache.get(&id).cloned();
						async move { pdu }
					};

					let event_batch_fetch = |ids: Vec<OwnedEventId>| {
						let results: Vec<Arc<conduwuit_core::PduEvent>> = ids
							.iter()
							.filter_map(|id| event_cache.get(id).cloned())
							.collect();
						async move { results }
					};

					// Pre-computed auth chain lookup via roaring bitmaps
					let auth_chain_fetch = |events: Vec<OwnedEventId>| {
						let mut combined = roaring::RoaringBitmap::new();
						for id in &events {
							if let Some(&i) = eid_to_idx.get(id) {
								combined.insert(i);
								combined |= &auth_chain_bitmaps[to_usize(i)];
							}
						}
						let chain: HashSet<OwnedEventId> = combined
							.iter()
							.map(|i| idx_to_eid[to_usize(i)].clone())
							.collect();
						async move { chain }
					};

					let resolved = state_res::resolve(
						&room_version,
						fork_states.iter().copied(),
						&event_fetch,
						Some(&event_batch_fetch),
						&auth_chain_fetch,
						None::<&fn(Vec<OwnedEventId>)>,
					)
					.await;

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
							state_groups.push(resolved_state);
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

			// Apply state event delta
			let group_after = if let Some(sk) = state_key {
				let Some(pdu) = event_cache.get(eid) else {
					warn!("rebuild_state: state event {eid} missing from cache — skipping");
					event_group.insert(eid.clone(), state_before_group);
					continue;
				};
				let event_type: ruma::events::StateEventType = pdu.kind.to_string().into();

				let mut new_state = state_groups[state_before_group].clone();
				new_state.insert((event_type, sk.as_str().into()), eid.clone());
				let gid = state_groups.len();
				state_groups.push(new_state);
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
			rebuild_start.elapsed(),
			processed,
			state_groups.len(),
			fork_resolve_count,
			cumulative_resolve_time,
		);

		// ── Phase 4: Batch-write to DB ──
		let write_start = std::time::Instant::now();
		let mut cork = Some(self.db.db.cork());

		// Deduplicate: group_id → shortstatehash (only compress each unique state once)
		let mut group_to_ssh: HashMap<usize, u64> = HashMap::new();
		let empty_ssh = self
			.services
			.state_compressor
			.save_state(room_id, Arc::new(BTreeSet::new()))
			.await?
			.shortstatehash;
		group_to_ssh.insert(empty_group, empty_ssh);

		let mut current_shortstatehash = empty_ssh;
		let mut groups_compressed = 0_usize;

		for (eid, _prev, state_key, _depth) in &events_meta {
			let Some(&group_id) = event_group.get(eid) else {
				continue;
			};

			// Lazily compress state groups as we encounter them
			let ssh = if let Some(&cached_ssh) = group_to_ssh.get(&group_id) {
				cached_ssh
			} else {
				// Compress this state group → shortstatehash
				let state_map = &state_groups[group_id];
				let mut compressed = BTreeSet::new();
				for ((ty, sk), event_id) in state_map {
					let ssk = self
						.services
						.short
						.get_or_create_shortstatekey(&ty.to_string().into(), sk)
						.await;
					let sei = self
						.services
						.short
						.get_or_create_shorteventid(event_id)
						.await;
					compressed.insert(rooms::state_compressor::compress_state_event(ssk, sei));
				}
				let result = self
					.services
					.state_compressor
					.save_state(room_id, Arc::new(compressed))
					.await?;
				let ssh = result.shortstatehash;
				group_to_ssh.insert(group_id, ssh);
				groups_compressed = groups_compressed.saturating_add(1);
				ssh
			};

			// Write pdu_shortstatehash for this event
			if state_key.is_some() {
				// State event: compute_state_for_event equivalent
				let (_, mut json) = self.db.get_from_eventid_pdu(eid).await?;
				let pdu_id: conduwuit_core::matrix::pdu::RawPduId =
					conduwuit_core::matrix::pdu::PduId {
						shortroomid: self.services.short.get_or_create_shortroomid(room_id).await,
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
				let shorteventid = self.services.short.get_or_create_shorteventid(eid).await;
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
			"rebuild_state: batch-write done in {:?} | {} unique groups compressed",
			write_start.elapsed(),
			groups_compressed,
		);

		// ── Phase 5: Final multi-head extremity merge ──
		let mut has_children: HashSet<&OwnedEventId> = HashSet::new();
		for (_, prev_events, ..) in &events_meta {
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

		if extremity_sshs.len() > 1 {
			debug!(
				"rebuild_state: {} forward extremities with {} unique SSHs — merging \
				 disconnected components...",
				num_extremities,
				extremity_sshs.len(),
			);

			// Load full compressed state for each unique SSH
			let mut all_compressed = BTreeSet::new();
			for &ssh in &extremity_sshs {
				if let Some(full_state) = self.services.state_compressor.get_full_state(ssh).await
				{
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
				current_shortstatehash = merged_ssh;
			} else {
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
						final_state
							.insert(rooms::state_compressor::compress_state_event(ssk, sei));
					} else {
						// Conflicting — pick winner by highest depth
						let mut best_sei = 0_u64;
						let mut best_depth = 0_u64;
						for &sei in values {
							let depth = sei_depth.get(&sei).copied().unwrap_or(0);
							if depth > best_depth || best_sei == 0 {
								best_depth = depth;
								best_sei = sei;
							}
						}
						final_state
							.insert(rooms::state_compressor::compress_state_event(ssk, best_sei));
					}
				}

				debug!("rebuild_state: merged state has {} entries", final_state.len());
				let merged_ssh = self
					.services
					.state_compressor
					.save_state(room_id, Arc::new(final_state))
					.await?
					.shortstatehash;
				current_shortstatehash = merged_ssh;
			}
		} else {
			debug!(
				"rebuild_state: all forward extremities share a single SSH — no multi-head \
				 merge needed",
			);
		}

		let (total_added, total_removed) = self
			.services
			.state_compressor
			.diff_full_state(original_room_shortstatehash.unwrap_or(0), current_shortstatehash)
			.await;

		// Now update the room's global state to match final calculated state
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
}
