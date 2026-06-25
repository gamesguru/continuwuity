use std::collections::{HashMap, HashSet};

use conduwuit::{Event, Result, matrix::pdu::PduEvent};
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId};

pub(crate) struct TimelineStateResolver<'a> {
	pub(crate) room_id: &'a RoomId,
	pub(crate) room_version: &'a RoomVersionId,
	pub(crate) event_set: &'a HashSet<&'a EventId>,
	pub(crate) ssh_cache: &'a HashMap<OwnedEventId, u64>,
	pub(crate) resolved_state_cache: &'a mut HashMap<Vec<u64>, u64>,
	pub(crate) empty_ssh: u64,
}

#[conduwuit_core::implement(super::Service)]
pub(super) async fn resolve_state_before(
	&self,
	resolver: &mut TimelineStateResolver<'_>,
	pdu: &PduEvent,
) -> Result<u64> {
	let mut prev_sshs = Vec::new();
	for prev_id in pdu.prev_events() {
		if resolver.event_set.contains(prev_id) {
			if let Some(&pssh) = resolver.ssh_cache.get(prev_id) {
				prev_sshs.push(pssh);
			}
		} else {
			conduwuit_core::debug!(
				event_id = %pdu.event_id,
				%prev_id,
				"resolve_state_before: parent event not in timeline event_set (likely outlier or missing)"
			);
		}
	}

	let mut unique_sshs = prev_sshs.clone();
	unique_sshs.sort_unstable();
	unique_sshs.dedup();

	let state_before = match unique_sshs.len() {
		| 1 => unique_sshs[0],
		| 0 => resolver.empty_ssh,
		| _ =>
			if let Some(&cached_ssh) = resolver.resolved_state_cache.get(&unique_sshs) {
				cached_ssh
			} else {
				let compressed_state = self
					.services
					.event_handler
					.state_at_incoming_resolved(pdu, resolver.room_id, resolver.room_version)
					.await
					.unwrap_or_else(|_| std::sync::Arc::new(std::collections::BTreeSet::new()));

				let state_delta = self
					.services
					.state_compressor
					.save_state_with_parent(
						resolver.room_id,
						Some(unique_sshs[0]),
						compressed_state,
					)
					.await
					.ok();

				let ssh = state_delta.map_or(resolver.empty_ssh, |d| d.shortstatehash);
				resolver.resolved_state_cache.insert(unique_sshs, ssh);

				ssh
			},
	};

	Ok(state_before)
}
