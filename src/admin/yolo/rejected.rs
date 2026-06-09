use std::collections::HashSet;

use conduwuit::{Result, matrix::Event};
use futures::StreamExt;
use ruma::{OwnedEventId, OwnedRoomId};

use crate::admin_command;

#[admin_command]
pub(super) async fn manage_rejected(
	&self,
	event_ids: Vec<OwnedEventId>,
	unreject: bool,
	soft_fail: bool,
) -> Result {
	let mut changed = 0_usize;
	let mut already = 0_usize;

	for event_id in &event_ids {
		let is_rejected = self
			.services
			.rooms
			.pdu_metadata
			.is_event_rejected(event_id)
			.await;
		let is_soft_failed = self
			.services
			.rooms
			.pdu_metadata
			.is_event_soft_failed(event_id)
			.await;

		if unreject {
			if is_rejected {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_rejected(event_id);
				changed = changed.saturating_add(1);
			} else {
				already = already.saturating_add(1);
			}
			if soft_fail && is_soft_failed {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_soft_failed(event_id);
			}
		} else {
			if !is_rejected {
				self.services
					.rooms
					.pdu_metadata
					.mark_event_rejected(event_id);
				changed = changed.saturating_add(1);
			} else {
				already = already.saturating_add(1);
			}
			if soft_fail && !is_soft_failed {
				self.services
					.rooms
					.pdu_metadata
					.mark_event_soft_failed(event_id);
			}
		}
	}

	let action = if unreject { "unrejected" } else { "marked rejected" };
	let already_desc = if unreject {
		"already not rejected"
	} else {
		"already rejected"
	};
	let sf_note = if soft_fail { " (+ soft-fail marker)" } else { "" };
	self.write_str(&format!(
		"{changed} event(s) {action}{sf_note} ({already} {already_desc}, {} total)\n",
		event_ids.len()
	))
	.await
}

#[admin_command]
pub(super) async fn unreject_room(
	&self,
	room_id: OwnedRoomId,
	dry_run: bool,
	soft_fail: bool,
) -> Result {
	self.bail_restricted()?;

	let mut unmarked = 0_usize;
	let mut soft_unmarked = 0_usize;
	let mut total = 0_usize;

	// Collect all event IDs from timeline + outlier tree
	let mut pdu_ids: HashSet<OwnedEventId> = self
		.services
		.rooms
		.timeline
		.all_pdus(&room_id)
		.map(|(_, pdu)| pdu.event_id().to_owned())
		.collect()
		.await;

	let outlier_count_before = pdu_ids.len();

	let outliers: Vec<OwnedEventId> = self
		.services
		.rooms
		.outlier
		.room_stream(&room_id)
		.map(|(event_id, _)| event_id)
		.collect()
		.await;

	pdu_ids.extend(outliers);

	self.write_str(&format!(
		"Scanning {} events ({} timeline, {} outliers)...\n",
		pdu_ids.len(),
		outlier_count_before,
		pdu_ids.len().saturating_sub(outlier_count_before),
	))
	.await?;

	for event_id in &pdu_ids {
		if self
			.services
			.rooms
			.pdu_metadata
			.is_event_rejected(event_id)
			.await
		{
			total = total.saturating_add(1);
			if !dry_run {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_rejected(event_id);
				unmarked = unmarked.saturating_add(1);
			}
		}
		if soft_fail
			&& self
				.services
				.rooms
				.pdu_metadata
				.is_event_soft_failed(event_id)
				.await
		{
			if !dry_run {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_soft_failed(event_id);
				soft_unmarked = soft_unmarked.saturating_add(1);
			}
		}
	}

	if dry_run {
		self.write_str(&format!(
			"Dry run: Found {total} rejected events in {room_id} to unmark.\n"
		))
		.await
	} else {
		let soft_msg = if soft_fail {
			format!(", {soft_unmarked} soft-fail markers cleared")
		} else {
			String::new()
		};
		self.write_str(&format!("Unmarked {unmarked} rejected events{soft_msg} in {room_id}.\n"))
			.await
	}
}
