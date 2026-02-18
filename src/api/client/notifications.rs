use axum::extract::State;
use conduwuit::{Event, Result, matrix::pdu::PduCount, warn};
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
	use std::{cmp::Reverse, collections::BinaryHeap, time::Instant};

	// Wrapper to order notifications by timestamp
	#[derive(Debug)]
	struct NotificationItem(r::Notification);

	impl PartialEq for NotificationItem {
		fn eq(&self, other: &Self) -> bool { self.0.ts == other.0.ts }
	}

	impl Eq for NotificationItem {}

	impl PartialOrd for NotificationItem {
		fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
			Some(self.cmp(other))
		}
	}

	impl Ord for NotificationItem {
		fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.0.ts.cmp(&other.0.ts) }
	}

	let started = Instant::now();

	// Extract the `limit` and `from` query parameters
	let limit = body.limit.unwrap_or_else(|| UInt::new(10).unwrap());
	let limit = std::cmp::min(limit, UInt::new(100).unwrap()); // Cap limit to 100 for safety
	let start_ts = body
		.from
		.as_ref()
		.and_then(|s| s.parse::<u64>().ok())
		.unwrap_or(u64::MAX);

	let sender_user = body.sender_user();

	// Min-heap to keep the top `limit` notifications (newest timestamps).
	// The top of the heap is the oldest of the newest notifications.
	let limit_usize = limit.try_into().unwrap_or(usize::MAX);
	let mut notifications: BinaryHeap<Reverse<NotificationItem>> =
		BinaryHeap::with_capacity(limit_usize);

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

		// Iterate over PDUs, reverse scan should be the fastest
		let mut pdus = std::pin::pin!(services.rooms.timeline.pdus_rev(&room_id, None));

		// optimization: we can stop once we have enough notifications and current pdu
		// is older than the oldest one in our list
		while let Some(Ok((pdu_count, pdu))) = pdus.next().await {
			if pdu_count <= PduCount::Normal(last_read) {
				break;
			}

			// Skip events strictly newer than our start_ts (pagination)
			if pdu.origin_server_ts >= UInt::new(start_ts).unwrap_or(UInt::MAX) {
				continue;
			}

			// Optimization: if we have enough notifications, check if this PDU is older
			// than the oldest one we have. If it is, then all subsequent PDUs in this
			// room will be even older, so we can skip the rest of the room.
			// We check this BEFORE the expensive push rule calculation.
			if notifications.len() >= limit_usize {
				if let Some(Reverse(oldest_kept)) = notifications.peek() {
					if pdu.origin_server_ts <= oldest_kept.0.ts.0 {
						break;
					}
				}
			}

			// Skip events sent by the user themselves
			if pdu.sender == *sender_user {
				continue;
			}

			// Check push rules to see if this event should notify
			let pdu_raw: Raw<AnySyncTimelineEvent> = pdu.to_format();

			let actions = services
				.pusher
				.get_actions(sender_user, &ruleset, &power_levels, &pdu_raw, &room_id)
				.await;

			let mut notify = false;

			for action in actions {
				if matches!(action, &Action::Notify) {
					notify = true;
				}
			}

			if notify {
				let event: Raw<AnySyncTimelineEvent> = pdu_raw;

				let notification_item = NotificationItem(r::Notification {
					actions: actions.to_vec(),
					event,
					profile_tag: None,
					read: false,
					room_id: room_id.clone(),
					ts: MilliSecondsSinceUnixEpoch(pdu.origin_server_ts),
				});

				if notifications.len() >= limit_usize {
					// We already checked if this is newer than the oldest kept above.
					// So we just pop the oldest before pushing this one.
					notifications.pop();
				}
				notifications.push(Reverse(notification_item));
			}
		}
	}

	// Capture heap stats before consuming
	let heap_count = notifications.len();
	let heap_bytes = size_of_val(notifications.as_slice());

	// Convert heap to vector and sort by timestamp descending (newest first)
	let mut notifications: Vec<_> = notifications
		.into_iter()
		.map(|Reverse(item)| item.0)
		.collect();
	notifications.sort_by(|a, b| b.ts.cmp(&a.ts));

	let next_token = if notifications.len() >= limit_usize {
		notifications.last().map(|n| n.ts.0.to_string())
	} else {
		None
	};

	let elapsed = started.elapsed();
	conduwuit::info!(
		"built notification heap: {} items for {} in {:.3}s (used {} bytes)",
		heap_count,
		sender_user,
		elapsed.as_secs_f64(),
		heap_bytes,
	);

	Ok(get_notifications::v3::Response { next_token, notifications })
}
