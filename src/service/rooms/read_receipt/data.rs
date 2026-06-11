use std::sync::Arc;

use conduwuit::{
	Result,
	utils::{ReadyExt, stream::TryIgnore},
};
use database::{Deserialized, Json, Map};
use futures::{Stream, StreamExt};
use ruma::{
	CanonicalJsonObject, OwnedUserId, RoomId, UserId,
	events::{
		AnySyncEphemeralRoomEvent,
		receipt::{ReceiptEvent, ReceiptType},
	},
	serde::Raw,
};

use crate::{Dep, globals};

pub(super) struct Data {
	roomuserid_privateread: Arc<Map>,
	roomuserid_privatereadevent: Arc<Map>,
	roomuserid_lastprivatereadupdate: Arc<Map>,
	roomuserid_readreceipt: Arc<Map>,
	services: Services,
	readreceiptid_readreceipt: Arc<Map>,
}

struct Services {
	globals: Dep<globals::Service>,
	timeline: Dep<crate::rooms::timeline::Service>,
	short: Dep<crate::rooms::short::Service>,
}

pub(super) type ReceiptItem = (OwnedUserId, u64, Raw<AnySyncEphemeralRoomEvent>);

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			roomuserid_privateread: db["roomuserid_privateread"].clone(),
			roomuserid_privatereadevent: db["roomuserid_privatereadevent"].clone(),
			roomuserid_lastprivatereadupdate: db["roomuserid_lastprivatereadupdate"].clone(),
			roomuserid_readreceipt: db["roomuserid_readreceipt"].clone(),
			readreceiptid_readreceipt: db["readreceiptid_readreceipt"].clone(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				timeline: args.depend::<crate::rooms::timeline::Service>("rooms::timeline"),
				short: args.depend::<crate::rooms::short::Service>("rooms::short"),
			},
		}
	}

	/// Returns the user's current public read receipt event ID for the given
	/// thread.
	pub(super) async fn readreceipt_get(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		target_thread: Option<&ruma::events::receipt::ReceiptThread>,
	) -> Option<ruma::OwnedEventId> {
		let key = roomuserid_key(room_id, user_id);
		let value = self.roomuserid_readreceipt.get(&key).await.ok()?;
		let (_, receipt_event): (u64, ReceiptEvent) = serde_json::from_slice(&value).ok()?;

		for (event_id, receipts) in receipt_event.content.0 {
			if let Some(users) = receipts.get(&ReceiptType::Read) {
				if let Some(receipt) = users.get(user_id) {
					if Some(&receipt.thread) == target_thread {
						return Some(event_id);
					}
				}
			}
		}

		// Fallback for pre-migration data
		let last_possible_key = (room_id, u64::MAX);
		self.readreceiptid_readreceipt
			.rev_stream_from_raw(&last_possible_key)
			.ignore_err()
			.ready_take_while(|(key, _)| {
				key.starts_with(room_id.as_bytes())
					&& key.get(room_id.as_bytes().len()) == Some(&database::SEP)
			})
			.ready_filter_map(|(key, value)| {
				let user_id_bytes = user_id.as_bytes();
				if key.ends_with(user_id_bytes)
					&& key
						.len()
						.checked_sub(user_id_bytes.len())
						.and_then(|len| len.checked_sub(1))
						.and_then(|idx| key.get(idx))
						== Some(&database::SEP)
				{
					let receipt = serde_json::from_slice::<ReceiptEvent>(value).ok()?;
					let (event_id, types) = receipt.content.0.into_iter().next()?;
					let users = types.get(&ReceiptType::Read)?;
					let receipt_data = users.get(user_id)?;

					if Some(&receipt_data.thread) == target_thread {
						return Some(event_id);
					}
				}
				None
			})
			.next()
			.await
	}

	pub(super) async fn private_read_get(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<Option<(u64, ReceiptEvent)>> {
		let mut key = room_id.as_bytes().to_vec();
		key.push(0xFF);
		key.extend_from_slice(user_id.as_bytes());

		let count = self
			.roomuserid_privateread
			.get(&key)
			.await
			.map(|bytes| {
				conduwuit::utils::u64_from_bytes(&bytes).expect("bytes have right length")
			})
			.ok();

		let Some(count) = count else {
			return Ok(None);
		};

		// Fast path: try to get the full JSON event
		if let Ok(handle) = self.roomuserid_privatereadevent.get(&key).await {
			if let Ok(event) = handle.deserialized() {
				return Ok(Some((count, event)));
			}
		}

		// Fallback for legacy private read receipts that were only saved as a u64 count
		let mut user_map = std::collections::BTreeMap::new();
		user_map.insert(user_id.to_owned(), ruma::events::receipt::Receipt {
			thread: ruma::events::receipt::ReceiptThread::Unthreaded,
			ts: None, // Legacy receipts have no timestamp
		});

		let shortroomid = self.services.short.get_shortroomid(room_id).await?;
		let shorteventid = conduwuit::matrix::pdu::PduCount::Normal(count);
		let pdu_id: conduwuit::matrix::pdu::RawPduId =
			conduwuit::matrix::pdu::PduId { shortroomid, shorteventid }.into();
		let pdu = self.services.timeline.get_pdu_from_id(&pdu_id).await?;
		let event_id = pdu.event_id;

		let mut receipt_map = std::collections::BTreeMap::new();
		receipt_map.insert(ReceiptType::ReadPrivate, user_map);
		let mut content = std::collections::BTreeMap::new();
		content.insert(event_id, receipt_map);

		let receipt_sync_event = ruma::events::SyncEphemeralRoomEvent {
			content: ruma::events::receipt::ReceiptEventContent(content),
		};

		// We cast it back to ReceiptEvent because pack_receipts takes an iterator of
		// AnySyncEphemeralRoomEvent
		let event: ReceiptEvent = serde_json::from_str(
			serde_json::to_string(&receipt_sync_event)
				.expect("receipt created manually")
				.as_str(),
		)?;

		Ok(Some((count, event)))
	}

	pub(super) async fn readreceipt_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
	) {
		let mut new_receipts = Vec::new();
		for (event_id, receipts) in &event.content.0 {
			for (receipt_type, users) in receipts {
				if let Some(receipt) = users.get(user_id) {
					new_receipts.push((event_id.clone(), receipt_type.clone(), receipt.clone()));
				}
			}
		}

		if new_receipts.is_empty() {
			return;
		}

		let key = roomuserid_key(room_id, user_id);

		// Get existing receipts for this user in this room
		let mut existing_event = if let Ok(value) = self.roomuserid_readreceipt.get(&key).await {
			if let Ok((_, ev)) = serde_json::from_slice::<(u64, ReceiptEvent)>(&value) {
				ev
			} else {
				ReceiptEvent {
					content: ruma::events::receipt::ReceiptEventContent(
						std::collections::BTreeMap::new(),
					),
					room_id: room_id.to_owned(),
				}
			}
		} else {
			ReceiptEvent {
				content: ruma::events::receipt::ReceiptEventContent(
					std::collections::BTreeMap::new(),
				),
				room_id: room_id.to_owned(),
			}
		};

		// Remove old receipts for the same thread and type
		for (_, new_type, new_receipt) in &new_receipts {
			let mut empty_event_ids = Vec::new();
			for (event_id, receipts) in &mut existing_event.content.0 {
				if let Some(users) = receipts.get_mut(new_type) {
					if let Some(existing_receipt) = users.get(user_id) {
						if existing_receipt.thread == new_receipt.thread {
							users.remove(user_id);
						}
					}
					if users.is_empty() {
						receipts.remove(new_type);
					}
				}
				if receipts.is_empty() {
					empty_event_ids.push(event_id.clone());
				}
			}
			for event_id in empty_event_ids {
				existing_event.content.0.remove(&event_id);
			}
		}

		// Insert new receipts
		for (new_event_id, new_type, new_receipt) in new_receipts {
			existing_event
				.content
				.0
				.entry(new_event_id)
				.or_default()
				.entry(new_type)
				.or_default()
				.insert(user_id.to_owned(), new_receipt);
		}

		let count = self.services.globals.next_count().unwrap();

		conduwuit::trace!(
			?room_id,
			?user_id,
			?count,
			"Inserting new read receipt into roomuserid_readreceipt map"
		);
		self.roomuserid_readreceipt
			.put(key, Json((count, existing_event)));
	}

	pub(super) fn readreceipts_since<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
	) -> impl Stream<Item = ReceiptItem> + Send + 'a {
		// New format: roomuserid_readreceipt
		let mut prefix = room_id.as_bytes().to_vec();
		prefix.push(database::SEP);

		// Legacy support during migration: also fetch from old map for keys not in new
		// map But actually, we don't need legacy support here because we will provide
		// a migrate command and the user already requested a migrate command!
		// However, it's safer to stream both, but that requires merging or just
		// returning both. `pack_receipts` will handle deduplication if there are
		// multiples. For now, let's just use the new stream! The migrate command will
		// migrate everything.

		self.roomuserid_readreceipt
			.raw_stream_from(&prefix)
			.ignore_err()
			.ready_take_while(move |(key, _): &(&[u8], &[u8])| key.starts_with(&prefix))
			.map(move |(key, value): (&[u8], &[u8])| {
				// Parse the user_id from the key (RoomId, UserId)
				let room_id_bytes = room_id.as_bytes();
				// key structure is room_id + SEP + user_id
				if key.len() <= room_id_bytes.len().saturating_add(1)
					|| key[room_id_bytes.len()] != database::SEP
				{
					return Err(conduwuit::Error::bad_database(
						"Invalid roomuserid_readreceipt key",
					));
				}
				let user_id_bytes = &key[room_id_bytes.len().saturating_add(1)..];
				let user_id_str = conduwuit::utils::str_from_bytes(user_id_bytes)?;
				let user_id = <&UserId>::try_from(user_id_str)
					.map_err(|_| conduwuit::Error::bad_database("Invalid user ID"))?
					.to_owned();

				let (count, json): (u64, CanonicalJsonObject) = serde_json::from_slice(value)?;

				if count > since {
					let event = serde_json::value::to_raw_value(&json)?;

					conduwuit::trace!(
						"Yielding read receipt for user {} at count {} (since was {})",
						user_id,
						count,
						since
					);

					Ok((user_id, count, Raw::from_json(event)))
				} else {
					Err(conduwuit::Error::bad_database("Count below since parameter"))
				}
			})
			.ignore_err()
	}

	pub(super) fn private_read_set(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		count: u64,
		receipt: &ReceiptEvent,
	) -> Result<()> {
		let mut key = room_id.as_bytes().to_vec();
		key.push(0xFF);
		key.extend_from_slice(user_id.as_bytes());
		let next_count = self.services.globals.next_count()?;

		let receipt_json = serde_json::to_vec(receipt).expect("ReceiptEvent serializes");

		self.roomuserid_privateread
			.insert(&key, count.to_be_bytes());
		self.roomuserid_privatereadevent.insert(&key, &receipt_json);
		self.roomuserid_lastprivatereadupdate
			.insert(&key, next_count.to_be_bytes());

		Ok(())
	}

	pub(super) async fn private_read_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<u64> {
		let key = (room_id, user_id);
		self.roomuserid_privateread.qry(&key).await.deserialized()
	}

	pub(super) async fn last_privateread_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
	) -> u64 {
		let key = (room_id, user_id);
		self.roomuserid_lastprivatereadupdate
			.qry(&key)
			.await
			.deserialized()
			.unwrap_or(0)
	}
}

#[inline]
fn roomuserid_key(room_id: &RoomId, user_id: &UserId) -> Vec<u8> {
	let mut key = room_id.as_bytes().to_vec();
	key.push(database::SEP);
	key.extend_from_slice(user_id.as_bytes());
	key
}
