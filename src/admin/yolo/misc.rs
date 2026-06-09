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
		.map(|(_, _, v)| v)
		.collect::<Vec<_>>()
		.await;

	let packed = conduwuit_service::rooms::read_receipt::pack_receipts(receipts.into_iter());
	let json = packed.json().get();

	self.write_str(&format!("Pack Receipts Output:\n```json\n{json}\n```"))
		.await
}

#[admin_command]
pub(super) async fn check_read_receipts_legacy(&self, room_id: OwnedRoomId) -> Result {
	use std::collections::BTreeMap;

	use futures::StreamExt;

	self.bail_restricted()?;
	let mut stream = self
		.services
		.rooms
		.read_receipt
		.readreceipts_since(&room_id, Some(0));

	let mut user_counts: BTreeMap<_, usize> = BTreeMap::new();
	let mut total_receipts: usize = 0;

	while let Some((user_id, _count, _event_raw)) = stream.next().await {
		total_receipts = total_receipts.saturating_add(1);
		user_counts
			.entry(user_id.clone())
			.and_modify(|c| *c = c.saturating_add(1))
			.or_insert(1);
	}

	let mut msg = format!(
		"Checked read receipts for room {room_id}\nTotal receipt items: {total_receipts}\n"
	);
	let duplicates: Vec<_> = user_counts.iter().filter(|(_, c)| **c > 1).collect();

	if duplicates.is_empty() {
		msg.push_str("No duplicate read receipts found.");
	} else {
		use std::fmt::Write as _;
		writeln!(msg, "Found {} users with duplicate read receipts!", duplicates.len()).unwrap();
		for (user, count) in duplicates.iter().take(10) {
			writeln!(msg, "- {user}: {count} receipts").unwrap();
		}
	}

	self.write_str(&msg).await
}
