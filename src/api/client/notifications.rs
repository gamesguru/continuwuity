use std::cmp::{Ordering, Reverse};

use axum::extract::State;
use conduwuit::{Err, Event, Result, matrix::pdu::PduCount, warn};
use futures::StreamExt;
use ruma::{
	MilliSecondsSinceUnixEpoch, UInt,
	api::client::push::{get_notifications, get_notifications::v3 as notif_route_v3},
	events::{
		AnySyncTimelineEvent, GlobalAccountDataEventType, StateEventType,
		push_rules::PushRulesEvent, room::power_levels::RoomPowerLevelsEventContent,
	},
	push::{Action, Ruleset},
	serde::Raw,
};

use crate::Ruma;

/// Wrapper to order notifications by timestamp for the min-heap.
#[derive(Debug)]
struct NotificationItem(notif_route_v3::Notification);

impl PartialEq for NotificationItem {
	fn eq(&self, other: &Self) -> bool { self.0.ts == other.0.ts }
}

impl Eq for NotificationItem {}

impl PartialOrd for NotificationItem {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Ord for NotificationItem {
	fn cmp(&self, other: &Self) -> Ordering { self.0.ts.cmp(&other.0.ts) }
}

/// # `GET /_matrix/client/v3/notifications`
///
/// Get notifications for the user.
///
/// Returns list of notifications based on user push rules & room history.
pub(crate) async fn get_notifications_route(
	State(services): State<crate::State>,
	body: Ruma<get_notifications::v3::Request>,
) -> Result<get_notifications::v3::Response> {
	use std::collections::BinaryHeap;

	let max_limit = services.server.config.notification_max_limit_per_request;

	// 0 = disabled
	if max_limit == 0 {
		return Err!(Request(NotFound("Notification endpoint is disabled.")));
	}

	let limit = body.limit.unwrap_or_else(|| UInt::new(10).unwrap());
	let limit = std::cmp::min(limit, UInt::try_from(max_limit).unwrap());
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

		// Clean up and skip rooms the user left (stale notification counts can linger)
		if !services
			.rooms
			.state_cache
			.is_joined(sender_user, &room_id)
			.await
		{
			services
				.rooms
				.user
				.reset_notification_counts(sender_user, &room_id);

			continue;
		}

		// Get the last read receipt for current room (as PDU count)
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

		// Iterate over PDUs, cap per-room scan depth to prevent overload
		let max_pdus_per_room = services.server.config.notification_max_pdus_per_room;
		// A value of 0 disables notifications; avoid unbounded per-room scans.
		if max_pdus_per_room == 0 {
			continue;
		}
		let mut pdus = std::pin::pin!(services.rooms.timeline.pdus_rev(&room_id, None));
		let mut scanned: usize = 0;

		// optimization: stop if we have enough notifications and current pdu
		// is older than any in our list
		while let Some(Ok((pdu_count, pdu))) = pdus.next().await {
			scanned = scanned.saturating_add(1);
			if scanned > max_pdus_per_room
				|| pdu_count <= PduCount::Normal(last_read)
			{
				break;
			}

			// Skip events newer than start_ts or sent by our user
			if pdu.origin_server_ts >= UInt::new(start_ts).unwrap_or(UInt::MAX)
				|| pdu.sender == *sender_user
			{
				continue;
			}

			// Check push rules to see if this event should notify
			let pdu_raw: Raw<AnySyncTimelineEvent> = pdu.to_format();

			let actions = services
				.pusher
				.get_actions(sender_user, &ruleset, &power_levels, &pdu_raw, &room_id)
				.await;

			// Look for notifications
			if actions
				.iter()
				.any(|action| matches!(action, &Action::Notify))
			{
				let event: Raw<AnySyncTimelineEvent> = pdu_raw;

				// Prepare each item
				let notification_item = NotificationItem(notif_route_v3::Notification {
					actions: actions.to_vec(),
					event,
					profile_tag: None,
					read: false,
					room_id: room_id.clone(),
					ts: MilliSecondsSinceUnixEpoch(pdu.origin_server_ts),
				});

				if notifications.len() >= limit_usize {
					// Heap is full; evict oldest to make room for this newer one
					notifications.pop();
				}
				notifications.push(Reverse(notification_item));
			}
		}
	}

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

	Ok(get_notifications::v3::Response { next_token, notifications })
}

/// Unit tests for notification endpoint and supporting functions

#[cfg(test)]
mod tests {
	use std::cmp::Reverse;

	use ruma::{
		MilliSecondsSinceUnixEpoch, OwnedRoomId, UInt,
		api::client::push::get_notifications::v3 as notif_route_v3, events::AnySyncTimelineEvent,
		serde::Raw,
	};

	use super::NotificationItem;

	fn make_item(ts_millis: u64) -> NotificationItem {
		// Minimally viable timeline event (m.room.message)
		let json = serde_json::json!({
			"type": "m.room.message",
			"content": {"msgtype": "m.text", "body": "test"},
			"sender": "@alice:example.com",
			"event_id": "$test",
			"origin_server_ts": ts_millis,
		});

		let event: Raw<AnySyncTimelineEvent> =
			Raw::from_json(serde_json::value::to_raw_value(&json).unwrap());

		NotificationItem(notif_route_v3::Notification {
			actions: Vec::new(),
			event,
			profile_tag: None,
			read: false,
			room_id: OwnedRoomId::try_from("!test:example.com").unwrap(),
			ts: MilliSecondsSinceUnixEpoch(UInt::new(ts_millis).unwrap()),
		})
	}

	#[test]
	fn ordering_newer_is_greater() {
		let older = make_item(1000);
		let newer = make_item(2000);
		assert!(newer > older);
	}

	#[test]
	fn ordering_equal_timestamps() {
		let a = make_item(5000);
		let b = make_item(5000);
		assert_eq!(a, b);
	}

	#[test]
	fn min_heap_evicts_oldest() {
		use std::collections::BinaryHeap;

		let mut heap: BinaryHeap<Reverse<NotificationItem>> = BinaryHeap::new();
		let limit = 2;

		// Push three items; evict when at capacity
		for ts in [1000, 3000, 2000] {
			let item = make_item(ts);
			if heap.len() >= limit {
				heap.pop(); // evict oldest (smallest ts)
			}
			heap.push(Reverse(item));
		}

		// Heap should contain two newest: 2000 and 3000
		let mut timestamps: Vec<u64> = heap
			.into_sorted_vec()
			.into_iter()
			.map(|Reverse(item)| {
				let ts: u64 = item.0.ts.0.into();
				ts
			})
			.collect();

		// Reversing output back to chronological
		timestamps.reverse();

		assert_eq!(timestamps, vec![2000, 3000]);
	}
}
