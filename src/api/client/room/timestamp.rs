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
	// Maximum events to scan before giving up. TODO: May belong as a config option.
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

	// Walk the from target toward destination, checking along way.
	// First event we find, we take. This is relatively cheap.
	let stream = services
		.rooms
		.timeline
		.pdus_by_timestamp(room_id, ts.0.into(), dir)
		.filter_map(|res| async {
			match res {
				| Ok(p) => Some(Ok(p)),
				| Err(e) if e.is_not_found() => {
					tracing::debug!("Skipping unresolvable event, ts-index: {e}");
					None
				},
				| Err(e) => Some(Err(e)),
			}
		})
		.take(MAX_SCAN);
	pin_mut!(stream);

	// Look for first visible event
	while let Some(res) = stream.next().await {
		// If the event is not found, skip it (the question mark is meaningful here)
		let pdu = res?;

		if services
			.rooms
			.state_accessor
			.user_can_see_event(body.sender_user(), room_id, &pdu.event_id)
			.await
		{
			// return the nearest event (or our best guess, at least)
			// the algo is hopefully cheap and not requiring feature flag gating
			return Ok(get_event_by_timestamp::v1::Response::new(
				pdu.event_id.clone(),
				MilliSecondsSinceUnixEpoch(pdu.origin_server_ts),
			));
		}
	}

	Err!(Request(NotFound("Unable to navigate to the given timestamp")))
}
