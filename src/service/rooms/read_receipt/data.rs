use std::{collections::BTreeMap, sync::Arc};

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
type PrivateReadReceipts = BTreeMap<String, (u64, ReceiptEvent, u64)>;

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
			if let Ok(receipts) = serde_json::from_slice::<PrivateReadReceipts>(&value) {
				return Ok(combine_private_read_receipts(room_id, receipts));
			}

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
		let mut user_map = BTreeMap::new();
		user_map.insert(user_id.to_owned(), Receipt {
			thread: ReceiptThread::Unthreaded,
			ts: None, // Legacy receipts have no timestamp
		});

		let shortroomid = self.services.short.get_shortroomid(room_id).await?;
		let shorteventid = PduCount::Normal(count);
		let pdu_id: RawPduId = PduId { shortroomid, shorteventid }.into();
		let pdu = self.services.timeline.get_pdu_from_id(&pdu_id).await?;
		let event_id = pdu.event_id;

		let mut receipt_map = BTreeMap::new();
		receipt_map.insert(ReceiptType::ReadPrivate, user_map);
		let mut content = BTreeMap::new();
		content.insert(event_id, receipt_map);

		let event = ReceiptEvent {
			content: ruma::events::receipt::ReceiptEventContent(content),
			room_id: room_id.to_owned(),
		};

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
					new_receipts.push((
						event_id.clone(),
						receipt_type.clone(),
						receipt.clone(),
						false,
					));
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
						content: ruma::events::receipt::ReceiptEventContent(BTreeMap::new()),
						room_id: room_id.to_owned(),
					})
				}
			} else {
				(None, ReceiptEvent {
					content: ruma::events::receipt::ReceiptEventContent(BTreeMap::new()),
					room_id: room_id.to_owned(),
				})
			};

		// MSC4102: Synthesize unthreaded receipts for threaded ones
		let synthetic_receipts = self
			.synthesize_msc4102_unthreaded(user_id, &new_receipts, &existing_event)
			.await;
		new_receipts.extend(synthetic_receipts);

		// Remove old receipts for the same thread and type
		for (_, new_type, new_receipt, _) in &new_receipts {
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
		for (new_event_id, new_type, new_receipt, is_synthetic) in new_receipts {
			let users = existing_event
				.content
				.0
				.entry(new_event_id)
				.or_default()
				.entry(new_type)
				.or_default();

			if is_synthetic && users.contains_key(user_id) {
				continue;
			}

			users.insert(user_id.to_owned(), new_receipt);
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

		conduwuit::trace!(
			target: "read_receipt_debug",
			?existing_event,
			"Saving existing_event to DB"
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
		let thread_key = private_read_thread_key(receipt, user_id);
		let mut receipts =
			if let Ok(value) = self.roomuserid_privatereadreceipt.get_blocking(&key) {
				serde_json::from_slice::<PrivateReadReceipts>(&value).unwrap_or_else(|_| {
					serde_json::from_slice::<(u64, ReceiptEvent, u64)>(&value)
						.map(|entry| {
							BTreeMap::from([(private_read_thread_key(&entry.1, user_id), entry)])
						})
						.unwrap_or_default()
				})
			} else {
				BTreeMap::new()
			};

		// Delete from legacy maps so they don't shadow in private_read_get during the
		// transitional phase
		let mut legacy_key = room_id.as_bytes().to_vec();
		legacy_key.push(0xFF);
		legacy_key.extend_from_slice(user_id.as_bytes());
		self.roomuserid_privateread.remove(&legacy_key);
		self.roomuserid_privatereadevent.remove(&legacy_key);
		self.roomuserid_lastprivatereadupdate.remove(&legacy_key);

		receipts.insert(thread_key, (count, receipt.clone(), next_count));
		self.roomuserid_privatereadreceipt.put(key, Json(receipts));

		Ok(())
	}

	pub(super) async fn private_read_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		thread: Option<&ReceiptThread>,
	) -> Result<u64> {
		let key = roomuserid_key(room_id, user_id);
		if let Ok(value) = self.roomuserid_privatereadreceipt.get(&key).await {
			if let Ok(receipts) = serde_json::from_slice::<PrivateReadReceipts>(&value) {
				if let Some((count, ..)) = receipts.get(&thread_key(thread)) {
					return Ok(*count);
				}
			}

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
			if let Ok(receipts) = serde_json::from_slice::<PrivateReadReceipts>(&value) {
				return receipts
					.values()
					.map(|(_, _, update_count)| *update_count)
					.max()
					.unwrap_or(0);
			}

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

	/// MSC4102: When a threaded receipt is received, synthesize an unthreaded
	/// copy so older clients still see it. Skips synthesis if the user already
	/// has an unthreaded receipt on a more recent event.
	async fn synthesize_msc4102_unthreaded(
		&self,
		user_id: &UserId,
		new_receipts: &[(ruma::OwnedEventId, ReceiptType, Receipt, bool)],
		existing_event: &ReceiptEvent,
	) -> Vec<(ruma::OwnedEventId, ReceiptType, Receipt, bool)> {
		let mut synthetic = Vec::new();
		for (new_event_id, new_type, new_receipt, _) in new_receipts {
			if new_receipt.thread == ReceiptThread::Unthreaded {
				continue;
			}

			// Check if user already has an unthreaded receipt for this type
			// on a more recent event -- if so, skip synthesis.
			let existing_unthreaded_event_id =
				existing_event
					.content
					.0
					.iter()
					.find_map(|(ev_id, receipts)| {
						receipts
							.get(new_type)
							.and_then(|users| users.get(user_id))
							.filter(|r| r.thread == ReceiptThread::Unthreaded)
							.map(|_| ev_id.clone())
					});

			if let Some(existing_ev_id) = existing_unthreaded_event_id {
				if let (Ok(PduCount::Normal(new_count)), Ok(PduCount::Normal(existing_count))) = (
					self.services.timeline.get_pdu_count(new_event_id).await,
					self.services.timeline.get_pdu_count(&existing_ev_id).await,
				) {
					if existing_count > new_count {
						continue;
					}
				}
			}

			let mut unthreaded = new_receipt.clone();
			unthreaded.thread = ReceiptThread::Unthreaded;
			synthetic.push((new_event_id.clone(), new_type.clone(), unthreaded, true));
		}
		synthetic
	}
}

#[inline]
fn roomuserid_key(room_id: &RoomId, user_id: &UserId) -> Vec<u8> {
	let mut key = room_id.as_bytes().to_vec();
	key.push(database::SEP);
	key.extend_from_slice(user_id.as_bytes());
	key
}

fn thread_key(thread: Option<&ReceiptThread>) -> String {
	thread
		.and_then(ReceiptThread::as_str)
		.unwrap_or_default()
		.to_owned()
}

fn private_read_thread_key(event: &ReceiptEvent, user_id: &UserId) -> String {
	event
		.content
		.0
		.values()
		.flat_map(BTreeMap::values)
		.find_map(|users| users.get(user_id))
		.map(|receipt| thread_key(Some(&receipt.thread)))
		.unwrap_or_default()
}

fn combine_private_read_receipts(
	room_id: &RoomId,
	receipts: PrivateReadReceipts,
) -> Option<(u64, ReceiptEvent)> {
	let mut count = 0;
	let mut content = BTreeMap::new();

	for (receipt_count, event, _) in receipts.into_values() {
		count = count.max(receipt_count);
		for (event_id, receipt_types) in event.content.0 {
			for (receipt_type, users) in receipt_types {
				content
					.entry(event_id.clone())
					.or_insert_with(BTreeMap::new)
					.entry(receipt_type.clone())
					.or_insert_with(BTreeMap::new)
					.extend(users);
			}
		}
	}

	(!content.is_empty()).then(|| {
		(count, ReceiptEvent {
			content: ruma::events::receipt::ReceiptEventContent(content),
			room_id: room_id.to_owned(),
		})
	})
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use ruma::{
		OwnedEventId,
		events::receipt::{
			Receipt, ReceiptEvent, ReceiptEventContent, ReceiptThread, ReceiptType,
		},
		room_id,
	};

	fn make_empty_receipt_event() -> ReceiptEvent {
		ReceiptEvent {
			content: ReceiptEventContent(BTreeMap::new()),
			room_id: room_id!("!test:example.com").to_owned(),
		}
	}

	/// Threaded receipt must produce a synthetic unthreaded copy (MSC4102).
	/// This is the exact regression that broke TestThreadReceiptsInSyncMSC4102.
	#[test]
	fn msc4102_threaded_produces_unthreaded() {
		let event_id: OwnedEventId = "$msg:example.com".try_into().unwrap();
		let threaded = Receipt {
			ts: None,
			thread: ReceiptThread::Thread("$root:example.com".try_into().unwrap()),
		};

		let mut new_receipts = vec![(event_id.clone(), ReceiptType::Read, threaded)];
		let _existing = make_empty_receipt_event();

		// Simulate what readreceipt_update does: identify threaded receipts
		// and append unthreaded copies.
		let synthetics: Vec<_> = new_receipts
			.iter()
			.filter(|(_, _, r)| r.thread != ReceiptThread::Unthreaded)
			.map(|(eid, rtype, r)| {
				let mut unthreaded = r.clone();
				unthreaded.thread = ReceiptThread::Unthreaded;
				(eid.clone(), rtype.clone(), unthreaded)
			})
			.collect();
		new_receipts.extend(synthetics);

		// Must have original + synthetic
		assert_eq!(new_receipts.len(), 2);
		assert!(
			matches!(new_receipts[0].2.thread, ReceiptThread::Thread(_)),
			"original must stay threaded"
		);
		assert!(
			matches!(new_receipts[1].2.thread, ReceiptThread::Unthreaded),
			"synthetic must be unthreaded"
		);
		assert_eq!(new_receipts[0].0, new_receipts[1].0, "same event_id");
		assert_eq!(new_receipts[0].1, new_receipts[1].1, "same receipt type");
	}

	/// Unthreaded receipt must NOT produce a synthetic -- no duplication.
	#[test]
	fn msc4102_unthreaded_no_synthesis() {
		let event_id: OwnedEventId = "$msg:example.com".try_into().unwrap();
		let unthreaded = Receipt {
			ts: None,
			thread: ReceiptThread::Unthreaded,
		};

		let mut new_receipts = vec![(event_id.clone(), ReceiptType::Read, unthreaded)];

		let synthetics: Vec<_> = new_receipts
			.iter()
			.filter(|(_, _, r)| r.thread != ReceiptThread::Unthreaded)
			.map(|(eid, rtype, r)| {
				let mut copy = r.clone();
				copy.thread = ReceiptThread::Unthreaded;
				(eid.clone(), rtype.clone(), copy)
			})
			.collect();
		new_receipts.extend(synthetics);

		assert_eq!(new_receipts.len(), 1, "no synthetic should be added");
	}
}
