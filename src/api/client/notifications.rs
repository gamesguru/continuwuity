use axum::extract::State;
use conduwuit::{Result, matrix::pdu::PduCount};
use futures::StreamExt;
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::push::{get_notifications, get_notifications::v3 as r},
	events::{
		AnySyncTimelineEvent, GlobalAccountDataEventType, StateEventType,
		push_rules::PushRulesEvent, room::power_levels::RoomPowerLevelsEventContent,
	},
	push::{Action, Ruleset},
	serde::Raw,
	uint,
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
	let limit = body.limit.unwrap_or_else(|| uint!(10));

	let sender_user = body.sender_user();

	let mut notifications = Vec::new();

	// iterate over all joined rooms to catch read notifications (history) too
	let mut rooms_stream = std::pin::pin!(services.rooms.state_cache.rooms_joined(sender_user));

	while let Some(room_id) = rooms_stream.next().await {
		let room_id = room_id.to_owned();

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

		// Get user's push rules
		let global_account_data = services
			.account_data
			.get_global(sender_user, GlobalAccountDataEventType::PushRules)
			.await;

		let ruleset = global_account_data.map_or_else(
			|_| Ruleset::server_default(sender_user),
			|ev: PushRulesEvent| ev.content.global,
		);

		// Iterate backwards over PDUs using pdus_rev to find the newest updates first
		let mut pdus = std::pin::pin!(services.rooms.timeline.pdus_rev(&room_id, None));

		// Search depth (iterations) to prevent checking too far back in history
		// Synapse default is flexible, but we need a hard limit to avoid slow responses
		// checking 50 events per room seems reasonable for recent notifications
		let search_limit = 50;
		let mut iterations = 0;

		while let Some(Ok((pdu_count, pdu))) = pdus.next().await {
			iterations += 1;
			if iterations > search_limit {
				break;
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

				// Determine read status
				let read = if let PduCount::Normal(c) = pdu_count {
					c <= last_read
				} else {
					false
				};

				// Construct the Notification object
				notifications.push(r::Notification {
					actions: actions.to_vec(),
					event,
					profile_tag: None, // TODO
					read,
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

	// TODO: implement pagination token (next_token)
	// For now we return None, which means the client might re-request if they
	// scroll? But since this is "unread" focus, it might be fine.

	Ok(get_notifications::v3::Response {
		next_token: None,
		notifications: limited_notifications,
	})
}
