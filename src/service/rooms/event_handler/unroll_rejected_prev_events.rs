use std::collections::HashSet;

use conduwuit::{implement, matrix::Event};
use ruma::{EventId, OwnedEventId};

#[implement(super::Service)]
pub(super) async fn unroll_rejected_prev_events(
	&self,
	prev_events: impl Iterator<Item = &EventId>,
) -> Vec<OwnedEventId> {
	let mut resolved = HashSet::new();
	let mut stack: Vec<OwnedEventId> = prev_events.map(ToOwned::to_owned).collect();

	while let Some(prev_id) = stack.pop() {
		if self.services.pdu_metadata.is_event_rejected(&prev_id).await {
			if let Ok(prev_pdu) = self.services.timeline.get_pdu(&prev_id).await {
				stack.extend(prev_pdu.prev_events().map(ToOwned::to_owned));
			}
		} else {
			resolved.insert(prev_id);
		}
	}

	resolved.into_iter().collect()
}
