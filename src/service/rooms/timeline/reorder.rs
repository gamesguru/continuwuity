use std::collections::HashMap;

use conduwuit_core::{
	Result, debug, info,
	matrix::{
		event::Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	warn,
};
use futures::StreamExt;
use ruma::{OwnedEventId, RoomId};

use super::{Service, extremities::calculate_true_extremities, metadata::EventMetadata};
use crate::rooms::short::ShortRoomId;

impl Service {
	/// Rebuild the topological index for a room using proper DAG
	/// topological sort.
	///
	/// Reads all PDUs, builds the DAG from `prev_events`, performs a
	/// topological sort (parents before children, Kahn's algorithm with
	/// chronological tiebreaking), then rebuilds the
	/// `roomid_topologicalorder_pducount` index with correct
	/// `local_topological_depth` values computed as
	/// `max(parent_depths) + 1`. Stream order
	/// (`room_pducount_eventid`) is NEVER modified — it is immutable
	/// arrival-time ordering.
	///
	/// Optionally recomputes state snapshots incrementally and repairs
	/// `unsigned.prev_content` on state events.
	pub async fn reorder_timeline(
		&self,
		room_id: &RoomId,
		no_compute_state: bool,
		force_reindex: bool,
	) -> Result<usize> {
		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
		let _insert_lock = if force_reindex {
			Some(self.mutex_insert.lock(room_id).await)
		} else {
			None
		};
		let state_lock = self.services.state.mutex.lock(room_id).await;

		// Lightweight collection: reads only metadata + shortprevevents,
		// avoids full PDU JSON deserialization.
		debug!("reorder_timeline: collecting timeline entries (lightweight)...");
		let collect_start = std::time::Instant::now();
		let (entries, mut graph, mut metadata_cache) =
			self.db.collect_reorder_entries(room_id).await?;
		debug!(
			"reorder_timeline: collected {} PDUs in {:?} (lightweight)",
			entries.len(),
			collect_start.elapsed()
		);

		if entries.is_empty() {
			return Ok(0);
		}

		// Retain only edges within our event set for both topo sort and extremities.
		for parents in graph.values_mut() {
			parents.retain(|prev_id| entries.contains_key(prev_id));
		}

		// Topological sort: parents before children (Kahn's algorithm).
		// Tiebreak on origin_server_ts then event_id for determinism.
		let start = std::time::Instant::now();
		debug!("reorder_timeline: topologically sorting {} events...", entries.len());
		let sorted = conduwuit::utils::timeline_sorter::sort_timeline_events(&entries, &graph);
		debug!(
			"reorder_timeline: topo sort took {:?} ({} events)",
			start.elapsed(),
			sorted.len()
		);

		if sorted.len() != entries.len() {
			warn!(
				"reorder_timeline: topo sort dropped {} events (cycles or disconnected)",
				entries.len().saturating_sub(sorted.len())
			);
		}

		// Rebuild topological index
		let count = sorted.len();
		let reindex_start = std::time::Instant::now();
		debug!("reorder_timeline: rebuilding topological index for {count} events...");

		let mut available_counts: Vec<PduCount> = Vec::new();
		if force_reindex {
			available_counts = entries.values().map(|(c, _)| *c).collect();
			available_counts.sort();
		}

		if !no_compute_state {
			// Full mode: rebuild topo index + recompute state snapshots
			let _final_ssh = self
				.rebuild_topo_index_with_state(
					room_id,
					shortroomid,
					&sorted,
					&entries,
					force_reindex,
					&available_counts,
					&mut metadata_cache,
				)
				.await;
			debug!("reorder_timeline: topo rebuild+state took {:?}", reindex_start.elapsed());
			// _final_ssh ignored; room state resolved via true extremities
			// below
		} else {
			// Fast mode: rebuild topo index only, no state computation.
			// Uses cached metadata to avoid all blocking DB reads.
			let mut depths: HashMap<OwnedEventId, u64> = HashMap::new();
			for event_id in &sorted {
				// TODO: Extract depth calculation from parents into a helper method
				let max_parent_depth = graph.get(event_id).map_or(0, |parents| {
					parents
						.iter()
						.filter_map(|p| depths.get(p))
						.copied()
						.max()
						.unwrap_or(0)
				});
				let local_topo_depth = max_parent_depth.saturating_add(1);
				depths.insert(event_id.clone(), local_topo_depth);
			}

			let cork = self.db.db.cork();
			if force_reindex {
				for (event_id, &(old_count, _)) in &entries {
					let old_pdu_id: RawPduId =
						PduId { shortroomid, shorteventid: old_count }.into();
					// Use cached depth to avoid blocking metadata read
					if let Some(meta) = metadata_cache.get(event_id) {
						self.db.remove_stream_and_topo_pducount_at_depth(
							&old_pdu_id,
							event_id.as_bytes(),
							meta.local_topological_depth,
						);
					} else {
						self.db
							.remove_stream_and_topo_pducount(&old_pdu_id, event_id.as_bytes());
					}
				}
			}
			for (i, event_id) in sorted.iter().enumerate() {
				let &(existing_count, _) = entries.get(event_id).expect("in sorted list");
				let new_count = if force_reindex {
					available_counts[i]
				} else {
					existing_count
				};
				let pdu_id: RawPduId = PduId { shortroomid, shorteventid: new_count }.into();
				let local_topo_depth = depths.get(event_id).copied().unwrap_or(0);

				if force_reindex {
					// Use cached metadata to avoid blocking DB reads
					if let Some(meta) = metadata_cache.get_mut(event_id) {
						self.db.replace_stream_topo_with_cached_metadata(
							&pdu_id,
							event_id,
							local_topo_depth,
							new_count,
							meta,
						);
					} else {
						self.db.replace_stream_and_topo_pducount(
							&pdu_id,
							event_id,
							local_topo_depth,
							new_count,
						);
					}
				} else {
					// Use cached metadata to avoid blocking DB reads
					if let Some(meta) = metadata_cache.get_mut(event_id) {
						self.db.reindex_topo_with_cached_metadata(
							&pdu_id,
							event_id,
							local_topo_depth,
							meta,
						);
					} else {
						self.db.reindex_topo(&pdu_id, event_id, local_topo_depth);
					}
				}
			}
			drop(cork);
			debug!("reorder_timeline: topo rebuild took {:?}", reindex_start.elapsed());
		}

		// Final batch: cork_and_sync ensures WAL is durable when dropped
		let final_sync = self.db.db.cork_and_sync();
		drop(final_sync);
		debug!("reorder_timeline: topo rebuild complete, calculating forward extremities...");

		// Calculate the true DAG forward extremities (events with in-degree 0
		// in the reversed graph). This fixes broken pagination and fork storms.
		let mut true_extremities: Vec<OwnedEventId> = calculate_true_extremities(&graph, &sorted)
			.into_iter()
			.map(ToOwned::to_owned)
			.collect();

		// Preserve outlier extremities (e.g. from force-set-state) that are not in
		// the timeline.
		let current_exts: Vec<OwnedEventId> = self
			.services
			.state
			.get_forward_extremities(room_id)
			.collect()
			.await;
		for ext in current_exts {
			if !entries.contains_key(&ext) {
				true_extremities.push(ext);
			}
		}

		if !true_extremities.is_empty() {
			self.services
				.state
				.set_forward_extremities(
					room_id,
					true_extremities.clone().into_iter(),
					&state_lock,
				)
				.await;

			info!(
				"reorder_timeline: set forward extremities to {} true DAG tips",
				true_extremities.len()
			);

			if !no_compute_state {
				let room_version = self
					.services
					.state
					.get_room_version(room_id)
					.await
					.unwrap_or(ruma::RoomVersionId::V11);

				let final_ssh = if true_extremities.len() == 1 {
					self.services
						.state_accessor
						.pdu_shortstatehash(&true_extremities[0])
						.await
						.ok()
				} else {
					info!(
						"reorder_timeline: resolving state across {} extremities",
						true_extremities.len()
					);

					if let Ok(Some(state)) = self
						.services
						.event_handler
						.resolve_extremities(
							true_extremities.iter().map(|id| &**id),
							room_id,
							&room_version,
						)
						.await
					{
						let compressed: crate::rooms::state_compressor::CompressedState = self
							.services
							.state_compressor
							.compress_state_events(state.iter().map(|(k, v)| (k, v.as_ref())))
							.collect()
							.await;
						let result = self
							.services
							.state_compressor
							.save_state_as_root(room_id.as_ref(), std::sync::Arc::new(compressed))
							.await;
						if let Ok(res) = result {
							Some(res.shortstatehash)
						} else {
							None
						}
					} else {
						None
					}
				};

				if let Some(ssh) = final_ssh {
					if ssh != 0 {
						self.services
							.state
							.set_room_state(room_id, ssh, &state_lock);
						debug!(
							"reorder_timeline: updated room shortstatehash to resolved state \
							 {ssh}"
						);
					}
				}
			}
		}

		debug!("reorder_timeline: skipped repair unsigned per metadata design");

		// Rebuild membership cache from the authoritative state snapshot.
		// This fixes stale/missing entries left by previous DAG fractures.
		// Bootstrap room state if missing (e.g. first reorder after import).
		if self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.is_err()
		{
			if let Some(latest_eid) = sorted.last() {
				if let Ok(ssh) = self
					.services
					.state_accessor
					.pdu_shortstatehash(latest_eid)
					.await
				{
					self.services
						.state
						.set_room_state(room_id, ssh, &state_lock);
					info!(
						"reorder_timeline: bootstrapped room state to shortstatehash {ssh} from \
						 latest event {latest_eid}"
					);
				}
			}
		}

		self.services
			.state_cache
			.reconcile_membership(room_id)
			.await;

		drop(state_lock);

		debug!("reorder_timeline: complete, {count} events reordered (topo index/state)");

		Ok(count)
	}

	/// Rebuild topological index with incremental state computation.
	///
	/// For each event in topo-sorted order: removes old topo entry,
	/// computes `local_topological_depth` as position in topo-sorted
	/// list, writes new topo key, and optionally recomputes state
	/// snapshots. Stream order is NOT touched.
	#[allow(clippy::too_many_arguments)]
	pub(super) async fn rebuild_topo_index_with_state(
		&self,
		room_id: &RoomId,
		shortroomid: ShortRoomId,
		sorted: &[OwnedEventId],
		entries: &HashMap<OwnedEventId, (PduCount, ruma::UInt)>,
		force_reindex: bool,
		available_counts: &[PduCount],
		metadata_cache: &mut HashMap<OwnedEventId, EventMetadata>,
	) -> Option<u64> {
		let mut current_shortstatehash = {
			let mut ssh = 0;
			if let Some(oldest_event_id) = sorted.first() {
				if let Ok(oldest_pdu) = self
					.db
					.get_pdu_in_room(Some(room_id), oldest_event_id)
					.await
				{
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
		};

		let mut depths: HashMap<OwnedEventId, u64> = HashMap::new();
		for event_id in sorted {
			// Use the existing stream order count -- do NOT fabricate a new one
			let Some(&(existing_count, _)) = entries.get(event_id) else {
				continue;
			};
			let pdu_id: RawPduId = PduId {
				shortroomid,
				shorteventid: existing_count,
			}
			.into();

			let (pdu, mut json) = match self.db.get_from_eventid_pdu(event_id).await {
				| Ok(res) => res,
				| Err(e) => {
					warn!(
						%event_id,
						"PDU missing during topo rebuild (skipping): {e}"
					);
					continue;
				},
			};

			// Events being reindexed are definitively in the timeline; any
			// rejection flags are stale and would poison state resolution
			// if left in place. Soft-fail flags are intentional and persist.
			self.services.pdu_metadata.unmark_event_rejected(event_id);

			// Use in-memory depths from topo-sorted order
			let max_parent_depth = pdu
				.prev_events()
				.filter_map(|p| depths.get(p))
				.copied()
				.max()
				.unwrap_or(0);
			let local_topo_depth = max_parent_depth.saturating_add(1);
			depths.insert(event_id.clone(), local_topo_depth);

			// State computation — uses existing pdu_id (unchanged stream order)
			if let Some(mut ssh) = current_shortstatehash {
				// Snapshot the JSON before state computation to detect changes
				let json_before = json.clone();
				self.compute_state_for_event(&pdu, event_id, &mut json, &mut ssh, &pdu_id)
					.await;
				current_shortstatehash = Some(ssh);

				// Only write JSON when unsigned.prev_content was actually repaired
				if json != json_before {
					self.db.update_pdu_json(event_id, &json);
				}
			}
		}

		// Now apply the DB replacements inside a single atomic cork.
		// Uses cached metadata to avoid blocking DB reads where possible.
		let cork = self.db.db.cork();
		if force_reindex {
			for (event_id, &(old_count, _)) in entries {
				let old_pdu_id: RawPduId = PduId { shortroomid, shorteventid: old_count }.into();
				if let Some(meta) = metadata_cache.get(event_id) {
					self.db.remove_stream_and_topo_pducount_at_depth(
						&old_pdu_id,
						event_id.as_bytes(),
						meta.local_topological_depth,
					);
				} else {
					self.db
						.remove_stream_and_topo_pducount(&old_pdu_id, event_id.as_bytes());
				}
			}
		}

		for (i, event_id) in sorted.iter().enumerate() {
			let Some(&(existing_count, _)) = entries.get(event_id) else { continue };
			let new_count = if force_reindex {
				available_counts[i]
			} else {
				existing_count
			};
			let pdu_id: RawPduId = PduId { shortroomid, shorteventid: new_count }.into();
			let local_topo_depth = depths.get(event_id).copied().unwrap_or(0);

			if force_reindex {
				if let Some(meta) = metadata_cache.get_mut(event_id) {
					self.db.replace_stream_topo_with_cached_metadata(
						&pdu_id,
						event_id,
						local_topo_depth,
						new_count,
						meta,
					);
				} else {
					self.db.replace_stream_and_topo_pducount(
						&pdu_id,
						event_id,
						local_topo_depth,
						new_count,
					);
				}
			} else {
				if let Some(meta) = metadata_cache.get_mut(event_id) {
					self.db.reindex_topo_with_cached_metadata(
						&pdu_id,
						event_id,
						local_topo_depth,
						meta,
					);
				} else {
					self.db.reindex_topo(&pdu_id, event_id, local_topo_depth);
				}
			}
		}
		drop(cork);

		current_shortstatehash.filter(|&ssh| ssh != 0)
	}
}
