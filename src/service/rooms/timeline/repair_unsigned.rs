use std::future::ready;

use conduwuit::{Event, PduCount, Result};
use futures::{StreamExt, pin_mut};
use ruma::{EventId, RoomId};

#[conduwuit_macros::implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn repair_room_unsigned(&self, room_id: &RoomId) -> Result<usize> {
	let pdus_stream = self
		.pdus(room_id, Some(PduCount::min()))
		.filter_map(|r| ready(r.ok()))
		.filter(|(_count, pdu)| ready(pdu.state_key().is_some()))
		.map(|(_count, pdu)| {
			let event_id = pdu.event_id().to_owned();
			let kind = pdu.kind().to_string();
			let state_key = pdu.state_key().unwrap_or_default().to_owned();
			async move {
				// Get the stored JSON
				let pdu_json = self.get_pdu_json(&event_id).await;

				// Try state snapshot lookup
				let prev_state = if let Ok(ssh) = self
					.services
					.state_accessor
					.pdu_shortstatehash(&event_id)
					.await
				{
					self.services
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
		.buffer_unordered(100);

	pin_mut!(pdus_stream);

	tracing::info!("repair_unsigned: starting streaming state event repair in {room_id}");

	let mut repaired = 0_usize;
	let mut skipped = 0_usize;
	let mut errors = 0_usize;

	while let Some((event_id, _kind, _state_key, pdu_json, prev_state)) = pdus_stream.next().await
	{
		let Ok(mut pdu_json): std::result::Result<ruma::CanonicalJsonObject, _> = pdu_json else {
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
					.filter(|eid| **eid != event_id);

				match replaces {
					| Some(prev_eid) => self.get_pdu(prev_eid).await.ok(),
					| None => {
						skipped = skipped.saturating_add(1);
						continue;
					},
				}
			},
		};

		// Populate from the previous state event
		if let Some(prev_state) = prev_state {
			if let Err(e) = super::update_unsigned_prev_content(&mut pdu_json, &prev_state) {
				tracing::warn!(%event_id, "repair_unsigned: failed to update unsigned: {e}");
				errors = errors.saturating_add(1);
				continue;
			}
		}

		// Write back
		let Ok(pdu_id) = self.get_pdu_id(&event_id).await else {
			errors = errors.saturating_add(1);
			continue;
		};

		if let Err(e) = self.replace_pdu(&pdu_id, &pdu_json).await {
			tracing::warn!(%event_id, "repair_unsigned: failed to write updated json: {e}");
			errors = errors.saturating_add(1);
		} else {
			repaired = repaired.saturating_add(1);
		}

		let processed = repaired.saturating_add(skipped).saturating_add(errors);
		if processed.is_multiple_of(1000) {
			tracing::info!(
				"repair_unsigned: {processed} processed ({repaired} repaired, {skipped} skipped)"
			);
		}
	}

	tracing::info!(
		"repair_unsigned complete for {room_id}: {repaired} repaired, {skipped} skipped, \
		 {errors} errors"
	);

	Ok(repaired)
}
