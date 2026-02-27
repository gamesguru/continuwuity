use axum::extract::State;
use conduwuit::{Err, PduCount, Result};
use futures::{StreamExt, pin_mut};
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::{Direction, client::room::get_event_by_timestamp},
};

use crate::Ruma;

/// # `GET /_matrix/client/unstable/org.matrix.msc3030/rooms/{roomId}/timestamp_to_event`
///
/// Get the ID of the event closest to the given timestamp.
pub(crate) async fn get_room_event_by_timestamp_route(
	State(services): State<crate::State>,
	body: Ruma<get_event_by_timestamp::v1::Request>,
) -> Result<get_event_by_timestamp::v1::Response> {
	// Maximum PDUs to scan per binary search step before giving up on that range.
	// Bounds worst-case work at O(K * MAX_SCAN) where K = log2(timeline count).
	const MAX_SCAN_PER_STEP: usize = 64;
	let room_id = &body.room_id;
	let ts = body.ts;
	let dir = body.dir;

	if !services
		.rooms
		.state_cache
		.is_joined(body.sender_user(), room_id)
		.await
	{
		return Err!(Request(Forbidden(
			"You don't have permission to view events in this room."
		)));
	}

	let mut event = None;

	let Ok((first_count, _)) = services.rooms.timeline.first_item_in_room(room_id).await else {
		return Err!(Request(NotFound("No events found in this room")));
	};

	let mut low = first_count.into_signed();
	let mut high = services
		.rooms
		.timeline
		.last_timeline_count(room_id)
		.await?
		.into_signed();

	while low <= high {
		let mid = low.saturating_add(high.saturating_sub(low) / 2);
		let pdus = services
			.rooms
			.timeline
			.pdus(room_id, Some(PduCount::from_signed(mid.saturating_sub(1))));
		pin_mut!(pdus);

		let mut found_pdu = None;
		let mut found_count = 0_i64;
		let mut scanned = 0_usize;

		while let Some(Ok((count, pdu))) = pdus.next().await {
			scanned = scanned.saturating_add(1);
			if services
				.rooms
				.state_accessor
				.user_can_see_event(body.sender_user(), room_id, &pdu.event_id)
				.await
			{
				found_pdu = Some(pdu);
				found_count = count.into_signed();
				break;
			}
			if scanned >= MAX_SCAN_PER_STEP {
				break;
			}
		}

		if let Some(pdu) = found_pdu {
			let pdu_ts = MilliSecondsSinceUnixEpoch(pdu.origin_server_ts);

			if dir == Direction::Forward {
				if pdu_ts >= ts {
					event = Some(pdu);
					high = mid.saturating_sub(1);
				} else {
					low = found_count.saturating_add(1);
				}
			} else {
				// dir == Direction::Backward
				if pdu_ts <= ts {
					event = Some(pdu);
					low = found_count.saturating_add(1);
				} else {
					high = mid.saturating_sub(1);
				}
			}
		} else {
			// No visible events from mid onwards. Search before mid.
			high = mid.saturating_sub(1);
		}
	}

	if let Some(event) = event {
		Ok(get_event_by_timestamp::v1::Response::new(
			event.event_id.clone(),
			MilliSecondsSinceUnixEpoch(event.origin_server_ts),
		))
	} else {
		Err!(Request(NotFound("No event found for the given timestamp")))
	}
}
