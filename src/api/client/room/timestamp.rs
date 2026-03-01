use axum::extract::State;
use conduwuit::{Err, Result};
use futures::{StreamExt, pin_mut};
use ruma::{MilliSecondsSinceUnixEpoch, api::client::room::get_event_by_timestamp};

use crate::Ruma;

/// # `GET /_matrix/client/unstable/org.matrix.msc3030/rooms/{roomId}/timestamp_to_event`
///
/// Get the ID of the event closest to the given timestamp.
pub(crate) async fn get_room_event_by_timestamp_route(
	State(services): State<crate::State>,
	body: Ruma<get_event_by_timestamp::v1::Request>,
) -> Result<get_event_by_timestamp::v1::Response> {
	// Maximum events to scan in the timestamp index before giving up.
	// Bounds worst-case work at O(MAX_SCAN) visibility checks.
	const MAX_SCAN: usize = 256;

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

	// Walk the timestamp index starting from the target timestamp in the
	// requested direction. The first visible event we encounter is the answer.
	// This is O(1) in the common case (first event is visible) and bounded at
	// O(MAX_SCAN) in the worst case.
	let stream = services
		.rooms
		.timeline
		.pdus_by_timestamp(room_id, ts.0.into(), dir);
	pin_mut!(stream);

	let mut scanned = 0_usize;
	while let Some(Ok(pdu)) = stream.next().await {
		scanned = scanned.saturating_add(1);
		if scanned > MAX_SCAN {
			break;
		}

		if services
			.rooms
			.state_accessor
			.user_can_see_event(body.sender_user(), room_id, &pdu.event_id)
			.await
		{
			return Ok(get_event_by_timestamp::v1::Response::new(
				pdu.event_id.clone(),
				MilliSecondsSinceUnixEpoch(pdu.origin_server_ts),
			));
		}
	}

	Err!(Request(NotFound("No visible event found near the given timestamp")))
}
