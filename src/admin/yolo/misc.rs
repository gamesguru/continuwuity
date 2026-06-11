use conduwuit::Result;
use ruma::OwnedRoomId;

use crate::admin_command;

#[admin_command]
pub(super) async fn clear_ratelimiter(&self) -> Result {
	self.bail_restricted()?;
	self.services.globals.bad_event_ratelimiter.write().clear();
	self.write_str("Cleared the global bad_event ratelimiter cache.")
		.await
}

#[admin_command]
pub(super) async fn check_read_receipts(&self, room_id: OwnedRoomId) -> Result {
	use futures::StreamExt;

	let receipts: Vec<_> = self
		.services
		.rooms
		.read_receipt
		.readreceipts_since(&room_id, None)
		.map(|(_, count, event)| format!("Count: {count}, Event: {:?}", event))
		.collect()
		.await;

	let msg = if receipts.is_empty() {
		"No read receipts found.".to_owned()
	} else {
		receipts.join("\n")
	};

	self.write_str(&msg).await
}

#[admin_command]
pub(super) async fn check_read_receipts_legacy(&self, room_id: OwnedRoomId) -> Result {
	use futures::StreamExt;
	use ruma::{UserId, events::receipt::ReceiptEvent};

	let db = &self.services.db;
	let old_map = db["readreceiptid_readreceipt"].clone();

	let mut stream =
		old_map.stream_raw_from::<(&ruma::RoomId, u64, &UserId), ReceiptEvent, _>(&[]);

	let mut msg = String::new();
	let mut found = false;
	while let Some(Ok(((room, count, user), event))) = stream.next().await {
		if room == room_id {
			found = true;
			msg.push_str(&format!(
				"Legacy Receipt -> Count: {count}, User: {user}, Event: {event:?}\n"
			));
		}
	}

	if !found {
		msg.push_str("No legacy read receipts found for this room.");
	}

	self.write_str(&msg).await
}

#[admin_command]
pub(super) async fn migrate_read_receipts(&self) -> Result {
	use conduwuit_database::Json;
	use futures::StreamExt;
	use ruma::{RoomId, UserId, events::receipt::ReceiptEvent};

	self.bail_restricted()?;

	let db = &self.services.db;
	let stream_index = db["readreceiptid_readreceipt"].clone();
	let state_map = db["roomuserid_readreceipt"].clone();

	let mut stream =
		stream_index.stream_raw_from::<(&RoomId, u64, &UserId), ReceiptEvent, _>(&[]);

	let mut total_migrated: usize = 0;

	self.write_str("Starting read receipt state map migration...")
		.await?;

	while let Some(Ok(((room_id, count, user_id), event))) = stream.next().await {
		// Populate the new state map using the exact historical count
		// Because the stream is ordered by count, later entries for the same user will
		// naturally overwrite older ones, leaving the state map with the absolute
		// latest count and event.
		let mut key = room_id.as_bytes().to_vec();
		key.push(conduwuit_database::SEP);
		key.extend_from_slice(user_id.as_bytes());

		state_map.put(key, Json((count, event)));

		total_migrated = total_migrated.saturating_add(1);

		if total_migrated.is_multiple_of(10000) {
			conduwuit::info!("Migrated {} read receipts to state map...", total_migrated);
		}
	}

	self.write_str(&format!(
		"Successfully migrated {total_migrated} read receipts into the new O(1) state map!"
	))
	.await
}

#[admin_command]
pub(super) async fn migrate_private_read_receipts(&self) -> Result {
	use conduwuit_database::Json;
	use futures::StreamExt;
	use ruma::{RoomId, UserId};

	self.bail_restricted()?;

	let db = &self.services.db;
	let legacy_count_map = db["roomuserid_privateread"].clone();
	let legacy_event_map = db["roomuserid_privatereadevent"].clone();
	let legacy_update_map = db["roomuserid_lastprivatereadupdate"].clone();
	let new_receipt_map = db["roomuserid_privatereadreceipt"].clone();

	// Stream through the legacy count map (it contains all users who have a private
	// read receipt)
	let mut stream = legacy_count_map.stream_raw_from::<(&RoomId, &UserId), [u8; 8], _>(&[]);

	let mut total_migrated: usize = 0;
	self.write_str("Starting private read receipt migration...")
		.await?;

	while let Some(Ok(((room_id, user_id), count_bytes))) = stream.next().await {
		let count = u64::from_be_bytes(count_bytes);

		let mut legacy_key = room_id.as_bytes().to_vec();
		legacy_key.push(0xFF);
		legacy_key.extend_from_slice(user_id.as_bytes());

		// Attempt to fetch the event and update count
		let event: ruma::events::receipt::ReceiptEvent =
			if let Ok(event_bytes) = legacy_event_map.get(&legacy_key).await {
				serde_json::from_slice(&event_bytes).unwrap_or_else(|_| {
					ruma::events::receipt::ReceiptEvent {
						content: ruma::events::receipt::ReceiptEventContent(
							std::collections::BTreeMap::new(),
						),
						room_id: room_id.to_owned(),
					}
				})
			} else {
				ruma::events::receipt::ReceiptEvent {
					content: ruma::events::receipt::ReceiptEventContent(
						std::collections::BTreeMap::new(),
					),
					room_id: room_id.to_owned(),
				}
			};

		let update_count = if let Ok(update_bytes) = legacy_update_map.get(&legacy_key).await {
			conduwuit::utils::u64_from_bytes(&update_bytes).unwrap_or(0)
		} else {
			0
		};

		let mut new_key = room_id.as_bytes().to_vec();
		new_key.push(conduwuit_database::SEP);
		new_key.extend_from_slice(user_id.as_bytes());

		new_receipt_map.put(new_key, Json((count, event, update_count)));
		total_migrated = total_migrated.saturating_add(1);

		if total_migrated.is_multiple_of(5000) {
			conduwuit::info!("Migrated {} private read receipts...", total_migrated);
		}
	}

	self.write_str(&format!(
		"Successfully migrated {total_migrated} private read receipts to new consolidated map"
	))
	.await
}
