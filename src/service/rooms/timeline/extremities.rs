use std::collections::{HashMap, HashSet};

use conduwuit_core::{Result, info, warn};
use futures::StreamExt;
use roaring::RoaringBitmap;
use ruma::{EventId, OwnedEventId};

use super::Service;
use crate::rooms::short::ShortEventId;

/// Detect stored extremities that are provably broken: they appear in the
/// window graph AND have children there (meaning they shouldn't be tips).
///
/// Returns the set of phantom tip event IDs. An empty return means the stored
/// extremities are consistent with the local DAG window.
///
/// This avoids false positives from:
/// - Stored extremities outside the tail window (unverifiable)
/// - MAX_FORWARD_EXTREMITIES capping (stored set is a subset of true tips)
pub fn detect_phantom_extremities<S1, S2, S3>(
	graph: &HashMap<OwnedEventId, HashSet<OwnedEventId, S2>, S1>,
	stored_extremities: &HashSet<OwnedEventId, S3>,
) -> Vec<OwnedEventId>
where
	S1: std::hash::BuildHasher,
	S2: std::hash::BuildHasher,
	S3: std::hash::BuildHasher,
{
	let has_children: HashSet<&OwnedEventId> =
		graph.values().flat_map(|parents| parents.iter()).collect();

	stored_extremities
		.iter()
		.filter(|eid| has_children.contains(eid))
		.cloned()
		.collect()
}

/// Calculate the true forward extremities of a DAG: events that have no
/// children (i.e., are not referenced as a parent by any other event).
///
/// Falls back to the last element in `sorted` if the entire graph is cyclic.
pub fn calculate_true_extremities<'a, S1, S2>(
	graph: &HashMap<OwnedEventId, HashSet<OwnedEventId, S2>, S1>,
	sorted: &'a [OwnedEventId],
) -> Vec<&'a EventId>
where
	S1: std::hash::BuildHasher,
	S2: std::hash::BuildHasher,
{
	let mut has_children: HashSet<OwnedEventId> = HashSet::new();
	for parents in graph.values() {
		for parent in parents {
			has_children.insert(parent.clone());
		}
	}

	let mut true_extremities: Vec<&EventId> = sorted
		.iter()
		.filter(|eid| !has_children.contains(*eid))
		.map(AsRef::as_ref)
		.collect();

	if true_extremities.is_empty() {
		if let Some(last_event_id) = sorted.last() {
			true_extremities.push(last_event_id.as_ref());
		}
	}

	true_extremities
}

/// Merge newly calculated true extremities with the current stored set,
/// removing phantom tips that have been proven stale.
pub fn merge_true_extremities<S: ::std::hash::BuildHasher>(
	true_extremities: Vec<&EventId>,
	current_set: &HashSet<OwnedEventId, S>,
	phantom_tips: &[OwnedEventId],
) -> HashSet<OwnedEventId> {
	let mut true_extremities_set: HashSet<OwnedEventId> = true_extremities
		.into_iter()
		.map(ToOwned::to_owned)
		.collect();

	for eid in current_set {
		if !phantom_tips.contains(eid) {
			true_extremities_set.insert(eid.clone());
		}
	}

	true_extremities_set
}

// --- Roaring bitmap variants for high-performance extremity calculations ---

#[must_use]
pub fn detect_phantom_extremities_roaring(
	graph: &[RoaringBitmap],
	stored_extremities: &RoaringBitmap,
) -> RoaringBitmap {
	let mut has_children = RoaringBitmap::new();
	for parents in graph {
		has_children |= parents;
	}

	stored_extremities & has_children
}

#[must_use]
pub fn calculate_true_extremities_roaring(
	graph: &[RoaringBitmap],
	sorted: &[u32],
) -> RoaringBitmap {
	let mut has_children = RoaringBitmap::new();
	for parents in graph {
		has_children |= parents;
	}

	let mut true_extremities = RoaringBitmap::new();
	for &id in sorted {
		if !has_children.contains(id) {
			true_extremities.insert(id);
		}
	}

	if true_extremities.is_empty() {
		if let Some(&last_id) = sorted.last() {
			true_extremities.insert(last_id);
		}
	}

	true_extremities
}

#[must_use]
pub fn merge_true_extremities_roaring(
	true_extremities: &RoaringBitmap,
	current_set: &RoaringBitmap,
	phantom_tips: &RoaringBitmap,
) -> RoaringBitmap {
	let mut true_extremities_set = true_extremities.clone();
	let valid_current = <&RoaringBitmap as std::ops::Sub>::sub(current_set, phantom_tips);
	true_extremities_set |= valid_current;
	true_extremities_set
}

impl Service {
	/// Prune fork storms down to operationally relevant tips using tail-based
	/// recalculation. This is a convenience wrapper around
	/// `recalculate_extremities` with standardized logging.
	pub async fn prune_extremities(&self, room_id: &ruma::RoomId, tail: usize) {
		match self.recalculate_extremities(room_id, tail, true).await {
			| Ok((true, tips)) => info!(
				%room_id, tail, tips,
				"pruned extremities via tail-based recalculation"
			),
			| Ok((false, tips)) => info!(
				%room_id, tail, tips,
				"extremities already consistent after recalculation"
			),
			| Err(e) => warn!(
				%room_id, tail,
				"failed to prune extremities: {e}"
			),
		}
	}

	/// Automatically recalculates the true topological DAG forward extremities
	/// by querying the last `tail` events from the room's timeline and
	/// analyzing their `prev_events` graph to find all nodes with out-degree
	/// 0. Optionally overwrites the stored forward extremities if `update_db`
	///    is true.
	/// Returns true if the extremities were changed (or would be changed).
	#[tracing::instrument(skip(self), level = "info")]
	pub async fn recalculate_extremities(
		&self,
		room_id: &ruma::RoomId,
		tail: usize,
		update_db: bool,
	) -> Result<(bool, usize)> {
		let state_lock = self.services.state.mutex.lock(room_id).await;

		let capacity = if tail == usize::MAX { 0 } else { tail };
		let mut eids = Vec::with_capacity(capacity);

		let mut stream = std::pin::pin!(self.db.room_event_ids_rev(room_id, None));
		while let Some(Ok(eid)) = stream.next().await {
			eids.push(eid);
			if eids.len() >= tail {
				break;
			}
		}

		// room_event_ids_rev returns newest first. We need oldest for true_extremities
		eids.reverse();

		let mut short_ids: Vec<ShortEventId> = Vec::with_capacity(eids.len());
		for eid in &eids {
			let short = self.services.short.get_shorteventid(eid).await?;
			short_ids.push(short);
		}

		let mut ts_map = HashMap::with_capacity(eids.len());
		let mut id_map: HashMap<ShortEventId, u32> = HashMap::with_capacity(eids.len());
		let mut reverse_id_map: Vec<ShortEventId> = Vec::with_capacity(eids.len());

		let get_or_insert_id = |short: ShortEventId,
		                        id_map: &mut HashMap<ShortEventId, u32>,
		                        reverse_id_map: &mut Vec<ShortEventId>|
		 -> u32 {
			if let Some(&id) = id_map.get(&short) {
				id
			} else {
				let id = u32::try_from(reverse_id_map.len()).unwrap_or(0);
				id_map.insert(short, id);
				reverse_id_map.push(short);
				id
			}
		};

		let mut graph: Vec<RoaringBitmap> = Vec::with_capacity(eids.len());
		let mut sorted: Vec<u32> = Vec::with_capacity(eids.len());

		for short in &short_ids {
			let id = get_or_insert_id(*short, &mut id_map, &mut reverse_id_map);
			let id_usize = usize::try_from(id).expect("u32 fits in usize");

			if id_usize >= graph.len() {
				graph.resize(id_usize.saturating_add(1), RoaringBitmap::new());
			}

			let mut prev_bitmap = RoaringBitmap::new();
			let prev_shorts = self
				.db
				.get_shortprevevents(*short)
				.await
				.unwrap_or_default();
			for prev_short in prev_shorts {
				prev_bitmap.insert(get_or_insert_id(
					prev_short,
					&mut id_map,
					&mut reverse_id_map,
				));
			}
			graph[id_usize] = prev_bitmap;
			sorted.push(id);
		}

		// Calculate true extremities via roaring bitmap intersections
		let true_extremities_bm = calculate_true_extremities_roaring(&graph, &sorted);

		let current_extremities = self.services.state.get_forward_extremities(room_id);
		let current_set: HashSet<_> = current_extremities.collect().await;

		let mut current_bm = RoaringBitmap::new();
		for eid in &current_set {
			if let Ok(short) = self.services.short.get_shorteventid(eid).await {
				if let Some(&id) = id_map.get(&short) {
					current_bm.insert(id);
				}
			}
		}

		let phantom_tips_bm = detect_phantom_extremities_roaring(&graph, &current_bm);
		let merged_extremities_bm =
			merge_true_extremities_roaring(&true_extremities_bm, &current_bm, &phantom_tips_bm);

		let mut true_extremities_set: HashSet<OwnedEventId> = HashSet::with_capacity(
			usize::try_from(merged_extremities_bm.len()).unwrap_or(usize::MAX),
		);
		for id in merged_extremities_bm {
			let short = reverse_id_map[usize::try_from(id).expect("u32 fits in usize")];
			if let Ok(eid) = self.services.short.get_eventid_from_short(short).await {
				true_extremities_set.insert(eid);
			}
		}

		// Add current extremities that were outside the graph window
		for eid in &current_set {
			if let Ok(short) = self.services.short.get_shorteventid(eid).await {
				if !id_map.contains_key(&short) {
					true_extremities_set.insert(eid.clone());
				}
			} else {
				// If we can't even get its shorteventid, still preserve it just in case
				true_extremities_set.insert(eid.clone());
			}
		}

		// Ensure we have timestamps for all tips we intend to keep
		for eid in &true_extremities_set {
			if !ts_map.contains_key(eid) {
				if let Ok(ts) = self.db.get_origin_server_ts(eid).await {
					ts_map.insert(eid.to_owned(), ts);
				}
			}
		}

		let mut final_extremities: Vec<OwnedEventId> = true_extremities_set.into_iter().collect();

		final_extremities.sort_by_key(|eid| {
			ts_map
				.get(eid)
				.copied()
				.unwrap_or_else(|| ruma::MilliSecondsSinceUnixEpoch(0_u32.into()))
		});

		let num_true_extremities = final_extremities.len();

		// If the finalized extremities perfectly match the current DB, we skip
		let final_set: HashSet<_> = final_extremities.iter().cloned().collect();
		if final_set == current_set {
			return Ok((false, num_true_extremities));
		}

		if update_db {
			// STRICT OVERWRITE: Erases phantom tips that fell out of the window.
			// set_forward_extremities enforces MAX_FORWARD_EXTREMITIES cap.
			self.services
				.state
				.set_forward_extremities(room_id, final_extremities.into_iter(), &state_lock)
				.await;
		}

		Ok((true, num_true_extremities))
	}

	/// Resolves, filters, chronologically sorts, prunes, and updates the
	/// forward extremities of a room. Returns the final pruned list of
	/// extremities.
	#[tracing::instrument(skip(self, graph, sorted, get_ts, state_lock), level = "info")]
	pub async fn update_true_extremities<F>(
		&self,
		room_id: &ruma::RoomId,
		graph: &HashMap<OwnedEventId, HashSet<OwnedEventId>>,
		sorted: &[OwnedEventId],
		get_ts: F,
		state_lock: &super::RoomMutexGuard,
	) -> Result<Vec<OwnedEventId>>
	where
		F: Fn(ShortEventId, &OwnedEventId) -> u64,
	{
		let mut true_extremities: Vec<OwnedEventId> = calculate_true_extremities(graph, sorted)
			.into_iter()
			.map(ToOwned::to_owned)
			.collect();

		// Preserve current/outlier extremities that are not in the timeline
		let current_exts: Vec<OwnedEventId> = self
			.services
			.state
			.get_forward_extremities(room_id)
			.collect()
			.await;
		let timeline_set: HashSet<&OwnedEventId> = sorted.iter().collect();
		for ext in current_exts {
			if !timeline_set.contains(&ext) {
				true_extremities.push(ext);
			}
		}

		// Resolve short IDs for soft-failure check and sorting
		let mut tips_with_shorts = Vec::with_capacity(true_extremities.len());
		for tip in true_extremities {
			if let Ok(short) = self.services.short.get_shorteventid(&tip).await {
				tips_with_shorts.push((short, tip));
			}
		}

		// Filter out soft-failed events (per Spec Server-Server API §Soft Failure:
		// soft-failed events must not be forward extremities).
		let mut filtered_tips = Vec::new();
		for (short, tip) in tips_with_shorts {
			if let Ok(meta) = self.db.get_event_metadata(&tip).await {
				if !meta.soft_failed {
					filtered_tips.push((short, tip));
				}
			} else {
				filtered_tips.push((short, tip));
			}
		}

		// Sort by origin_server_ts (oldest first) so that the newest extremities
		// are at the end of the vector. When set_forward_extremities takes the last N
		// elements, it will correctly keep the chronologically newest extremities
		// instead of getting poisoned by recently-inserted backfilled history.
		filtered_tips.sort_by_key(|(short, eid)| get_ts(*short, eid));

		// Enforce the configured cap to prevent state resolution OOMs.
		let max_extremities = self.services.globals.max_forward_extremities();
		let len = filtered_tips.len();
		if len > max_extremities {
			let prune_count = len.saturating_sub(max_extremities);
			info!(
				"update_true_extremities: pruning {} extremities down to {} for room {}",
				len, max_extremities, room_id
			);
			// Keep the last max_extremities (which are the newest)
			filtered_tips.drain(0..prune_count);
		}

		let final_extremities: Vec<OwnedEventId> =
			filtered_tips.into_iter().map(|(_, eid)| eid).collect();

		if !final_extremities.is_empty() {
			self.services
				.state
				.set_forward_extremities(room_id, final_extremities.iter().cloned(), state_lock)
				.await;
		}

		Ok(final_extremities)
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use ruma::{OwnedEventId, event_id};

	use super::*;

	#[test]
	fn test_calculate_true_extremities_00_single_tip() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());

		let sorted = vec![a, b.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![b.as_ref()];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_01_fork() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![a.clone()].into_iter().collect());

		let sorted = vec![a, b.clone(), c.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![b.as_ref(), c.as_ref()];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_02_diamond() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();
		let d = event_id!("$d").to_owned();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();

		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(d.clone(), vec![b.clone(), c.clone()].into_iter().collect());

		let sorted = vec![a, b, c, d.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![&*d];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_03_islands() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let x = event_id!("$x").to_owned();
		let y = event_id!("$y").to_owned();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();

		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(y.clone(), vec![x.clone()].into_iter().collect());

		let sorted = vec![a, b.clone(), x, y.clone()];
		let mut tips = calculate_true_extremities(&graph, &sorted);
		tips.sort();

		let mut expected: Vec<&EventId> = vec![&*b, &*y];
		expected.sort();

		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_04_missing_parents() {
		let a = event_id!("$a").to_owned();
		let z = event_id!("$z").to_owned(); // not in sorted, but referenced
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();

		graph.insert(a.clone(), vec![z].into_iter().collect());

		let sorted = vec![a.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![&*a];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_05_missing_from_graph() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		// Graph only knows about A's parents (none). B is omitted from the map
		// entirely.
		graph.insert(a.clone(), HashSet::new());

		let sorted = vec![a.clone(), b.clone()];
		let mut tips = calculate_true_extremities(&graph, &sorted);

		// Because B is in `sorted` and nothing in `graph` lists B as a parent, B must
		// be a tip. A is also a tip because nothing lists it as a parent.
		tips.sort();
		let mut expected: Vec<&EventId> = vec![&*a, &*b];
		expected.sort();

		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_06_cycle_fallback() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(a.clone(), vec![b.clone()].into_iter().collect());

		let sorted = vec![a, b.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);

		// Fallback returns the last element in `sorted`
		let expected: Vec<&EventId> = vec![&*b];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_07_no_cap() {
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		let mut sorted = Vec::new();
		let root = event_id!("$root").to_owned();
		sorted.push(root.clone());

		for i in 0..25 {
			let id: OwnedEventId = format!("$tip{i}").try_into().unwrap();
			graph.insert(id.clone(), vec![root.clone()].into_iter().collect());
			sorted.push(id);
		}

		let tips = calculate_true_extremities(&graph, &sorted);
		// No cap here — capping is done at the DB writer level
		// (set_forward_extremities) with MAX_FORWARD_EXTREMITIES = 10.
		assert_eq!(tips.len(), 25);
		assert_eq!(tips[0].as_str(), "$tip0");
		assert_eq!(tips[24].as_str(), "$tip24");
	}

	#[test]
	fn test_calculate_true_extremities_08_empty_input() {
		let graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		let sorted = vec![];
		let tips = calculate_true_extremities(&graph, &sorted);
		assert!(tips.is_empty(), "Empty graph should return empty extremities");
	}

	#[test]
	fn test_calculate_true_extremities_09_extraneous_graph_data() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let old = event_id!("$old").to_owned();
		let older = event_id!("$older").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();

		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		// Extraneous data outside the 'sorted' window
		graph.insert(old, vec![older].into_iter().collect());

		let sorted = vec![a, b.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);

		let expected: Vec<&EventId> = vec![&*b];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_10_out_of_order() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();

		// Linear chain: A -> B -> C
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![b.clone()].into_iter().collect());

		// Array is passed in completely scrambled chronological order
		let sorted = vec![c.clone(), a, b];
		let tips = calculate_true_extremities(&graph, &sorted);

		// Even though C was first in the array, A and B are in has_children.
		// The algorithm correctly identifies C as the sole extremity.
		let expected: Vec<&EventId> = vec![&*c];
		assert_eq!(tips, expected);
	}

	// --- detect_phantom_extremities tests ---

	#[test]
	fn test_phantom_11_no_drift_linear() {
		// Linear chain A -> B -> C, stored extremity is C (correct)
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		graph.insert(b.clone(), vec![a].into_iter().collect());
		graph.insert(c.clone(), vec![b].into_iter().collect());

		let stored: HashSet<OwnedEventId> = vec![c].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert!(phantoms.is_empty(), "correct tip should not be phantom");
	}

	#[test]
	fn test_phantom_12_real_drift() {
		// Linear chain A -> B -> C, but stored extremity is A (has children)
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c, vec![b].into_iter().collect());

		let stored: HashSet<OwnedEventId> = vec![a.clone()].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert_eq!(phantoms, vec![a], "A has children and is phantom");
	}

	#[test]
	fn test_phantom_13_out_of_window_tolerated() {
		// Window only has B -> C, but stored extremity includes $old (outside window)
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();
		let old = event_id!("$old").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		graph.insert(c.clone(), vec![b].into_iter().collect());

		// $old is stored but not in the window graph at all
		let stored: HashSet<OwnedEventId> = vec![c, old].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert!(phantoms.is_empty(), "out-of-window extremity should not be flagged");
	}

	#[test]
	fn test_phantom_14_capped_subset_ok() {
		// 25 fork tips from a root, but stored set is capped to 10
		let root = event_id!("$root").to_owned();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();

		let mut all_tips = Vec::new();
		for i in 0..25 {
			let id: OwnedEventId = format!("$tip{i}").try_into().unwrap();
			graph.insert(id.clone(), vec![root.clone()].into_iter().collect());
			all_tips.push(id);
		}

		// Stored set is a capped subset (first 10 tips)
		let stored: HashSet<OwnedEventId> = all_tips[..10].iter().cloned().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert!(phantoms.is_empty(), "capped tips are still valid tips");
	}

	#[test]
	fn test_phantom_15_mixed_valid_and_phantom() {
		// A -> B -> C, stored = {A, C}. A is phantom (has child B), C is valid.
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![b].into_iter().collect());

		let stored: HashSet<OwnedEventId> = vec![a.clone(), c].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert_eq!(phantoms, vec![a], "only A is phantom, C is valid");
	}

	// --- merge_true_extremities tests ---

	#[test]
	fn test_merge_true_extremities() {
		let e1 = event_id!("$1").to_owned();
		let e2 = event_id!("$2").to_owned();
		let e3 = event_id!("$3").to_owned();
		let e4 = event_id!("$4").to_owned();

		// newly discovered true extremity
		let true_exts = vec![&*e1];

		// current tips in DB
		let current_set: HashSet<OwnedEventId> = vec![e2.clone(), e3.clone(), e4.clone()]
			.into_iter()
			.collect();

		// phantom tips: e2 and e3 are phantoms
		let phantoms = vec![e2.clone(), e3.clone()];

		let result = merge_true_extremities(true_exts, &current_set, &phantoms);

		// Result should be e1 (true) and e4 (preserved from current_set because it's
		// not a phantom)
		assert_eq!(result.len(), 2);
		assert!(result.contains(&e1));
		assert!(result.contains(&e4));
		assert!(!result.contains(&e2));
		assert!(!result.contains(&e3));
	}

	// --- Roaring bitmap variant tests ---

	#[test]
	fn test_calculate_true_extremities_roaring_fork() {
		use roaring::RoaringBitmap;

		let mut graph = vec![RoaringBitmap::new(); 3];
		graph[1].insert(0); // 1 depends on 0
		graph[2].insert(0); // 2 depends on 0

		let sorted = vec![0, 1, 2];
		let tips = calculate_true_extremities_roaring(&graph, &sorted);

		let mut expected = RoaringBitmap::new();
		expected.insert(1);
		expected.insert(2);

		assert_eq!(tips, expected);
	}

	#[test]
	fn test_detect_phantom_extremities_roaring() {
		use roaring::RoaringBitmap;

		let mut graph = vec![RoaringBitmap::new(); 3];
		graph[1].insert(0);
		graph[2].insert(1);

		let mut stored_extremities = RoaringBitmap::new();
		stored_extremities.insert(0); // 0 is a phantom tip because it's a parent
		stored_extremities.insert(2); // 2 is a true tip

		let phantoms = detect_phantom_extremities_roaring(&graph, &stored_extremities);

		let mut expected = RoaringBitmap::new();
		expected.insert(0); // 0 should be detected as phantom

		assert_eq!(phantoms, expected);
	}

	#[test]
	fn test_merge_true_extremities_roaring() {
		use roaring::RoaringBitmap;

		let mut true_exts = RoaringBitmap::new();
		true_exts.insert(1);

		let mut current_set = RoaringBitmap::new();
		current_set.insert(2);
		current_set.insert(3);
		current_set.insert(4);

		let mut phantoms = RoaringBitmap::new();
		phantoms.insert(2);
		phantoms.insert(3);

		let result = merge_true_extremities_roaring(&true_exts, &current_set, &phantoms);

		let mut expected = RoaringBitmap::new();
		expected.insert(1); // from true_exts
		expected.insert(4); // from current_set (not a phantom)

		assert_eq!(result, expected);
	}
}
