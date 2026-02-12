use axum::extract::State;
use conduwuit::{Result, matrix::pdu::PduCount, warn};
use futures::StreamExt;
use ruma::{
	MilliSecondsSinceUnixEpoch, UInt,
	api::client::push::{get_notifications, get_notifications::v3 as r},
	events::{
		AnySyncTimelineEvent, GlobalAccountDataEventType, StateEventType,
		push_rules::PushRulesEvent, room::power_levels::RoomPowerLevelsEventContent,
	},
	push::{Action, Ruleset},
	serde::Raw,
};

use crate::Ruma;

/// # `GET /_matrix/client/v3/notifications`
///
/// Get notifications for the user.
///
/// Returns list of notifications based on user push rules & room history.
pub(crate) async fn get_notifications_route(
	State(services): State<crate::State>,
	body: Ruma<get_notifications::v3::Request>,
) -> Result<get_notifications::v3::Response> {
	// Extract the `limit` and `from` query parameters
	let limit = body.limit.unwrap_or_else(|| UInt::new(10).unwrap());
	let start_ts = body
		.from
		.as_ref()
		.and_then(|s| s.parse::<u64>().ok())
		.unwrap_or(u64::MAX);

	let sender_user = body.sender_user();

	let mut notifications = Vec::new();

	// Get user's push rules
	let global_account_data = services
		.account_data
		.get_global(sender_user, GlobalAccountDataEventType::PushRules)
		.await;

	let ruleset = global_account_data.map_or_else(
		|_| Ruleset::server_default(sender_user),
		|ev: PushRulesEvent| ev.content.global,
	);

	// iterate over all rooms where the user has a notification count
	let mut rooms_stream =
		std::pin::pin!(services.rooms.user.stream_notification_counts(sender_user));

	while let Some((room_id, count)) = rooms_stream.next().await {
		let room_id = match room_id {
			| Ok(room_id) => room_id,
			| Err(e) => {
				warn!("Failed to get room_id from notification stream: {e}");
				continue;
			},
		};

		// Skip rooms with no notifications
		if count == 0 {
			continue;
		}

		// Skip rooms the user is no longer joined to (stale notification
		// counts can persist after leaving a room)
		if !services
			.rooms
			.state_cache
			.is_joined(sender_user, &room_id)
			.await
		{
			continue;
		}

		// Get the last read receipt for this room (as PDU count)
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

		// Iterate over PDUs in the room *after* the last read receipt
		// Using forward scan guarantees we find all unread notifications
		let mut pdus = std::pin::pin!(
			services
				.rooms
				.timeline
				.pdus(&room_id, Some(PduCount::Normal(last_read)))
		);

		while let Some(Ok((_pdu_count, pdu))) = pdus.next().await {
			// Skip events strictly newer than our start_ts (pagination)
			// (Note: since we scan forward, it's sub-optimal but safe enough)
			if pdu.origin_server_ts >= UInt::new(start_ts).unwrap_or(UInt::MAX) {
				continue;
			}

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
				.get_actions(sender_user, &ruleset, &power_levels, &pdu_raw, &room_id)
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
					read: false,
					room_id: room_id.clone(),
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

	Ok(get_notifications::v3::Response {
		next_token: None, // TODO, but not vital apparently
		notifications: limited_notifications,
	})
}
