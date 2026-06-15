use std::{
	cmp::Reverse,
	collections::{BinaryHeap, HashMap, HashSet},
	sync::Arc,
};

use conduwuit_core::{
	Result, info,
	matrix::{
		event::Event,
		pdu::{PduCount, PduEvent, PduId, RawPduId},
	},
	warn,
};
use futures::StreamExt;
use roaring::RoaringBitmap;
use ruma::{
	CanonicalJsonObject, OwnedEventId, RoomId,
	events::{StateEventType, TimelineEventType},
};

use super::{ExtractBody, update_unsigned_prev_content};
use crate::rooms;

/// Options controlling what `heal_room` computes and writes.
pub struct HealOptions {
	/// Clear soft-fail/rejected markers on healed events.
	pub clear_markers: bool,

	/// Compute state snapshots incrementally for each event.
	/// When false, events are inserted without state computation (faster).
	pub compute_state: bool,

	/// Rebuild the membership cache from the final state snapshot.
	pub rebuild_membership: bool,

	/// For reorder mode: existing timeline events have old PduCounts that
	/// need to be backed up and removed before re-insertion.
	pub is_reorder: bool,
}

/// Result of a `heal_room` operation.
pub struct HealResult {
	/// Number of events successfully inserted into the timeline.
	pub inserted: usize,

	/// Number of events skipped (already in timeline, no JSON, etc.)
	pub skipped: usize,

	/// Number of events that failed to process.
	pub failed: usize,

	/// The forward extremities computed for this room.
	pub extremities: Vec<OwnedEventId>,
}

/// Safe u32 index from usize, saturating at u32::MAX.
#[inline]
fn idx32(v: usize) -> u32 { u32::try_from(v).unwrap_or(u32::MAX) }

/// Safe usize from u32.
#[inline]
fn idx(v: u32) -> usize { usize::try_from(v).unwrap_or(usize::MAX) }

/// Compact u32-indexed DAG for efficient topological sort and extremity
/// calculation. Avoids cloning `OwnedEventId` in the hot path by mapping
/// all event IDs to dense u32 indices.
struct CompactDag {
	/// Bidirectional mapping: event_id ↔ u32 index.
	id_to_idx: HashMap<OwnedEventId, u32>,
	idx_to_id: Vec<OwnedEventId>,

	/// origin_server_ts per index for sort tiebreaking.
	timestamps: Vec<u64>,

	/// Forward adjacency: parents[idx] = set of parent indices within our set.
	parents: Vec<Vec<u32>>,

	/// Reverse adjacency: which indices have each idx as a parent.
	/// Built lazily during sort but tracked here for extremity calc.
	has_children: RoaringBitmap,
}

impl CompactDag {
	/// Build from a map of events, retaining only edges within the set.
	fn build(events: &HashMap<OwnedEventId, PduEvent>) -> Self {
		let n = events.len();
		let mut id_to_idx = HashMap::with_capacity(n);
		let mut idx_to_id = Vec::with_capacity(n);
		let mut timestamps = Vec::with_capacity(n);

		for (event_id, pdu) in events {
			let idx = u32::try_from(idx_to_id.len()).expect("event count fits u32");
			id_to_idx.insert(event_id.clone(), idx);
			idx_to_id.push(event_id.clone());
			timestamps.push(u64::from(pdu.origin_server_ts));
		}

		let mut parents = Vec::with_capacity(n);
		let mut has_children = RoaringBitmap::new();

		for event_id in &idx_to_id {
			let pdu = &events[event_id];
			let mut parent_indices = Vec::new();
			for prev_id in pdu.prev_events() {
				if let Some(&pidx) = id_to_idx.get(prev_id) {
					parent_indices.push(pidx);
					has_children.insert(pidx);
				}
			}
			parents.push(parent_indices);
		}

		Self {
			id_to_idx,
			idx_to_id,
			timestamps,
			parents,
			has_children,
		}
	}

	/// Kahn's topological sort on compact u32 indices.
	/// Tiebreaker: origin_server_ts ascending, then event_id ascending.
	/// Returns sorted indices (oldest/root first).
	fn topo_sort(&self) -> Vec<u32> {
		let n = self.idx_to_id.len();

		// Compute out-degree (number of parents within our set)
		let mut outdegree: Vec<u32> = self.parents.iter().map(|p| idx32(p.len())).collect();

		// Reverse adjacency: child → list of parents that depend on it
		let mut reverse: Vec<Vec<u32>> = vec![Vec::new(); n];
		for (child, parent_list) in self.parents.iter().enumerate() {
			let child = idx32(child);
			for &parent in parent_list {
				reverse[idx(parent)].push(child);
			}
		}

		// Seed with zero-outdegree nodes (roots — events with no parents in set)
		let mut heap: BinaryHeap<Reverse<(u64, &OwnedEventId, u32)>> = BinaryHeap::new();
		for (i, &deg) in outdegree.iter().enumerate() {
			if deg == 0 {
				heap.push(Reverse((self.timestamps[i], &self.idx_to_id[i], idx32(i))));
			}
		}

		let mut sorted = Vec::with_capacity(n);
		while let Some(Reverse((_, _, node))) = heap.pop() {
			sorted.push(node);
			// Release children whose last parent has been processed
			for &child in &reverse[idx(node)] {
				let ci = idx(child);
				outdegree[ci] = outdegree[ci].saturating_sub(1);
				if outdegree[ci] == 0 {
					heap.push(Reverse((self.timestamps[ci], &self.idx_to_id[ci], child)));
				}
			}
		}

		sorted
	}

	/// Compute forward extremities: events in sorted that have no children.
	fn extremities(&self, sorted: &[u32]) -> Vec<OwnedEventId> {
		sorted
			.iter()
			.filter(|&&i| !self.has_children.contains(i))
			.map(|&i| self.idx_to_id[idx(i)].clone())
			.collect()
	}

	/// Map a u32 index back to its event ID.
	#[inline]
	fn id(&self, i: u32) -> &OwnedEventId { &self.idx_to_id[idx(i)] }
}

#[conduwuit_core::implement(super::Service)]
pub async fn heal_room(
	&self,
	room_id: &RoomId,
	events: HashMap<OwnedEventId, PduEvent>,
	old_counts: Option<&HashMap<OwnedEventId, PduCount>>,
	options: &HealOptions,
) -> Result<HealResult> {
	if events.is_empty() {
		return Ok(HealResult {
			inserted: 0,
			skipped: 0,
			failed: 0,
			extremities: Vec::new(),
		});
	}

	let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
	let state_lock = self.services.state.mutex.lock(room_id).await;

	info!("heal_room: processing {} events for {}", events.len(), room_id);

	// Phase 1: Build compact DAG with u32 indices + roaring bitmap
	let dag = CompactDag::build(&events);

	// Phase 2: Topological sort on compact indices
	info!("heal_room: topological sort of {} events...", events.len());
	let sorted = dag.topo_sort();
	info!("heal_room: sorted {} events", sorted.len());

	if sorted.len() != events.len() {
		warn!(
			"heal_room: topo sort dropped {} events (cycles or missing parents)",
			events.len().saturating_sub(sorted.len())
		);
	}

	// Phase 3: If reorder mode, backup and remove old timeline entries
	if options.is_reorder {
		if let Some(old_counts) = old_counts {
			info!(
				"heal_room: backing up {} old timeline entries before removal...",
				old_counts.len()
			);

			// Backup old routing keys
			let mut backup_batch = database::rocksdb::WriteBatch::default();
			for (event_id, &old_count) in old_counts {
				let old_pdu_id: RawPduId = PduId { shortroomid, shorteventid: old_count }.into();
				self.db.room_pducount_eventid_backup.insert_into_batch(
					&mut backup_batch,
					&old_pdu_id,
					event_id.as_bytes(),
				);
			}
			self.db
				.room_pducount_eventid_backup
				.apply_batch(&backup_batch);

			// Remove old timeline entries
			let mut cork = Some(self.db.db.cork());
			for (i, &idx) in sorted.iter().enumerate() {
				let event_id = dag.id(idx);
				if let Some(&old_count) = old_counts.get(event_id) {
					let old_pdu_id: RawPduId =
						PduId { shortroomid, shorteventid: old_count }.into();
					self.db.remove_from_timeline_by_id(&old_pdu_id, event_id);
				}
				if i.saturating_add(1).is_multiple_of(10_000) {
					drop(cork.take());
					tokio::task::yield_now().await;
					cork = Some(self.db.db.cork());
				}
			}
			drop(cork.take());
		}
	}

	// Phase 4: Insert events in topological order with fresh PduCounts and
	// incremental state computation.
	let count = sorted.len();
	let batch_start = self
		.services
		.globals
		.next_count_batch(u64::try_from(count).unwrap_or(u64::MAX))?;

	info!(
		"heal_room: inserting {count} events (counter range {}..{})",
		batch_start,
		batch_start.saturating_add(u64::try_from(count).unwrap_or(u64::MAX))
	);

	let mut current_shortstatehash = if options.compute_state {
		// Try to seed from the oldest event's prev_events state
		let mut ssh = 0_u64;
		if let Some(&oldest_idx) = sorted.first() {
			if let Some(oldest_pdu) = events.get(dag.id(oldest_idx)) {
				if let Some(prev) = oldest_pdu.prev_events.first() {
					if let Ok(prev_ssh) =
						self.services.state_accessor.pdu_shortstatehash(prev).await
					{
						ssh = prev_ssh;
					}
				}
			}
		}
		Some(ssh)
	} else {
		None
	};

	let mut inserted = 0_usize;
	let mut skipped = 0_usize;
	let mut failed = 0_usize;
	let mut cork = Some(self.db.db.cork());

	for (i, &idx) in sorted.iter().enumerate() {
		let event_id = dag.id(idx);
		let Some(pdu) = events.get(event_id) else {
			skipped = skipped.saturating_add(1);
			continue;
		};

		// Get the canonical JSON for this event
		let (pdu_from_db, mut json) = match self.db.get_from_eventid_pdu(event_id).await {
			| Ok(res) => res,
			| Err(e) => {
				warn!(%event_id, "heal_room: PDU JSON missing (skipping): {e}");
				failed = failed.saturating_add(1);
				continue;
			},
		};

		// Clear markers if requested
		if options.clear_markers {
			self.services.pdu_metadata.clear_pdu_markers(event_id);
		}

		// Assign fresh PduCount
		let new_count = batch_start
			.saturating_add(u64::try_from(i).unwrap_or(u64::MAX))
			.saturating_add(1);
		let pdu_count = PduCount::Normal(new_count);
		let pdu_id: RawPduId = PduId { shortroomid, shorteventid: pdu_count }.into();

		// Compute state incrementally if enabled
		if let Some(mut ssh) = current_shortstatehash {
			self.compute_state_for_event(pdu, event_id, &mut json, &mut ssh, &pdu_id)
				.await;
			current_shortstatehash = Some(ssh);
		}

		// Write to timeline
		self.db
			.append_pdu(&pdu_id, &pdu_from_db, &json, pdu_count)
			.await;

		// Index searchable content
		if pdu.kind == TimelineEventType::RoomMessage {
			if let Ok(content) = pdu.get_content::<ExtractBody>() {
				if let Some(body) = &content.body {
					self.services.search.index_pdu(shortroomid, &pdu_id, body);
				}
			}
		}

		// Remove from outlier table if it was an outlier
		if !options.is_reorder {
			self.services.outlier.remove_outlier(event_id).await;
		}

		inserted = inserted.saturating_add(1);

		if inserted.is_multiple_of(2000) {
			info!("heal_room: inserted {inserted}/{count} events...");
		}
		if inserted.is_multiple_of(10_000) {
			drop(cork.take());
			tokio::task::yield_now().await;
			cork = Some(self.db.db.cork());
		}
	}

	drop(cork.take());

	// Phase 5: Calculate forward extremities using roaring bitmap
	let mut true_extremities = dag.extremities(&sorted);

	// Preserve outlier extremities not in our event set
	let current_exts: Vec<OwnedEventId> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.collect()
		.await;
	for ext in current_exts {
		if !dag.id_to_idx.contains_key(&ext) {
			true_extremities.push(ext);
		}
	}

	if !true_extremities.is_empty() {
		self.services
			.state
			.set_forward_extremities(room_id, true_extremities.clone().into_iter(), &state_lock)
			.await;

		info!("heal_room: set forward extremities to {} true DAG tips", true_extremities.len());
	}

	// Phase 6: Repair unsigned payload values
	if let Err(e) = self.repair_room_unsigned(room_id).await {
		warn!("heal_room: failed to repair unsigned payload values: {e}");
	}

	// Phase 7: Rebuild membership cache if requested
	if options.rebuild_membership {
		self.rebuild_membership_cache(room_id, &state_lock).await;
	}

	// Phase 8: Clean up backup routing map (reorder mode only)
	if options.is_reorder {
		if let Some(old_counts) = old_counts {
			let mut backup_batch = database::rocksdb::WriteBatch::default();
			for &old_count in old_counts.values() {
				let old_pdu_id: RawPduId = PduId { shortroomid, shorteventid: old_count }.into();
				self.db
					.room_pducount_eventid_backup
					.remove_from_batch(&mut backup_batch, old_pdu_id.as_ref());
			}
			self.db
				.room_pducount_eventid_backup
				.apply_batch(&backup_batch);
		}
	}

	// Ensure WAL is durable
	let final_sync = self.db.db.cork_and_sync();
	drop(final_sync);

	drop(state_lock);

	info!("heal_room: complete — {inserted} inserted, {skipped} skipped, {failed} failed");

	Ok(HealResult {
		inserted,
		skipped,
		failed,
		extremities: true_extremities,
	})
}

/// Incrementally compute and store the state snapshot for a single event.
/// Extracted to keep the main loop readable.
#[conduwuit_core::implement(super::Service)]
async fn compute_state_for_event(
	&self,
	pdu: &PduEvent,
	event_id: &OwnedEventId,
	json: &mut CanonicalJsonObject,
	ssh: &mut u64,
	_pdu_id: &RawPduId,
) {
	let shorteventid = self
		.services
		.short
		.get_or_create_shorteventid(&pdu.event_id)
		.await;
	self.services
		.state
		.set_pdu_shortstatehash(shorteventid, *ssh);

	let Some(state_key) = &pdu.state_key else {
		return;
	};

	// Repair unsigned.prev_content
	if *ssh != 0 {
		if let Ok(prev_state) = self
			.services
			.state_accessor
			.state_get(*ssh, &pdu.kind.to_string().into(), state_key)
			.await
		{
			if let Err(e) = update_unsigned_prev_content(json, &prev_state) {
				warn!(
					%event_id,
					"heal_room: failed to repair unsigned.prev_content: {e}"
				);
			}
		}
	}

	let states_parents = if *ssh != 0 {
		self.services
			.state_compressor
			.load_shortstatehash_info(*ssh)
			.await
			.unwrap_or_default()
	} else {
		Vec::new()
	};

	let shortstatekey = self
		.services
		.short
		.get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)
		.await;

	let new = self
		.services
		.state_compressor
		.compress_state_event(shortstatekey, &pdu.event_id)
		.await;

	let replaces = states_parents.last().and_then(|info| {
		info.full_state.as_ref().expect("top frame").iter().find(
			|bytes: &&rooms::state_compressor::CompressedStateEvent| {
				bytes.starts_with(&shortstatekey.to_be_bytes())
			},
		)
	});

	if Some(&new) != replaces {
		if let Ok(new_ssh) = self.services.globals.next_count() {
			let mut statediffnew = rooms::state_compressor::CompressedState::new();
			statediffnew.insert(new);
			let mut statediffremoved = rooms::state_compressor::CompressedState::new();
			if let Some(replaces) = replaces {
				statediffremoved.insert(*replaces);
			}
			let _ = self.services.state_compressor.save_state_from_diff(
				new_ssh,
				Arc::new(statediffnew),
				Arc::new(statediffremoved),
				2,
				states_parents,
			);
			*ssh = new_ssh;
		}
	}
}

/// Rebuild the membership cache from the current room state snapshot.
/// Extracted from the reorder_timeline logic for reuse.
#[conduwuit_core::implement(super::Service)]
async fn rebuild_membership_cache(
	&self,
	room_id: &RoomId,
	_state_lock: &rooms::state::RoomMutexGuard,
) {
	let mut members_synced = 0_usize;
	let mut state_joined: HashSet<ruma::OwnedUserId> = HashSet::new();
	let mut state_invited: HashSet<ruma::OwnedUserId> = HashSet::new();

	let room_ssh_opt = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.ok();

	if let Some(room_ssh) = room_ssh_opt {
		let state_full = self.services.state_accessor.state_full(room_ssh);
		let mut state_full = std::pin::pin!(state_full);
		while let Some(((event_type, state_key), pdu)) = state_full.next().await {
			if event_type != StateEventType::RoomMember {
				continue;
			}
			let Ok(uid) = ruma::OwnedUserId::try_from(state_key.as_str()) else {
				continue;
			};

			let content: serde_json::Value = pdu.get_content_as_value();
			let membership = content
				.get("membership")
				.and_then(|v| v.as_str())
				.unwrap_or("leave");

			match membership {
				| "join" => {
					state_joined.insert(uid.clone());
					if !self.services.state_cache.is_joined(&uid, room_id).await {
						self.services
							.state_cache
							.mark_as_joined_silent(&uid, room_id)
							.await;
						members_synced = members_synced.saturating_add(1);
					}
				},
				| "invite" => {
					state_invited.insert(uid.clone());
				},
				| _ => {
					if self
						.services
						.state_cache
						.is_invited_or_joined(&uid, room_id)
						.await
					{
						self.services
							.state_cache
							.mark_as_left_silent(&uid, room_id)
							.await;
						members_synced = members_synced.saturating_add(1);
					}
				},
			}
		}
	}

	// Sweep stale joined cache entries
	let cached_members: Vec<ruma::OwnedUserId> = self
		.services
		.state_cache
		.room_members(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut stale_removed = 0_usize;
	for user_id in &cached_members {
		if !state_joined.contains(user_id) && !state_invited.contains(user_id) {
			self.services
				.state_cache
				.mark_as_left_silent(user_id, room_id)
				.await;
			stale_removed = stale_removed.saturating_add(1);
		}
	}

	// Sweep stale invited cache entries
	let cached_invited: Vec<ruma::OwnedUserId> = self
		.services
		.state_cache
		.room_members_invited(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	for user_id in &cached_invited {
		if !state_invited.contains(user_id) && !state_joined.contains(user_id) {
			self.services
				.state_cache
				.mark_as_left_silent(user_id, room_id)
				.await;
			stale_removed = stale_removed.saturating_add(1);
		}
	}

	self.services.state_cache.update_joined_count(room_id).await;
	info!(
		"heal_room: synced {members_synced} membership cache entries, removed {stale_removed} \
		 stale"
	);
}
