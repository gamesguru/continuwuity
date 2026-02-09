use axum::extract::State;
use conduwuit::Result;
use ruma::{
	api::client::push::get_notifications,
	events::{
		AnySyncTimelineEvent, GlobalAccountDataEventType, StateEventType,
		push_rules::PushRulesEvent, room::power_levels::RoomPowerLevelsEventContent,
	},
	push::{Action, Ruleset, Tweak},
	serde::Raw,
	uint,
	MilliSecondsSinceUnixEpoch,
};
use futures::StreamExt;
use conduwuit_core::matrix::pdu::PduCount;
use ruma::api::client::push::get_notifications::v3 as r;

use crate::Ruma;

/// # `GET /_matrix/client/v3/notifications`
///
/// Get notifications for the user.
///
/// Currently just returns an empty response.
pub(crate) async fn get_notifications_route(
	State(services): State<crate::State>,
	body: Ruma<get_notifications::v3::Request>,
) -> Result<get_notifications::v3::Response> {
	// Extract the `limit` and `from` query parameters
	let limit = body.limit.unwrap_or(uint!(10));
	let from = body.from.as_deref();

	let mut notifications = Vec::new();

	// iterate over all rooms where the user has a notification count
	// this is efficient because we only scan rooms with unread messages
	let mut rooms_stream = services.users.stream_notification_counts(&body.sender_user);

	while let Some((room_id, count)) = rooms_stream.next().await {
		let Ok(room_id) = room_id else { continue };

		// Skip rooms with no notifications
		if count == 0 {
			continue;
		}

		// Get the last read receipt for this room (as a PDU count)
		let last_read = services
			.users
			.last_notification_read(&body.sender_user, &room_id)
			.await;

		// Get the power levels for the room (needed for push rules)
		let power_levels: RoomPowerLevelsEventContent = services
			.state_accessor
			.room_state_get_content(&room_id, &StateEventType::RoomPowerLevels, "")
			.await
			.unwrap_or_default();

		// Get user's push rules
		let global_account_data = services
			.account_data
			.get_global(&body.sender_user, GlobalAccountDataEventType::PushRules)
			.await;

		let ruleset = global_account_data
			.map(|ev: PushRulesEvent| ev.content.global)
			.unwrap_or_else(|| Ruleset::server_default(&body.sender_user));

		// Iterate over PDUs in the room *after* the last read receipt
		let mut pdus = services
			.rooms
			.timeline
			.pdus(&room_id, Some(PduCount::Normal(last_read)));

		while let Some(Ok((pdu_count, pdu))) = pdus.next().await {
			// Skip events sent by the user themselves
			if pdu.sender == body.sender_user {
				continue;
			}

			// Check push rules to see if this event should notify
			let pdu_json = services.rooms.timeline.get_pdu_json(&pdu.event_id).await?;
			let pdu_raw = Raw::new(&pdu_json).expect("CanonicalJsonValue is valid Raw<...>");

			let actions = services
				.pusher
				.get_actions(
					&body.sender_user,
					&ruleset,
					&power_levels,
					&pdu_raw,
					&room_id,
				)
				.await;

			let mut notify = false;
			let mut highlight = false;
			let mut sound = None;

			for action in actions {
				match action {
					| Action::Notify => notify = true,
					| Action::SetTweak(Tweak::Highlight(h)) => highlight = *h,
					| Action::SetTweak(Tweak::Sound(s)) => sound = Some(s.clone()),
					| _ => {},
				}
			}

			if notify {
				let event: AnySyncTimelineEvent = serde_json::from_value(serde_json::to_value(&pdu_json).expect("CanonicalJsonObject is valid Value"))
					.map_err(|e| conduwuit::Error::bad_database(format!("Invalid PDU event: {e}")))?;

				// Construct the Notification object
				notifications.push(r::Notification {
					actions: actions.to_vec(),
					event,
					profile_tag: None, // TODO
					read: false,       // We are scanning unread, so false
					room_id: room_id.to_owned(),
					ts: MilliSecondsSinceUnixEpoch(pdu.origin_server_ts),
				});
			}
		}
	}

	// Sort by timestamp descending (newest first)
	notifications.sort_by(|a, b| b.ts.cmp(&a.ts));

	// Apply limit
	let limited_notifications: Vec<_> = notifications
		.into_iter()
		.take(limit.try_into().unwrap_or(usize::MAX))
		.collect();

	// TODO: implement pagination token (next_token)
	// For now we return None, which means the client might re-request if they scroll?
	// But since this is "unread" focus, it might be fine.

	Ok(get_notifications::v3::Response {
		next_token: None,
		notifications: limited_notifications,
	})
}
