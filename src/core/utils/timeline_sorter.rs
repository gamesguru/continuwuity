use std::collections::{HashMap, HashSet};

use ruma::{OwnedEventId, UInt};

use crate::PduCount;

/// Topologically sorts timeline events using Kahn's algorithm, falling back to
/// timestamp sorting for events that are involved in cycles or disconnected.
#[must_use]
pub fn sort_timeline_events<S: std::hash::BuildHasher>(
	entries: &HashMap<OwnedEventId, (PduCount, UInt), S>,
	graph: &HashMap<OwnedEventId, HashSet<OwnedEventId, S>, S>,
) -> Vec<OwnedEventId> {
	let mut in_degree: HashMap<OwnedEventId, usize> = HashMap::new();

	// Initialize in-degrees
	for node in entries.keys() {
		in_degree.insert(node.clone(), 0_usize);
	}

	// Calculate in-degrees based on graph edges
	for edges in graph.values() {
		for target in edges {
			if let Some(count) = in_degree.get_mut(target) {
				*count = count.saturating_add(1);
			}
		}
	}

	// Queue nodes with 0 in-degree
	let mut queue: Vec<OwnedEventId> = in_degree
		.iter()
		.filter(|&(_, &count)| count == 0)
		.map(|(node, _)| node.clone())
		.collect();

	// Sort queue by timestamp to ensure deterministic ordering among peers
	queue.sort_by_key(|node| {
		entries
			.get(node)
			.map_or_else(|| 0_u32.into(), |(_, ts)| *ts)
	});

	let mut sorted: Vec<OwnedEventId> = Vec::new();

	// Kahn's algorithm
	while let Some(node) = queue.pop() {
		sorted.push(node.clone());

		if let Some(edges) = graph.get(&node) {
			let mut newly_zero: Vec<OwnedEventId> = Vec::new();
			for target in edges {
				if let Some(count) = in_degree.get_mut(target) {
					*count = count.saturating_sub(1);
					if *count == 0 {
						newly_zero.push(target.clone());
					}
				}
			}
			// Sort newly freed nodes by timestamp to maintain determinism
			newly_zero
				.sort_by_key(|n| entries.get(n).map_or_else(|| 0_u32.into(), |(_, ts)| *ts));
			queue.extend(newly_zero);
		}
	}

	// Handle cycles or disconnected components
	if sorted.len() != entries.len() {
		// Collect unsorted nodes
		let mut remaining: Vec<OwnedEventId> = in_degree
			.into_iter()
			.filter(|&(_, count)| count > 0)
			.map(|(node, _)| node)
			.collect();

		// Fallback: sort remaining nodes purely by timestamp, breaking ties with
		// EventId string
		remaining.sort_by(|a, b| {
			let ts_a = entries.get(a).map_or_else(|| 0_u32.into(), |(_, ts)| *ts);
			let ts_b = entries.get(b).map_or_else(|| 0_u32.into(), |(_, ts)| *ts);
			ts_a.cmp(&ts_b).then_with(|| a.cmp(b))
		});

		sorted.extend(remaining);
	}

	sorted
}

#[cfg(test)]
mod tests {
	use ruma::event_id;

	use super::*;

	#[test]
	fn test_topological_sort_clean() {
		let mut entries = HashMap::new();
		let mut graph = HashMap::new();

		// A (ts: 1) -> B (ts: 2) -> C (ts: 3)
		let a = event_id!("$A").to_owned();
		let b = event_id!("$B").to_owned();
		let c = event_id!("$C").to_owned();

		entries.insert(a.clone(), (0_u64.into(), 1_u32.into()));
		entries.insert(b.clone(), (0_u64.into(), 2_u32.into()));
		entries.insert(c.clone(), (0_u64.into(), 3_u32.into()));

		graph.insert(a.clone(), vec![b.clone()].into_iter().collect());
		graph.insert(b.clone(), vec![c.clone()].into_iter().collect());

		let sorted = sort_timeline_events(&entries, &graph);
		assert_eq!(sorted, vec![a, b, c]);
	}

	#[test]
	fn test_topological_sort_with_cycle() {
		let mut entries = HashMap::new();
		let mut graph = HashMap::new();

		// A -> B -> C -> A (cycle) with D disconnected
		let a = event_id!("$A").to_owned();
		let b = event_id!("$B").to_owned();
		let c = event_id!("$C").to_owned();
		let d = event_id!("$D").to_owned();

		entries.insert(a.clone(), (0_u64.into(), 10_u32.into()));
		entries.insert(b.clone(), (0_u64.into(), 20_u32.into()));
		entries.insert(c.clone(), (0_u64.into(), 30_u32.into()));
		entries.insert(d.clone(), (0_u64.into(), 5_u32.into()));

		graph.insert(a.clone(), vec![b.clone()].into_iter().collect());
		graph.insert(b.clone(), vec![c.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![a.clone()].into_iter().collect());

		let sorted = sort_timeline_events(&entries, &graph);

		// D has 0 in-degree, so it goes first.
		// A, B, C form a cycle, so they fall back to timestamp sorting: A(10), B(20),
		// C(30)
		assert_eq!(sorted, vec![d, a, b, c]);
	}
}
