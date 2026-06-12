use std::sync::Arc;

use conduwuit::{
	Result,
	matrix::pdu::{PduCount, PduId, RawPduId},
	utils::{ReadyExt, stream::TryIgnore},
};
use database::{Deserialized, Json, Map};
use futures::{Stream, StreamExt};
use ruma::{
	CanonicalJsonObject, OwnedUserId, RoomId, UserId,
	events::{
		AnySyncEphemeralRoomEvent,
		receipt::{Receipt, ReceiptEvent, ReceiptThread, ReceiptType},
	},
	serde::Raw,
};

use crate::{Dep, globals};

pub(super) struct Data {
	roomuserid_privateread: Arc<Map>,
	roomuserid_privatereadevent: Arc<Map>,
	roomuserid_lastprivatereadupdate: Arc<Map>,
	roomuserid_privatereadreceipt: Arc<Map>,
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
			roomuserid_privatereadreceipt: db["roomuserid_privatereadreceipt"].clone(),
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
		target_thread: Option<&ReceiptThread>,
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
		let key = roomuserid_key(room_id, user_id);

		// Try the new consolidated map first
		if let Ok(value) = self.roomuserid_privatereadreceipt.get(&key).await {
			if let Ok((count, event, _update_count)) =
				serde_json::from_slice::<(u64, ReceiptEvent, u64)>(&value)
			{
				return Ok(Some((count, event)));
			}
		}

		// Fallback to legacy map
		let mut legacy_key = room_id.as_bytes().to_vec();
		legacy_key.push(0xFF);
		legacy_key.extend_from_slice(user_id.as_bytes());

		let count = self
			.roomuserid_privateread
			.get(&legacy_key)
			.await
			.map(|bytes| {
				conduwuit::utils::u64_from_bytes(&bytes).expect("bytes have right length")
			})
			.ok();

		let Some(count) = count else {
			return Ok(None);
		};

		// Fast path: try to get the full JSON event
		if let Ok(handle) = self.roomuserid_privatereadevent.get(&legacy_key).await {
			if let Ok(event) = handle.deserialized() {
				return Ok(Some((count, event)));
			}
		}

		// Fallback for legacy private read receipts that were only saved as a u64 count
		let mut user_map = std::collections::BTreeMap::new();
		user_map.insert(user_id.to_owned(), Receipt {
			thread: ReceiptThread::Unthreaded,
			ts: None, // Legacy receipts have no timestamp
		});

		let shortroomid = self.services.short.get_shortroomid(room_id).await?;
		let shorteventid = PduCount::Normal(count);
		let pdu_id: RawPduId = PduId { shortroomid, shorteventid }.into();
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

		// Get existing receipts for this user in this room to find old_count
		let (old_count, mut existing_event) =
			if let Ok(value) = self.roomuserid_readreceipt.get(&key).await {
				if let Ok((old_c, ev)) = serde_json::from_slice::<(u64, ReceiptEvent)>(&value) {
					(Some(old_c), ev)
				} else {
					(None, ReceiptEvent {
						content: ruma::events::receipt::ReceiptEventContent(
							std::collections::BTreeMap::new(),
						),
						room_id: room_id.to_owned(),
					})
				}
			} else {
				(None, ReceiptEvent {
					content: ruma::events::receipt::ReceiptEventContent(
						std::collections::BTreeMap::new(),
					),
					room_id: room_id.to_owned(),
				})
			};

		// MSC4102: Synthesize unthreaded receipt if needed.
		// "To ensure older clients receive read receipts for threads, a server MUST
		// generate an unthreaded receipt for the same event and user when a threaded
		// receipt is received." Because Ruma cannot represent both on the same event,
		// and MSC4102 says to prioritize unthreaded, we effectively mutate the
		// incoming threaded receipt to unthreaded, UNLESS the user's existing
		// unthreaded receipt is already on a more recent event.
		let mut synthetic_receipts = Vec::new();
		for (new_event_id, new_type, new_receipt) in &new_receipts {
			if new_receipt.thread != ReceiptThread::Unthreaded {
				let mut should_synthesize = true;

				// Find existing unthreaded receipt's event ID
				let mut existing_unthreaded_event_id = None;
				for (ev_id, receipts) in &existing_event.content.0 {
					if let Some(users) = receipts.get(new_type) {
						if let Some(receipt) = users.get(user_id) {
							if receipt.thread == ReceiptThread::Unthreaded {
								existing_unthreaded_event_id = Some(ev_id.clone());
								break;
							}
						}
					}
				}

				if let Some(existing_ev_id) = existing_unthreaded_event_id {
					if let (
						Ok(PduCount::Normal(new_count)),
						Ok(PduCount::Normal(existing_count)),
					) = (
						self.services.timeline.get_pdu_count(new_event_id).await,
						self.services.timeline.get_pdu_count(&existing_ev_id).await,
					) {
						if existing_count > new_count {
							should_synthesize = false;
						}
					}
				}

				if should_synthesize {
					let mut synthetic = new_receipt.clone();
					synthetic.thread = ReceiptThread::Unthreaded;
					synthetic_receipts.push((new_event_id.clone(), new_type.clone(), synthetic));
				}
			}
		}
		new_receipts.extend(synthetic_receipts);

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

		let new_count = self.services.globals.next_count().unwrap();

		conduwuit::trace!(
			?room_id,
			?user_id,
			?new_count,
			?old_count,
			"Updating dual-index read receipt maps"
		);

		// Delete old stream index entry
		if let Some(old_count) = old_count {
			let mut old_stream_key = room_id.as_bytes().to_vec();
			old_stream_key.push(database::SEP);
			old_stream_key.extend_from_slice(&old_count.to_be_bytes());
			old_stream_key.push(database::SEP);
			old_stream_key.extend_from_slice(user_id.as_bytes());
			self.readreceiptid_readreceipt.remove(&old_stream_key);
		}

		conduwuit::debug!(
			target: "read_receipt_debug",
			"Saving existing_event to DB: {}",
			serde_json::to_string(&existing_event).unwrap()
		);

		let existing_event_json = Json(&existing_event);

		// Insert new stream index entry
		let mut new_stream_key = room_id.as_bytes().to_vec();
		new_stream_key.push(database::SEP);
		new_stream_key.extend_from_slice(&new_count.to_be_bytes());
		new_stream_key.push(database::SEP);
		new_stream_key.extend_from_slice(user_id.as_bytes());

		// For backward compatibility with older legacy maps, we store the pure
		// ReceiptEvent in the stream index
		self.readreceiptid_readreceipt
			.put(new_stream_key, &existing_event_json);

		// Update state map
		self.roomuserid_readreceipt
			.put(key, Json((new_count, existing_event)));
	}

	pub(super) fn readreceipts_since<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
	) -> impl Stream<Item = ReceiptItem> + Send + 'a {
		// Dual-index stream: readreceiptid_readreceipt is keyed by (RoomId, Count,
		// UserId)
		let mut prefix = room_id.as_bytes().to_vec();
		prefix.push(database::SEP);

		let mut first_possible_key = prefix.clone();
		first_possible_key.extend_from_slice(&(since.saturating_add(1)).to_be_bytes());

		self.readreceiptid_readreceipt
			.raw_stream_from(&first_possible_key)
			.ignore_err()
			.ready_take_while(move |(key, _): &(&[u8], &[u8])| key.starts_with(&prefix))
			.map(move |(key, value): (&[u8], &[u8])| {
				// Parse count and user_id from the key
				let room_id_bytes = room_id.as_bytes();
				// Key structure: room_id + SEP + count (8 bytes) + SEP + user_id
				let count_start = room_id_bytes.len().saturating_add(1);
				let count_end = count_start.saturating_add(8);

				if key.len() <= count_end || key[count_end] != database::SEP {
					return Err(conduwuit::Error::bad_database(
						"Invalid readreceiptid_readreceipt key",
					));
				}

				let count_bytes = &key[count_start..count_end];
				let count = conduwuit::utils::u64_from_bytes(count_bytes)
					.map_err(|_| conduwuit::Error::bad_database("Invalid count bytes"))?;

				let user_id_bytes = &key[count_end.saturating_add(1)..];
				let user_id_str = conduwuit::utils::str_from_bytes(user_id_bytes)?;
				let user_id = <&UserId>::try_from(user_id_str)
					.map_err(|_| conduwuit::Error::bad_database("Invalid user ID"))?
					.to_owned();

				let json: CanonicalJsonObject = serde_json::from_slice(value)?;
				let event = serde_json::value::to_raw_value(&json)?;

				conduwuit::trace!(
					"Yielding read receipt for user {} at count {} (since was {})",
					user_id,
					count,
					since
				);

				Ok((user_id, count, Raw::from_json(event)))
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
		let key = roomuserid_key(room_id, user_id);
		let next_count = self.services.globals.next_count()?;

		// Delete from legacy maps so they don't shadow in private_read_get during the
		// transitional phase
		let mut legacy_key = room_id.as_bytes().to_vec();
		legacy_key.push(0xFF);
		legacy_key.extend_from_slice(user_id.as_bytes());
		self.roomuserid_privateread.remove(&legacy_key);
		self.roomuserid_privatereadevent.remove(&legacy_key);
		self.roomuserid_lastprivatereadupdate.remove(&legacy_key);

		self.roomuserid_privatereadreceipt
			.put(key, Json((count, receipt, next_count)));

		Ok(())
	}

	pub(super) async fn private_read_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<u64> {
		let key = roomuserid_key(room_id, user_id);
		if let Ok(value) = self.roomuserid_privatereadreceipt.get(&key).await {
			if let Ok((count, ..)) = serde_json::from_slice::<(u64, ReceiptEvent, u64)>(&value) {
				return Ok(count);
			}
		}

		let mut legacy_key = room_id.as_bytes().to_vec();
		legacy_key.push(0xFF);
		legacy_key.extend_from_slice(user_id.as_bytes());
		self.roomuserid_privateread
			.qry(&legacy_key)
			.await
			.deserialized()
	}

	pub(super) async fn last_privateread_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
	) -> u64 {
		let key = roomuserid_key(room_id, user_id);
		if let Ok(value) = self.roomuserid_privatereadreceipt.get(&key).await {
			if let Ok((_, _, update_count)) =
				serde_json::from_slice::<(u64, ReceiptEvent, u64)>(&value)
			{
				return update_count;
			}
		}

		let mut legacy_key = room_id.as_bytes().to_vec();
		legacy_key.push(0xFF);
		legacy_key.extend_from_slice(user_id.as_bytes());
		self.roomuserid_lastprivatereadupdate
			.qry(&legacy_key)
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
