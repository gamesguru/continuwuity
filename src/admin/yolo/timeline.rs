use conduwuit::{Result, info, matrix::Event, warn};
use futures::{StreamExt, future::ready, pin_mut};
use ruma::{EventId, OwnedEventId, OwnedRoomId};

use crate::admin_command;

#[admin_command]
pub(super) async fn reorder_timeline(
	&self,
	room_id: OwnedRoomId,
	all: bool,
	tail: Option<usize>,
) -> Result {
	self.bail_restricted()?;

	if all {
		let mut room_ids: Vec<OwnedRoomId> = Vec::new();
		let mut rooms = self.services.rooms.metadata.iter_ids();
		while let Some(room_id) = rooms.next().await {
			room_ids.push(room_id.to_owned());
		}
		drop(rooms);

		self.write_str(&format!("Reordering timeline for {} rooms...", room_ids.len()))
			.await?;

		let mut count = 0_usize;
		for room_id in room_ids {
			if Box::pin(
				self.services
					.rooms
					.timeline
					.reorder_timeline(&room_id, None),
			)
			.await
			.is_ok()
			{
				count = count.saturating_add(1);
			}
		}

		return self
			.write_str(&format!("Reordered timeline for {count} rooms. Clients should re-sync."))
			.await;
	}

	if let Some(n) = tail {
		self.write_str(&format!(
			"Reordering last {n} events in {room_id} by origin_server_ts (tail fast-path)..."
		))
		.await?;
		let count = Box::pin(
			self.services
				.rooms
				.timeline
				.reorder_timeline(&room_id, Some(n)),
		)
		.await?;
		return self
			.write_str(&format!(
				"Reordered {count} events in room {room_id}. Clients should re-sync this room."
			))
			.await;
	}

	self.write_str(&format!("Reordering timeline for {room_id} by origin_server_ts..."))
		.await?;

	let count = Box::pin(
		self.services
			.rooms
			.timeline
			.reorder_timeline(&room_id, None),
	)
	.await?;

	self.write_str(&format!(
		"Reordered {count} PDUs in room {room_id}. Clients should re-sync this room."
	))
	.await
}

#[admin_command]
pub(super) async fn purge_timeline_pdu(&self, event_id: OwnedEventId) -> Result {
	self.bail_restricted()?;

	let in_timeline = self
		.services
		.rooms
		.timeline
		.non_outlier_pdu_exists(&event_id)
		.await;

	let mut room_id_opt = None;
	if in_timeline {
		if let Ok(pdu) = self.services.rooms.timeline.get_pdu(&event_id).await {
			room_id_opt = pdu.room_id().map(ToOwned::to_owned);
		}
	}

	// Remove from timeline tables (pduid_pdu + eventid_pduid)
	self.services
		.rooms
		.timeline
		.remove_from_timeline(&event_id)
		.await;

	// Also remove from outlier tables
	self.services
		.rooms
		.outlier
		.remove_outlier(&event_id, None)
		.await;

	if in_timeline {
		if let Some(room_id) = room_id_opt {
			self.services
				.rooms
				.timeline
				.recalculate_extremities(&room_id, 100, true)
				.await?;
		}
		self.write_str(&format!(
			"Purged {event_id} from timeline and outlier tables. DAG Extremities automatically \
			 recalculated."
		))
		.await
	} else {
		self.write_str(&format!(
			"Event {event_id} was not in the timeline (purged outlier only)."
		))
		.await
	}
}

#[admin_command]
pub(super) async fn repair_unsigned(&self, room_id: OwnedRoomId) -> Result {
	use conduwuit::PduCount;

	self.bail_restricted()?;

	let pdus_stream = self
		.services
		.rooms
		.timeline
		.pdus(&room_id, Some(PduCount::min()))
		.filter_map(|r| ready(r.ok()))
		.filter(|(_count, pdu)| ready(pdu.state_key().is_some()))
		.map(|(_count, pdu)| {
			let event_id = pdu.event_id().to_owned();
			let kind = pdu.kind().to_string();
			let state_key = pdu.state_key().unwrap_or_default().to_owned();
			async move {
				// Get the stored JSON
				let pdu_json = self.services.rooms.timeline.get_pdu_json(&event_id).await;

				// Try state snapshot lookup
				let prev_state = if let Ok(ssh) = self
					.services
					.rooms
					.state_accessor
					.pdu_shortstatehash(&event_id)
					.await
				{
					self.services
						.rooms
						.state_accessor
						.state_get(ssh, &kind.clone().into(), &state_key)
						.await
						.ok()
						.filter(|prev| prev.event_id() != event_id)
				} else {
					None
				};

				(event_id, kind, state_key, pdu_json, prev_state)
			}
		})
		.buffer_unordered(500);

	pin_mut!(pdus_stream);

	info!("repair_unsigned: starting streaming state event repair in {room_id}");

	let mut repaired = 0_usize;
	let mut skipped = 0_usize;
	let mut errors = 0_usize;

	while let Some((event_id, _kind, _state_key, pdu_json, prev_state)) = pdus_stream.next().await
	{
		let Ok(mut pdu_json) = pdu_json else {
			errors = errors.saturating_add(1);
			continue;
		};

		let unsigned = pdu_json.entry("unsigned".to_owned()).or_insert_with(|| {
			ruma::CanonicalJsonValue::Object(std::collections::BTreeMap::new())
		});

		let ruma::CanonicalJsonValue::Object(unsigned) = unsigned else {
			errors = errors.saturating_add(1);
			continue;
		};

		// If no state snapshot, try replaces_state fallback
		let prev_state = match prev_state {
			| Some(_) => prev_state,
			| None => {
				let replaces = unsigned
					.get("replaces_state")
					.and_then(|v| v.as_str())
					.and_then(|s| <&EventId>::try_from(s).ok())
					.filter(|eid| *eid != event_id);

				match replaces {
					| Some(prev_eid) => self.services.rooms.timeline.get_pdu(prev_eid).await.ok(),
					| None => {
						skipped = skipped.saturating_add(1);
						continue;
					},
				}
			},
		};

		// Populate from the previous state event
		if let Some(prev_state) = prev_state {
			if let Err(e) = conduwuit_service::rooms::timeline::update_unsigned_prev_content(
				&mut pdu_json,
				&prev_state,
			) {
				warn!("repair_unsigned: failed to update unsigned for {event_id}: {e}");
				errors = errors.saturating_add(1);
				continue;
			}
		}

		// Write back
		let Ok(pdu_id) = self.services.rooms.timeline.get_pdu_id(&event_id).await else {
			errors = errors.saturating_add(1);
			continue;
		};

		if let Err(e) = self
			.services
			.rooms
			.timeline
			.replace_pdu(&pdu_id, &pdu_json)
			.await
		{
			warn!("Failed to replace PDU {event_id}: {e}");
			errors = errors.saturating_add(1);
			continue;
		}

		repaired = repaired.saturating_add(1);

		let processed = repaired.saturating_add(skipped).saturating_add(errors);
		if processed.is_multiple_of(1000) {
			info!(
				"repair_unsigned: {processed} processed ({repaired} repaired, {skipped} skipped)"
			);
		}
	}

	self.write_str(&format!(
		"Repair complete for room {room_id}: {repaired} state events repaired, {skipped} \
		 skipped (no state snapshot), {errors} errors"
	))
	.await
}
