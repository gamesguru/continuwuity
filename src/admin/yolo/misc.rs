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
	use futures::StreamExt;
	use ruma::{RoomId, UserId, events::receipt::ReceiptEvent};

	self.bail_restricted()?;

	let db = &self.services.db;
	let old_map = db["readreceiptid_readreceipt"].clone();

	let mut stream = old_map.stream_raw_from::<(&RoomId, u64, &UserId), ReceiptEvent, _>(&[]);

	let mut total_migrated: usize = 0;

	self.write_str("Starting read receipt migration...").await?;

	while let Some(Ok(((room_id, _count, user_id), event))) = stream.next().await {
		// Merge it using the update logic
		self.services
			.rooms
			.read_receipt
			.readreceipt_update(user_id, room_id, &event)
			.await;
		total_migrated = total_migrated.saturating_add(1);

		if total_migrated.is_multiple_of(10000) {
			conduwuit::info!("Migrated {} read receipts...", total_migrated);
		}
	}

	self.write_str(&format!(
		"Successfully migrated {total_migrated} read receipts to the new O(1) database map!"
	))
	.await
}
