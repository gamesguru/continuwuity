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
use tokio::task::yield_now;

use crate::{Dep, globals};

pub(super) struct Data {
	roomuserid_privateread: Arc<Map>,
	roomuserid_privatereadevent: Arc<Map>,
	roomuserid_lastprivatereadupdate: Arc<Map>,
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
				conduwuit::utils::u64_from_bytes(&*bytes).expect("bytes have right length")
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
		let event_id = pdu.event_id.to_owned();

		let mut receipt_map = std::collections::BTreeMap::new();
		receipt_map.insert(ruma::events::receipt::ReceiptType::ReadPrivate, user_map);
		let mut content = std::collections::BTreeMap::new();
		content.insert(event_id.into(), receipt_map);

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
					new_receipts.push((
						event_id.clone(),
						receipt_type.clone(),
						receipt.thread.clone(),
					));
				}
			}
		}

		if new_receipts.is_empty() {
			return;
		}

		// Remove old entry for the same thread
		let last_possible_key = (room_id, u64::MAX);
		let mut stream = self
			.readreceiptid_readreceipt
			.rev_stream_from_raw(&last_possible_key)
			.ignore_err()
			.ready_take_while(|(key, _)| {
				key.starts_with(room_id.as_bytes())
					&& key.get(room_id.as_bytes().len()) == Some(&database::SEP)
			});

		let mut to_remove = Vec::new();
		let mut matches_found: usize = 0;
		let mut actual_changes = new_receipts.len();

		while let Some((key, value)) = stream.next().await {
			let user_id_bytes = user_id.as_bytes();
			if key.ends_with(user_id_bytes)
				&& key
					.len()
					.checked_sub(user_id_bytes.len())
					.and_then(|len| len.checked_sub(1))
					.and_then(|idx| key.get(idx))
					== Some(&database::SEP)
			{
				let Ok(receipt) = serde_json::from_slice::<ReceiptEvent>(value) else {
					continue;
				};
				let mut match_found = false;
				for (old_event_id, old_receipts) in &receipt.content.0 {
					for (receipt_type, users) in old_receipts {
						if let Some(old_receipt) = users.get(user_id) {
							for (new_event_id, new_type, new_thread) in &new_receipts {
								if receipt_type == new_type && &old_receipt.thread == new_thread {
									match_found = true;
									matches_found = matches_found.saturating_add(1);

									if old_event_id == new_event_id {
										actual_changes = actual_changes.saturating_sub(1);
									} else {
										conduwuit::trace!(
											?room_id,
											?user_id,
											?receipt_type,
											?new_thread,
											"Deleting old read receipt"
										);
										to_remove.push(key.to_vec());
									}
									break;
								}
							}
						}
						if match_found {
							break;
						}
					}
					if match_found {
						break;
					}
				}

				if matches_found >= new_receipts.len() {
					break;
				}
			}
		}

		if actual_changes == 0 {
			conduwuit::trace!(
				?room_id,
				?user_id,
				"Read receipts did not change, skipping update"
			);
			return;
		}

		for key in to_remove {
			self.readreceiptid_readreceipt.remove_raw(&key);
			yield_now().await;
		}

		let count = self.services.globals.next_count().unwrap();
		let latest_id = (room_id, count, user_id);
		conduwuit::trace!(
			?room_id,
			?user_id,
			?count,
			?new_receipts,
			"Inserting new read receipt"
		);
		self.readreceiptid_readreceipt.put(latest_id, Json(event));
	}

	pub(super) fn readreceipts_since<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
	) -> impl Stream<Item = ReceiptItem> + Send + 'a {
		type Key<'a> = (&'a RoomId, u64, &'a UserId);
		type KeyVal<'a> = (Key<'a>, CanonicalJsonObject);

		let after_since = since.saturating_add(1); // +1 so we don't send the event at since
		let first_possible_edu = (room_id, after_since);

		self.readreceiptid_readreceipt
			.stream_from(&first_possible_edu)
			.ignore_err()
			.ready_take_while(move |((r, ..), _): &KeyVal<'_>| *r == room_id)
			.map(move |((_, count, user_id), mut json): KeyVal<'_>| {
				json.remove("room_id");

				let event = serde_json::value::to_raw_value(&json)?;

				conduwuit::trace!(
					"Yielding read receipt for user {} at count {} (since was {})",
					user_id,
					count,
					since
				);

				Ok((user_id.to_owned(), count, Raw::from_json(event)))
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
			.insert(&key, &count.to_be_bytes());
		self.roomuserid_privatereadevent.insert(&key, &receipt_json);
		self.roomuserid_lastprivatereadupdate
			.insert(&key, &next_count.to_be_bytes());

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
