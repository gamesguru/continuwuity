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

use crate::{
	Ruma,
	client::message::{ignored_filter, visibility_filter},
};

/// Wrapper to order notifications by timestamp for the min-heap.
#[derive(Debug)]
struct NotificationItem {
	notification: notif_route_v3::Notification,
	pdu_count: PduCount,
}

impl PartialEq for NotificationItem {
	fn eq(&self, other: &Self) -> bool {
		self.notification.ts == other.notification.ts && self.pdu_count == other.pdu_count
	}
}

impl Eq for NotificationItem {}

impl PartialOrd for NotificationItem {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Ord for NotificationItem {
	fn cmp(&self, other: &Self) -> Ordering {
		match self.notification.ts.cmp(&other.notification.ts) {
			| Ordering::Equal => self.pdu_count.cmp(&other.pdu_count),
			| other => other,
		}
	}
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
	let max_limit_uint = UInt::try_from(max_limit).unwrap_or(UInt::MAX);
	let limit = std::cmp::min(limit, max_limit_uint);
	let (start_ts, start_pdu_count) = body.from.as_deref().map_or((u64::MAX, None), |s| {
		let mut parts = s.split(':');
		let ts = parts
			.next()
			.and_then(|ts| ts.parse::<u64>().ok())
			.unwrap_or(u64::MAX);
		let pdu_count = parts.next().and_then(|p| {
			if let Some(c) = p.strip_prefix('n') {
				if let Ok(c) = c.parse::<u64>() {
					return Some(PduCount::Normal(c));
				}
			} else if let Some(c) = p.strip_prefix('b') {
				if let Ok(c) = c.parse::<i64>() {
					return Some(PduCount::Backfilled(c));
				}
			}
			None
		});
		(ts, pdu_count)
	});
	let start_ts_uint = UInt::new(start_ts).unwrap_or(UInt::MAX);

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

	while let Some(result) = rooms_stream.next().await {
		let (room_id, count) = match result {
			| Ok((room_id, count)) => (room_id, count),
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
			return Err!(Request(NotFound("Notifications are disabled.")));
		}
		let mut pdus = std::pin::pin!(services.rooms.timeline.pdus_rev(&room_id, None));
		let mut scanned: usize = 0;

		// optimization: stop if we have enough notifications and current pdu
		// is older than any in our list
		loop {
			let (pdu_count, pdu) = match pdus.next().await {
				| Some(Ok((pdu_count, pdu))) => (pdu_count, pdu),
				| Some(Err(error)) => {
					warn!(
						"Failed to read notification PDU while scanning room {room_id}: {error}"
					);
					continue;
				},
				| None => break,
			};
			scanned = scanned.saturating_add(1);
			if scanned > max_pdus_per_room || pdu_count <= PduCount::Normal(last_read) {
				break;
			}

			// Skip events sent by our user
			if pdu.sender == *sender_user {
				continue;
			}

			// Skip events newer than or equal to start_ts/start_pdu_count
			let pdu_ts = pdu.origin_server_ts;

			if pdu_ts > start_ts_uint {
				continue;
			}
			if pdu_ts == start_ts_uint {
				if let Some(start_pdu) = start_pdu_count {
					if pdu_count >= start_pdu {
						continue;
					}
				} else {
					// Fallback for old tokens: skip all events with exactly start_ts
					continue;
				}
			}

			let item = (pdu_count, pdu);
			let Some(item) = visibility_filter(&services, item, sender_user).await else {
				continue;
			};
			let Some((_, pdu)) = ignored_filter(&services, item, sender_user).await else {
				continue;
			};

			// Check push rules to see if this event should notify
			let pdu_raw: Raw<AnySyncTimelineEvent> = pdu.to_format();

			let actions = services
				.pusher
				.get_actions(sender_user, &ruleset, &power_levels, &pdu_raw, &room_id)
				.await;

			let is_highlight = actions.iter().any(Action::is_highlight);
			let is_notify = actions.iter().any(|a| matches!(a, Action::Notify));

			if !is_notify {
				continue;
			}

			if body.only.as_deref() == Some("highlight") && !is_highlight {
				continue;
			}

			let event: Raw<AnySyncTimelineEvent> = pdu_raw;

			// Prepare each item, carrying `pdu_count` for stable ordering/pagination
			let notification_item = NotificationItem {
				notification: notif_route_v3::Notification {
					actions: actions.to_vec(),
					event,
					profile_tag: None,
					read: false,
					room_id: room_id.clone(),
					ts: MilliSecondsSinceUnixEpoch(pdu.origin_server_ts),
				},
				pdu_count,
			};

			if notifications.len() >= limit_usize {
				// Heap is full; only evict the current oldest item if this one is newer.
				if let Some(Reverse(oldest)) = notifications.peek() {
					if notification_item.cmp(oldest) <= Ordering::Equal {
						continue;
					}
				}
				notifications.pop();
			}
			notifications.push(Reverse(notification_item));
		}
	}

	// Convert heap to vector and sort by timestamp descending (newest first),
	// using `pdu_count` as a tie-breaker for stable ordering.
	let mut notification_items: Vec<_> = notifications
		.into_iter()
		.map(|Reverse(item)| item)
		.collect();

	notification_items.sort_by(|a, b| match b.notification.ts.cmp(&a.notification.ts) {
		| Ordering::Equal => b.pdu_count.cmp(&a.pdu_count),
		| other => other,
	});

	let next_token = if notification_items.len() >= limit_usize {
		notification_items.last().map(|n| match n.pdu_count {
			| PduCount::Normal(c) => format!("{}:n{}", n.notification.ts.0, c),
			| PduCount::Backfilled(c) => format!("{}:b{}", n.notification.ts.0, c),
		})
	} else {
		None
	};

	let notifications = notification_items
		.into_iter()
		.map(|item| item.notification)
		.collect();

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

		NotificationItem {
			notification: notif_route_v3::Notification {
				actions: Vec::new(),
				event,
				profile_tag: None,
				read: false,
				room_id: OwnedRoomId::try_from("!test:example.com").unwrap(),
				ts: MilliSecondsSinceUnixEpoch(UInt::new(ts_millis).unwrap()),
			},
			pdu_count: conduwuit::matrix::pdu::PduCount::Normal(0),
		}
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
				let ts: u64 = item.notification.ts.0.into();
				ts
			})
			.collect();

		// Reversing output back to chronological
		timestamps.reverse();

		assert_eq!(timestamps, vec![2000, 3000]);
	}

	#[test]
	fn token_parsing_roundtrip() {
		use conduwuit::matrix::pdu::PduCount;

		let ts = 1_234_567_890;
		let count_n = PduCount::Normal(10);
		let count_b = PduCount::Backfilled(-5);

		// Helper to format as our token
		let token_n = format!("{}:n10", ts);
		let token_b = format!("{}:b-5", ts);

		// Parse back
		let parse = |s: &str| {
			let mut parts = s.split(':');
			let ts = parts
				.next()
				.and_then(|ts| ts.parse::<u64>().ok())
				.unwrap_or(u64::MAX);
			let pdu_count = parts.next().and_then(|p| {
				if let Some(c) = p.strip_prefix('n') {
					if let Ok(c) = c.parse::<u64>() {
						return Some(PduCount::Normal(c));
					}
				} else if let Some(c) = p.strip_prefix('b') {
					if let Ok(c) = c.parse::<i64>() {
						return Some(PduCount::Backfilled(c));
					}
				}
				None
			});
			(ts, pdu_count)
		};

		assert_eq!(parse(&token_n), (ts, Some(count_n)));
		assert_eq!(parse(&token_b), (ts, Some(count_b)));
	}
}
