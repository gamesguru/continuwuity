use axum::extract::State;
use conduwuit::Result;
use ruma::{
	api::client::push::get_notifications,
	events::{
		AnySyncTimelineEvent, GlobalAccountDataEventType, StateEventType,
		push_rules::PushRulesEvent, room::power_levels::RoomPowerLevelsEventContent,
	},
	push::{Action, Ruleset},
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
	let _from = body.from.as_deref();

	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	let mut notifications = Vec::new();

	// iterate over all rooms where the user has a notification count
	// this is efficient because we only scan rooms with unread messages
	let mut rooms_stream = std::pin::pin!(services.rooms.user.stream_notification_counts(sender_user));

	while let Some((room_id, count)) = rooms_stream.next().await {
		let Ok(room_id) = room_id else { continue };

		// Skip rooms with no notifications
		if count == 0 {
			continue;
		}

		// Get the last read receipt for this room (as a PDU count)
		let last_read = services
			.rooms
			.user
			.last_notification_read(sender_user, &room_id)
			.await;

		// Get the power levels for the room (needed for push rules)
		let power_levels: RoomPowerLevelsEventContent = services
			.rooms
			.state_accessor
			.room_state_get_content(&room_id, &StateEventType::RoomPowerLevels, "")
			.await
			.unwrap_or_default();

		// Get user's push rules
		let global_account_data = services
			.account_data
			.get_global(sender_user, GlobalAccountDataEventType::PushRules)
			.await;

		let ruleset = global_account_data
			.map(|ev: PushRulesEvent| ev.content.global)
			.unwrap_or_else(|_| Ruleset::server_default(sender_user));

		// Iterate over PDUs in the room *after* the last read receipt
		let mut pdus = std::pin::pin!(services
			.rooms
			.timeline
			.pdus(&room_id, Some(PduCount::Normal(last_read))));

		while let Some(Ok((_pdu_count, pdu))) = pdus.next().await {
			// Skip events sent by the user themselves
			if pdu.sender == *sender_user {
				continue;
			}

			// Check push rules to see if this event should notify
			let pdu_json = services.rooms.timeline.get_pdu_json(&pdu.event_id).await?;
			let pdu_raw: Raw<AnySyncTimelineEvent> = Raw::new(&pdu_json)
				.expect("CanonicalJsonValue is valid Raw<...>")
				.cast();

			let actions = services
				.pusher
				.get_actions(
					sender_user,
					&ruleset,
					&power_levels,
					&pdu_raw,
					&room_id,
				)
				.await;

			let mut notify = false;

			for action in actions {
				if matches!(action, Action::Notify) {
					notify = true;
				}
			}

			if notify {
				let event: Raw<AnySyncTimelineEvent> = pdu_raw.clone();

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
