use std::collections::{HashMap, HashSet};

use conduwuit_core::{
	Result, debug, info,
	matrix::{
		event::Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	warn,
};
use futures::{StreamExt, TryStreamExt, pin_mut};
use ruma::{OwnedEventId, RoomId};

use super::{Service, extremities::calculate_true_extremities};
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
	) -> Result<usize> {
		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
		let state_lock = self.services.state.mutex.lock(room_id).await;

		// Collect all PDUs from the timeline.
		// We need (PduCount, origin_server_ts) per event — the PduCount is the
		// existing immutable stream order which we preserve.
		let mut entries: HashMap<OwnedEventId, (PduCount, ruma::UInt)> = HashMap::new();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		let dropped = 0_usize;

		debug!("reorder_timeline: reading all PDUs from timeline...");
		let pdus_backfill = self.pdus(room_id, Some(PduCount::min()));
		let pdus_normal = self.pdus(room_id, Some(PduCount::Normal(0)));
		let pdus = pdus_backfill.chain(pdus_normal);
		pin_mut!(pdus);
		while let Some((count, pdu)) = pdus.try_next().await? {
			let eid = pdu.event_id.clone();
			entries.insert(eid.clone(), (count, pdu.origin_server_ts));
			graph.insert(eid, pdu.prev_events().map(ToOwned::to_owned).collect());
			if entries.len().is_multiple_of(10000) {
				debug!("reorder_timeline: read {} PDUs so far...", entries.len());
				tokio::task::yield_now().await;
			}
		}

		if dropped > 0 {
			warn!("{dropped} PDUs had no JSON and were skipped during reorder");
		}

		debug!("reorder_timeline: collected {} PDUs ({dropped} dropped)", entries.len());

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

		// Rebuild topological index only -- stream order is immutable.
		let count = sorted.len();
		let reindex_start = std::time::Instant::now();
		debug!("reorder_timeline: rebuilding topological index for {count} events...");

		if !no_compute_state {
			// Full mode: rebuild topo index + recompute state snapshots
			let final_ssh = self
				.rebuild_topo_index_with_state(room_id, shortroomid, &sorted, &entries)
				.await;
			debug!("reorder_timeline: topo rebuild+state took {:?}", reindex_start.elapsed());

			if let Some(ssh) = final_ssh {
				if ssh != 0 {
					self.services
						.state
						.set_room_state(room_id, ssh, &state_lock);
					debug!("reorder_timeline: updated room shortstatehash to {ssh}");
				}
			}
		} else {
			// Fast mode: rebuild topo index only, no state computation
			let mut cork = Some(self.db.db.cork());
			for (i, event_id) in sorted.iter().enumerate() {
				let &(existing_count, _) = entries.get(event_id).expect("in sorted list");
				let pdu_id: RawPduId = PduId {
					shortroomid,
					shorteventid: existing_count,
				}
				.into();

				let local_topo_depth = u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1);
				self.db.reindex_topo(&pdu_id, event_id, local_topo_depth);

				if i.saturating_add(1).is_multiple_of(10000) {
					drop(cork.take());
					tokio::task::yield_now().await;
					cork = Some(self.db.db.cork());
				}
			}
			drop(cork.take());
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
	pub(super) async fn rebuild_topo_index_with_state(
		&self,
		room_id: &RoomId,
		shortroomid: ShortRoomId,
		sorted: &[OwnedEventId],
		entries: &HashMap<OwnedEventId, (PduCount, ruma::UInt)>,
	) -> Option<u64> {
		let count = sorted.len();

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

		let mut cork = Some(self.db.db.cork());
		for (i, event_id) in sorted.iter().enumerate() {
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

			let local_topo_depth = u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1);

			// Rebuild topo index entry with new depth
			self.db.reindex_topo(&pdu_id, event_id, local_topo_depth);

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

			if i.saturating_add(1).is_multiple_of(2000) {
				debug!(
					"reorder_timeline: rebuilt {}/{count} topo entries...",
					i.saturating_add(1)
				);
			}
			if i.saturating_add(1).is_multiple_of(10000) {
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
			}
		}
		drop(cork.take());

		current_shortstatehash.filter(|&ssh| ssh != 0)
	}
}
