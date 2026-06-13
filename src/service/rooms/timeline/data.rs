use std::sync::Arc;

use conduwuit::{
	Err, Event, PduCount, PduEvent, Result, at, err,
	result::NotFound,
	utils::{
		self,
		stream::{TryReadyExt, WidebandExt},
	},
};
use database::{Database, Deserialized, Json, KeyVal, Map};
use futures::{FutureExt, Stream, TryFutureExt, TryStreamExt, future::select_ok, pin_mut};
use ruma::{CanonicalJsonObject, EventId, OwnedEventId, OwnedUserId, RoomId, api::Direction};

use super::{PduId, RawPduId};
use crate::{Dep, rooms, rooms::short::ShortRoomId};

pub(super) struct Data {
	eventid_pduid: Arc<Map>,
	roomid_outliereventid: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
	eventid_pdu: Arc<Map>,
	eventid_metadata: Arc<Map>,
	room_pducount_eventid: Arc<Map>,
	roomid_topologicalorder_pducount: Arc<Map>,
	pub(super) room_pducount_eventid_backup: Arc<Map>,
	pub(super) db: Arc<Database>,
	services: Services,
}

struct Services {
	short: Dep<rooms::short::Service>,
}

pub type PdusIterItem = (PduCount, PduEvent);

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			eventid_pduid: db["eventid_pduid"].clone(),
			roomid_outliereventid: db["roomid_outliereventid"].clone(),
			userroomid_highlightcount: db["userroomid_highlightcount"].clone(),
			userroomid_notificationcount: db["userroomid_notificationcount"].clone(),
			eventid_pdu: db["eventid_pdu"].clone(),
			eventid_metadata: db["eventid_metadata"].clone(),
			room_pducount_eventid: db["room_pducount_eventid"].clone(),
			roomid_topologicalorder_pducount: db["roomid_topologicalorder_pducount"].clone(),
			room_pducount_eventid_backup: db["room_pducount_eventid_backup"].clone(),
			db: args.db.clone(),
			services: Services {
				short: args.depend::<rooms::short::Service>("rooms::short"),
			},
		}
	}

	#[inline]
	pub(super) async fn last_timeline_count(&self, room_id: &RoomId) -> Result<PduCount> {
		let pdus_rev = self.pdus_rev(room_id, PduCount::max());

		pin_mut!(pdus_rev);
		let last_count = pdus_rev
			.try_next()
			.await?
			.map(at!(0))
			.filter(|&count| matches!(count, PduCount::Normal(_)))
			.unwrap_or_else(PduCount::max);

		Ok(last_count)
	}

	#[inline]
	pub(super) async fn latest_pdu_in_room(&self, room_id: &RoomId) -> Result<PduEvent> {
		let pdus_rev = self.pdus_rev(room_id, PduCount::max());

		pin_mut!(pdus_rev);
		pdus_rev
			.try_next()
			.await?
			.map(at!(1))
			.ok_or_else(|| err!(Request(NotFound("no PDUs found in room"))))
	}

	/// Returns the `count` of this pdu's id.
	pub(super) async fn get_pdu_count(&self, event_id: &EventId) -> Result<PduCount> {
		self.get_pdu_id(event_id)
			.await
			.map(|pdu_id| pdu_id.pdu_count())
	}

	/// Returns the json of a pdu.
	pub(super) async fn get_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
		let accepted = self.get_non_outlier_pdu_json(event_id).boxed();
		let outlier = async move {
			self.eventid_pdu
				.get(event_id.as_bytes())
				.await?
				.deserialized()
		}
		.boxed();

		select_ok([accepted, outlier]).await.map(at!(0))
	}

	/// Returns the json of a pdu.
	pub(super) async fn get_non_outlier_pdu_json(
		&self,
		event_id: &EventId,
	) -> Result<CanonicalJsonObject> {
		let _pduid = self.get_pdu_id(event_id).await?;

		self.eventid_pdu
			.get(event_id.as_bytes())
			.await
			.deserialized()
	}

	/// Directly gets the PDU and JSON from the double-write `eventid_pdu` tree.
	/// Used for timeline re-insertion when other indices are cleared.
	pub(super) async fn get_from_eventid_pdu(
		&self,
		event_id: &EventId,
	) -> Result<(PduEvent, CanonicalJsonObject)> {
		let handle = self.eventid_pdu.get(event_id.as_bytes()).await?;
		let pdu: PduEvent = handle.deserialized()?;
		let json: CanonicalJsonObject = handle.deserialized()?;
		Ok((pdu, json))
	}

	pub(super) async fn reindex_timeline(&self, room_id: &RoomId) -> Result<usize> {
		let mut count = 0_usize;
		let pdus = self.pdus(room_id, PduCount::min());
		pin_mut!(pdus);

		// Pre-allocate key buffer outside the loop to avoid repeated heap allocations
		let room_bytes = room_id.as_bytes();
		let mut key = Vec::with_capacity(room_bytes.len().saturating_add(1).saturating_add(64));

		while let Some((_, pdu)) = pdus.try_next().await? {
			// Use canonical key format: room_id || 0xFF || event_id
			// (must match add_pdu_outlier / room_stream expectations)
			key.clear();
			key.extend_from_slice(room_bytes);
			key.push(0xFF);
			key.extend_from_slice(pdu.event_id.as_bytes());

			if let Ok(json) = self.get_non_outlier_pdu_json(&pdu.event_id).await {
				// Since we combine outlier and timeline JSON, we just make sure
				// the JSON is stored in eventid_pdu. The add_pdu_outlier logic handles this.
				self.eventid_pdu
					.raw_put(pdu.event_id.as_bytes(), Json(&json));
				self.eventid_pdu.wake(pdu.event_id.as_bytes());
				// Must use raw_put to bypass Bincode serializer — .insert()
				// would prepend an 8-byte length prefix, corrupting lookups.
				self.roomid_outliereventid
					.raw_put::<&[u8], &[u8]>(&key, pdu.event_id.as_bytes());
				count = count.saturating_add(1);
			}
		}
		Ok(count)
	}

	pub(super) async fn fix_pdu_event_ids(&self) -> Result<usize> {
		use futures::TryStreamExt;
		let mut fixed: usize = 0;
		// Use raw_stream to iterate eventid_pduid mapping
		let iter = self.eventid_pduid.raw_stream();
		pin_mut!(iter);

		while let Some((event_id_bytes, pdu_id_bytes)) = iter.try_next().await? {
			if let Ok(event_id_str) = std::str::from_utf8(event_id_bytes) {
				if let Ok(event_id) = OwnedEventId::try_from(event_id_str) {
					let _pdu_id: RawPduId = pdu_id_bytes.into();
					if let Ok(mut json) = self
						.eventid_pdu
						.get(&event_id_bytes)
						.await
						.deserialized::<CanonicalJsonObject>()
					{
						if !json.contains_key("event_id") {
							json.insert(
								"event_id".into(),
								ruma::CanonicalJsonValue::String(event_id.as_str().to_owned()),
							);
							self.eventid_pdu.raw_put(event_id_bytes, Json(&json));
							fixed = fixed.saturating_add(1);
						}
					}
				}
			}
		}
		Ok(fixed)
	}

	pub(super) fn topo_pducount_key(pdu_id: &RawPduId, local_topological_depth: u64) -> Vec<u8> {
		let mut topo_key = Vec::with_capacity(32);
		topo_key.extend_from_slice(&pdu_id.shortroomid());
		topo_key.extend_from_slice(&local_topological_depth.to_be_bytes());
		topo_key.extend_from_slice(&pdu_id.as_ref()[8..]);
		topo_key
	}

	pub(super) fn topo_key_to_pdu_id(topo_key: &[u8]) -> RawPduId {
		let mut pdu_id_bytes = [0_u8; 16];
		pdu_id_bytes[0..8].copy_from_slice(&topo_key[0..8]);
		pdu_id_bytes[8..16].copy_from_slice(&topo_key[16..24]);
		pdu_id_bytes.as_slice().into()
	}

	pub(super) async fn pdu_id_to_topo_key(&self, pdu_id: &RawPduId) -> Result<Vec<u8>> {
		let event_id_bytes = self.room_pducount_eventid.get(pdu_id).await?;
		let metadata_bytes = self.eventid_metadata.get(&event_id_bytes).await?;
		let meta: rooms::timeline::EventMetadata = bincode::deserialize(&metadata_bytes)
			.map_err(|e| err!(Database("Failed to deserialize EventMetadata: {e}")))?;
		Ok(Self::topo_pducount_key(pdu_id, meta.local_topological_depth))
	}

	pub(super) fn remove_topo_pducount(&self, pdu_id: &RawPduId, event_id_bytes: &[u8]) {
		if let Ok(bytes) = self.eventid_metadata.get_blocking(event_id_bytes) {
			if let Ok(meta) = bincode::deserialize::<rooms::timeline::EventMetadata>(&bytes) {
				self.roomid_topologicalorder_pducount
					.remove(&Self::topo_pducount_key(pdu_id, meta.local_topological_depth));
			}
		}
	}

	pub(super) async fn remove_from_timeline(&self, event_id: &EventId) {
		if let Ok(pduid) = self.get_pdu_id(event_id).await {
			self.eventid_pduid.remove(event_id);
			self.room_pducount_eventid.remove(&pduid);
			self.remove_topo_pducount(&pduid, event_id.as_bytes());

			if self.outlier_pdu_exists(event_id).await.is_err() {
				self.eventid_pdu.remove(event_id.as_bytes());
				self.eventid_metadata.remove(event_id.as_bytes());
			}
		}
	}

	/// Remove timeline entry when pdu_id is known (avoids DB lookup).
	pub(super) fn remove_from_timeline_by_id(&self, pdu_id: &RawPduId, event_id: &EventId) {
		self.eventid_pduid.remove(event_id);
		self.room_pducount_eventid.remove(pdu_id);
		self.remove_topo_pducount(pdu_id, event_id.as_bytes());
	}

	/// Drop a duplicate PDU by ID without removing the event mapping
	pub(super) fn drop_duplicate_pdu(&self, pdu_id: &RawPduId) {
		self.room_pducount_eventid.remove(pdu_id);
		if let Ok(event_id_bytes) = self.room_pducount_eventid.get_blocking(pdu_id) {
			self.remove_topo_pducount(pdu_id, &event_id_bytes);
		}
	}

	/// Returns the pdu's id.
	#[inline]
	pub(super) async fn get_pdu_id(&self, event_id: &EventId) -> Result<RawPduId> {
		self.eventid_pduid
			.get(event_id)
			.await
			.map(|handle| RawPduId::from(&*handle))
	}

	/// Returns the pdu directly from `eventid_pduid` only.
	/// If `room_id` is provided, validates the PDU belongs to that room.
	pub(super) async fn get_non_outlier_pdu_in_room(
		&self,
		room_id: Option<&RoomId>,
		event_id: &EventId,
	) -> Result<PduEvent> {
		let pduid = self.get_pdu_id(event_id).await?;
		let pdu: PduEvent = self
			.eventid_pdu
			.get(event_id.as_bytes())
			.await
			.deserialized()?;

		// Enforce cross-room boundary: verify the PDU belongs to the expected room
		if let Some(expected_room) = room_id {
			let actual_room = pdu.room_id_or_hash();
			if let Some(actual_room) = actual_room {
				if actual_room != expected_room {
					return Err!(Database(
						"PDU {event_id} does belong to room {actual_room} (expected \
						 {expected_room})"
					));
				}
			} else {
				// v12 hashed-room PDUs may not contain room_id in the JSON.
				// Verify room association by comparing ShortRoomId from pdu_id.
				let expected_shortroomid =
					self.services.short.get_shortroomid(expected_room).await?;
				if pduid.shortroomid() != expected_shortroomid.to_be_bytes() {
					return Err!(Database(
						"PDU {event_id} does not belong to room {expected_room}"
					));
				}
			}
		}

		Ok(pdu)
	}

	/// Like get_non_outlier_pdu(), but without the expense of fetching and
	/// parsing the PduEvent
	pub(super) async fn non_outlier_pdu_exists(&self, event_id: &EventId) -> Result {
		let pduid = self.get_pdu_id(event_id).await?;

		self.room_pducount_eventid.exists(&pduid).await
	}

	/// Returns the pdu.
	///
	/// Checks the `eventid_pdu` Tree if not found in the timeline.
	/// If `room_id` is provided, validates the PDU belongs to that room.
	pub(super) async fn get_pdu_in_room(
		&self,
		room_id: Option<&RoomId>,
		event_id: &EventId,
	) -> Result<PduEvent> {
		let accepted = self.get_non_outlier_pdu_in_room(room_id, event_id).boxed();
		let outlier = self
			.eventid_pdu
			.get(event_id.as_bytes())
			.then(move |handle| async move {
				let handle = handle?;
				let pdu: PduEvent = handle.deserialized()?;

				// Enforce cross-room boundary
				if let Some(expected_room) = room_id {
					let actual_room = pdu.room_id_or_hash();
					if let Some(actual_room) = actual_room {
						if actual_room != expected_room {
							return Err(conduwuit::err!(Database(
								"Outlier PDU {event_id} does belong to room {actual_room} \
								 (expected {expected_room})"
							)));
						}
					} else {
						// v12 hashed-room PDUs may not contain room_id in the JSON.
						// Verify room association via roomid_outliereventid table.
						let mut key = expected_room.as_bytes().to_vec();
						key.push(0xFF);
						key.extend_from_slice(event_id.as_bytes());
						if self.roomid_outliereventid.exists(&key).await.is_err() {
							return Err(conduwuit::err!(Database(
								"Outlier PDU {event_id} is not associated with room \
								 {expected_room}"
							)));
						}
					}
				}

				Ok(pdu)
			})
			.boxed();

		select_ok([accepted, outlier]).await.map(at!(0))
	}

	pub(super) async fn get_pdus_in_room_batch(
		&self,
		room_id: Option<&RoomId>,
		event_ids: &[OwnedEventId],
	) -> Vec<Result<PduEvent>> {
		use futures::StreamExt;
		let mut results = Vec::with_capacity(event_ids.len());

		let mut expected_shortroomid: Option<ShortRoomId> = None;
		if let Some(expected_room) = room_id {
			if let Ok(id) = self.services.short.get_shortroomid(expected_room).await {
				expected_shortroomid = Some(id);
			}
		}

		// Batch fetch from eventid_pduid
		let pdu_ids: Vec<Result<database::Handle<'_>>> = self
			.eventid_pduid
			.get_batch(futures::stream::iter(event_ids.iter().map(|id| id.as_bytes())))
			.collect()
			.await;

		// Separate into hits and misses
		let mut valid_pdu_ids = Vec::with_capacity(event_ids.len());
		let mut missing_event_ids = Vec::with_capacity(event_ids.len());

		for (i, pdu_id_res) in pdu_ids.iter().enumerate() {
			match pdu_id_res {
				| Ok(handle) => valid_pdu_ids.push(RawPduId::from(&**handle)),
				| Err(_) => missing_event_ids.push(event_ids[i].as_bytes()),
			}
		}

		// Batch fetch timeline PDUs
		let pdu_events = if !valid_pdu_ids.is_empty() {
			let event_id_bytes_batch: Vec<Result<database::Handle<'_>>> = self
				.room_pducount_eventid
				.get_batch(futures::stream::iter(valid_pdu_ids.iter().map(AsRef::as_ref)))
				.collect()
				.await;

			let valid_event_id_bytes: Vec<Vec<u8>> = event_id_bytes_batch
				.into_iter()
				.map(|r| r.map(|h| h.to_vec()).unwrap_or_default())
				.collect();

			self.eventid_pdu
				.get_batch(futures::stream::iter(valid_event_id_bytes.iter().map(Vec::as_slice)))
				.map(|res: Result<database::Handle<'_>>| {
					res.and_then(|handle| handle.deserialized::<PduEvent>())
				})
				.collect()
				.await
		} else {
			Vec::new()
		};

		// Batch fetch outliers
		let outlier_events = if !missing_event_ids.is_empty() {
			self.eventid_pdu
				.get_batch(futures::stream::iter(missing_event_ids))
				.map(|res: Result<database::Handle<'_>>| {
					res.and_then(|handle| handle.deserialized::<PduEvent>())
				})
				.collect()
				.await
		} else {
			Vec::new()
		};

		// Re-assemble results in original order
		let mut pdu_iter = pdu_events.into_iter();
		let mut outlier_iter = outlier_events.into_iter();

		for (i, pdu_id_res) in pdu_ids.iter().enumerate() {
			if let Ok(pdu_id_handle) = pdu_id_res {
				// Result comes from timeline
				let pdu_res: Result<PduEvent> = pdu_iter
					.next()
					.expect("length matches timeline fetch count");
				match pdu_res {
					| Ok(pdu) => {
						// Verify room boundary
						if let Some(expected_room) = room_id {
							if let Some(actual_room) = pdu.room_id_or_hash() {
								if actual_room != expected_room {
									results.push(Err!(Database(
										"PDU {} does belong to room {} (expected {})",
										event_ids[i],
										actual_room,
										expected_room
									)));
									continue;
								}
							} else if let Some(expected_short) = expected_shortroomid {
								let pduid = RawPduId::from(&**pdu_id_handle);
								if pduid.shortroomid() != expected_short.to_be_bytes() {
									results.push(Err!(Database(
										"PDU {} does not belong to room {}",
										event_ids[i],
										expected_room
									)));
									continue;
								}
							} else {
								results.push(Err!(Database(
									"PDU {} lacks room_id and expected shortroomid is unknown",
									event_ids[i]
								)));
								continue;
							}
						}
						results.push(Ok(pdu));
					},
					| Err(e) => results.push(Err(e)),
				}
			} else {
				// Result comes from outlier
				let outlier_res: Result<PduEvent> = outlier_iter
					.next()
					.expect("length matches outlier fetch count");
				match outlier_res {
					| Ok(pdu) => {
						if let Some(expected_room) = room_id {
							if let Some(actual_room) = pdu.room_id_or_hash() {
								if actual_room != expected_room {
									results.push(Err!(Database(
										"PDU {} does belong to room {} (expected {})",
										event_ids[i],
										actual_room,
										expected_room
									)));
									continue;
								}
							}
						}
						results.push(Ok(pdu));
					},
					| Err(_) => {
						results.push(Err!(Request(NotFound(
							"PDU not found in timeline or outliers"
						))));
					},
				}
			}
		}

		results
	}

	pub(super) fn multi_get_pdus<'a, S>(
		&'a self,
		room_id: Option<&'a RoomId>,
		event_ids: S,
	) -> impl Stream<Item = Result<PduEvent>> + Send + 'a
	where
		S: Stream<Item = OwnedEventId> + Send + 'a,
	{
		use conduwuit::utils::stream::{automatic_amplification, automatic_width};
		use futures::StreamExt;

		event_ids
			.boxed()
			.ready_chunks(automatic_amplification())
			.widen_then(automatic_width(), move |chunk| async move {
				self.get_pdus_in_room_batch(room_id, &chunk).await
			})
			.map(futures::stream::iter)
			.flatten()
	}

	/// Like get_non_outlier_pdu(), but without the expense of fetching and
	/// parsing the PduEvent
	#[inline]
	pub(super) async fn outlier_pdu_exists(&self, event_id: &EventId) -> Result<()> {
		let bytes = self.eventid_metadata.get(event_id.as_bytes()).await?;
		let meta: rooms::timeline::EventMetadata =
			bincode::deserialize(&bytes).map_err(|e| err!(Database("corrupt metadata: {e}")))?;
		if meta.is_outlier {
			Ok(())
		} else {
			Err(err!(Request(NotFound("Not an outlier"))))
		}
	}

	/// Like get_pdu(), but without the expense of fetching and parsing the data
	pub(super) async fn pdu_exists(&self, event_id: &EventId) -> Result {
		let non_outlier = self.non_outlier_pdu_exists(event_id).boxed();
		let outlier = self.outlier_pdu_exists(event_id).boxed();

		select_ok([non_outlier, outlier]).await.map(at!(0))
	}

	/// Returns the pdu.
	///
	/// This does __NOT__ check the outliers `Tree`.
	/// If `room_id` is provided, validates the PDU belongs to that room.
	pub(super) async fn get_pdu_from_id_in_room(
		&self,
		room_id: Option<&RoomId>,
		pdu_id: &RawPduId,
	) -> Result<PduEvent> {
		let event_id_bytes = self.room_pducount_eventid.get(pdu_id).await?;
		let pdu: PduEvent = self.eventid_pdu.get(&event_id_bytes).await.deserialized()?;

		if let Some(expected_room) = room_id {
			let actual_room = pdu.room_id_or_hash();
			if let Some(actual_room) = actual_room {
				if actual_room != expected_room {
					return Err!(Database(
						"PDU does belong to room {actual_room} (expected {expected_room})"
					));
				}
			} else {
				// v12 hashed-room PDUs may not contain room_id in the JSON.
				// Verify room association by comparing ShortRoomId from pdu_id.
				let expected_shortroomid =
					self.services.short.get_shortroomid(expected_room).await?;
				if pdu_id.shortroomid() != expected_shortroomid.to_be_bytes() {
					return Err!(Database("PDU does not belong to room {expected_room}"));
				}
			}
		}

		Ok(pdu)
	}

	/// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
	pub(super) async fn get_pdu_json_from_id(
		&self,
		pdu_id: &RawPduId,
	) -> Result<CanonicalJsonObject> {
		let event_id_bytes = self.room_pducount_eventid.get(pdu_id).await?;
		self.eventid_pdu.get(&event_id_bytes).await.deserialized()
	}

	pub(super) async fn append_pdu(
		&self,
		pdu_id: &RawPduId,
		pdu: &PduEvent,
		json: &CanonicalJsonObject,
		count: PduCount,
	) {
		debug_assert!(matches!(count, PduCount::Normal(_)), "PduCount not Normal");

		let mut batch = database::rocksdb::WriteBatch::default();

		let event_id_bytes = pdu.event_id.as_bytes();

		// Map event_id -> pdu_id
		self.eventid_pduid
			.insert_into_batch(&mut batch, &event_id_bytes, pdu_id);

		// No need to remove from eventid_outlierpdu.
		// remove_from_timeline will drop it from eventid_pdu if it's not an outlier.

		// CLEANUP: Drop the room outlier index to prevent ghosts during
		// reorder-timeline
		let room_id_from_json = json
			.get("room_id")
			.and_then(ruma::CanonicalJsonValue::as_str)
			.and_then(|r| <&RoomId>::try_from(r).ok());

		let room_id = room_id_from_json.map(ToOwned::to_owned).or_else(|| {
			(json.get("type").and_then(ruma::CanonicalJsonValue::as_str) == Some("m.room.create"))
				.then(|| pdu.event_id.as_str().replace('$', "!"))
				.and_then(|r| ruma::OwnedRoomId::parse(r).ok())
		});

		if let Some(room) = room_id {
			let mut key = room.as_bytes().to_vec();
			key.push(0xFF);
			key.extend_from_slice(event_id_bytes);
			self.roomid_outliereventid
				.remove_from_batch(&mut batch, &key);
		}

		// --- Phase 1: Double-Write ---
		self.eventid_pdu
			.raw_put_into_batch(&mut batch, event_id_bytes, Json(json));

		self.room_pducount_eventid
			.insert_into_batch(&mut batch, pdu_id, event_id_bytes);

		let existing_metadata = if let Ok(bytes) = self.eventid_metadata.get(event_id_bytes).await
		{
			bincode::deserialize::<rooms::timeline::EventMetadata>(&bytes).ok()
		} else {
			None
		};

		let local_topological_depth = existing_metadata.as_ref().map_or_else(
			|| {
				let mut max_depth = 0;
				for prev_id in pdu.prev_events() {
					if let Ok(bytes) = self.eventid_metadata.get_blocking(prev_id.as_bytes()) {
						if let Ok(meta) =
							bincode::deserialize::<rooms::timeline::EventMetadata>(&bytes)
						{
							max_depth = max_depth.max(meta.local_topological_depth);
						}
					}
				}
				max_depth.saturating_add(1)
			},
			|m| m.local_topological_depth,
		);

		let topo_key = Self::topo_pducount_key(pdu_id, local_topological_depth);
		self.roomid_topologicalorder_pducount.insert_into_batch(
			&mut batch,
			&topo_key,
			event_id_bytes,
		);

		let metadata = rooms::timeline::EventMetadata {
			short_room_id: u64::from_be_bytes(pdu_id.shortroomid()),
			is_outlier: false,
			origin_server_ts: pdu.origin_server_ts().0,
			depth: pdu.depth(),
			soft_failed: existing_metadata.as_ref().is_some_and(|m| m.soft_failed),
			rejected: pdu.rejected(),
			redacted_by: pdu.redacts().map(ToOwned::to_owned),
			short_state_hash: existing_metadata.and_then(|m| m.short_state_hash),
			local_topological_depth,
		};
		if let Ok(metadata_bytes) = bincode::serialize(&metadata) {
			self.eventid_metadata
				.insert_into_batch(&mut batch, event_id_bytes, metadata_bytes);
		}

		self.eventid_pdu.apply_batch(&batch);
		self.room_pducount_eventid.wake(pdu_id);
		self.eventid_pdu.wake(event_id_bytes);
	}

	pub(super) fn prepend_backfill_pdu(
		&self,
		pdu_id: &RawPduId,
		event_id: &EventId,
		json: &CanonicalJsonObject,
	) {
		let mut batch = database::rocksdb::WriteBatch::default();

		let event_id_bytes = event_id.as_bytes();
		self.eventid_pduid
			.insert_into_batch(&mut batch, &event_id_bytes, pdu_id);

		// CLEANUP: Drop the room outlier index to prevent ghosts during
		// reorder-timeline
		let room_id_from_json = json
			.get("room_id")
			.and_then(ruma::CanonicalJsonValue::as_str)
			.and_then(|r| <&RoomId>::try_from(r).ok());

		let room_id = room_id_from_json.map(ToOwned::to_owned).or_else(|| {
			(json.get("type").and_then(ruma::CanonicalJsonValue::as_str) == Some("m.room.create"))
				.then(|| event_id.as_str().replace('$', "!"))
				.and_then(|r| ruma::OwnedRoomId::parse(r).ok())
		});

		if let Some(room) = room_id {
			let mut key = room.as_bytes().to_vec();
			key.push(0xFF);
			key.extend_from_slice(event_id_bytes);
			self.roomid_outliereventid
				.remove_from_batch(&mut batch, &key);
		}

		// --- Phase 1: Double-Write ---
		self.eventid_pdu
			.raw_put_into_batch(&mut batch, event_id_bytes, Json(json));
		self.room_pducount_eventid
			.insert_into_batch(&mut batch, pdu_id, event_id_bytes);

		// Backfilled PDUs don't have full event structs readily available here,
		// but we can parse enough to populate the metadata.
		if let Ok(pdu) = serde_json::from_value::<PduEvent>(serde_json::to_value(json).unwrap()) {
			let existing_metadata =
				if let Ok(bytes) = self.eventid_metadata.get_blocking(event_id_bytes) {
					bincode::deserialize::<rooms::timeline::EventMetadata>(&bytes).ok()
				} else {
					None
				};

			let local_topological_depth = existing_metadata.as_ref().map_or_else(
				|| {
					let mut max_depth = 0;
					for prev_id in pdu.prev_events() {
						if let Ok(bytes) = self.eventid_metadata.get_blocking(prev_id.as_bytes())
						{
							if let Ok(meta) =
								bincode::deserialize::<rooms::timeline::EventMetadata>(&bytes)
							{
								max_depth = max_depth.max(meta.local_topological_depth);
							}
						}
					}
					max_depth.saturating_add(1)
				},
				|m| m.local_topological_depth,
			);

			let topo_key = Self::topo_pducount_key(pdu_id, local_topological_depth);
			self.roomid_topologicalorder_pducount.insert_into_batch(
				&mut batch,
				&topo_key,
				event_id_bytes,
			);

			let metadata = rooms::timeline::EventMetadata {
				short_room_id: u64::from_be_bytes(pdu_id.shortroomid()),
				is_outlier: false,
				origin_server_ts: pdu.origin_server_ts().0,
				depth: pdu.depth(),
				soft_failed: existing_metadata.as_ref().is_some_and(|m| m.soft_failed),
				rejected: pdu.rejected(),
				redacted_by: pdu.redacts().map(ToOwned::to_owned),
				short_state_hash: existing_metadata.and_then(|m| m.short_state_hash),
				local_topological_depth,
			};
			if let Ok(metadata_bytes) = bincode::serialize(&metadata) {
				self.eventid_metadata.insert_into_batch(
					&mut batch,
					event_id_bytes,
					metadata_bytes,
				);
			}
		}
		self.eventid_pdu.apply_batch(&batch);
		self.room_pducount_eventid.wake(pdu_id);
		self.eventid_pdu.wake(event_id_bytes);
	}

	/// Removes a pdu and creates a new one with the same id.
	pub(super) async fn replace_pdu(
		&self,
		pdu_id: &RawPduId,
		pdu_json: &CanonicalJsonObject,
		event_id: &EventId,
	) -> Result {
		if self.room_pducount_eventid.get(pdu_id).await.is_not_found() {
			return Err!(Request(NotFound("PDU does not exist.")));
		}

		let mut batch = database::rocksdb::WriteBatch::default();

		let event_id_bytes = event_id.as_bytes();

		// --- Phase 1: Double-Write ---
		self.eventid_pdu
			.raw_put_into_batch(&mut batch, event_id_bytes, Json(pdu_json));

		if let Ok(pdu) =
			serde_json::from_value::<PduEvent>(serde_json::to_value(pdu_json).unwrap())
		{
			let existing_metadata =
				if let Ok(bytes) = self.eventid_metadata.get(event_id_bytes).await {
					bincode::deserialize::<rooms::timeline::EventMetadata>(&bytes).ok()
				} else {
					None
				};

			let local_topological_depth = existing_metadata.as_ref().map_or_else(
				|| {
					let mut max_depth = 0;
					for prev_id in pdu.prev_events() {
						if let Ok(bytes) = self.eventid_metadata.get_blocking(prev_id.as_bytes())
						{
							if let Ok(meta) =
								bincode::deserialize::<rooms::timeline::EventMetadata>(&bytes)
							{
								max_depth = max_depth.max(meta.local_topological_depth);
							}
						}
					}
					max_depth.saturating_add(1)
				},
				|m| m.local_topological_depth,
			);

			let metadata = rooms::timeline::EventMetadata {
				short_room_id: u64::from_be_bytes(pdu_id.shortroomid()),
				is_outlier: false,
				origin_server_ts: pdu.origin_server_ts().0,
				depth: pdu.depth(),
				soft_failed: existing_metadata.as_ref().is_some_and(|m| m.soft_failed),
				rejected: pdu.rejected(),
				redacted_by: pdu.redacts().map(ToOwned::to_owned),
				short_state_hash: existing_metadata.and_then(|m| m.short_state_hash),
				local_topological_depth,
			};
			if let Ok(metadata_bytes) = bincode::serialize(&metadata) {
				self.eventid_metadata.insert_into_batch(
					&mut batch,
					event_id_bytes,
					metadata_bytes,
				);
			}
		}

		self.eventid_pdu.apply_batch(&batch);
		self.room_pducount_eventid.wake(pdu_id);
		self.eventid_pdu.wake(event_id_bytes);
		Ok(())
	}

	/// Returns an iterator over all events and their tokens in a room that
	/// happened before the event with id `until` in reverse-chronological
	/// order.
	pub(super) fn pdus_rev<'a>(
		&'a self,
		room_id: &'a RoomId,
		until: PduCount,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		use conduwuit::utils::stream::TryWidebandExt;

		self.count_to_id(room_id, until.saturating_inc(Direction::Backward), Direction::Backward)
			.map_ok(move |current| {
				let prefix = current.shortroomid();
				self.room_pducount_eventid
					.rev_raw_stream_from(&current)
					.ready_try_take_while(move |(key, _)| Ok(key.starts_with(&prefix)))
					.wide_and_then(move |(pdu_id, event_id_bytes)| async move {
						let json_bytes = self.eventid_pdu.get(&event_id_bytes).await?;
						Self::parse_json_slice(None, (pdu_id, json_bytes.as_ref()))
					})
			})
			.try_flatten_stream()
	}

	pub(super) fn pdus<'a>(
		&'a self,
		room_id: &'a RoomId,
		from: PduCount,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		use conduwuit::utils::stream::TryWidebandExt;

		self.count_to_id(room_id, from.saturating_inc(Direction::Forward), Direction::Forward)
			.map_ok(move |current| {
				let prefix = current.shortroomid();
				self.room_pducount_eventid
					.raw_stream_from(&current)
					.ready_try_take_while(move |(key, _)| Ok(key.starts_with(&prefix)))
					.wide_and_then(move |(pdu_id, event_id_bytes)| async move {
						let json_bytes = self.eventid_pdu.get(&event_id_bytes).await?;
						Self::parse_json_slice(None, (pdu_id, json_bytes.as_ref()))
					})
			})
			.try_flatten_stream()
	}

	pub(super) fn topo_pdus_rev<'a>(
		&'a self,
		room_id: &'a RoomId,
		until: PduCount,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.count_to_id(room_id, until.saturating_inc(Direction::Backward), Direction::Backward)
			.and_then(move |current| async move {
				let prefix = current.shortroomid();
				let topo_key = self
					.seek_topo_key(room_id, until, &current, Direction::Backward)
					.await?;

				let stream = self
					.roomid_topologicalorder_pducount
					.rev_raw_stream_from(&topo_key);
				Ok(self.parse_topo_stream(stream, prefix.to_vec()))
			})
			.try_flatten_stream()
	}

	pub(super) fn topo_pdus<'a>(
		&'a self,
		room_id: &'a RoomId,
		from: PduCount,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.count_to_id(room_id, from.saturating_inc(Direction::Forward), Direction::Forward)
			.and_then(move |current| async move {
				let prefix = current.shortroomid();
				let topo_key = self
					.seek_topo_key(room_id, from, &current, Direction::Forward)
					.await?;

				let stream = self
					.roomid_topologicalorder_pducount
					.raw_stream_from(&topo_key);
				Ok(self.parse_topo_stream(stream, prefix.to_vec()))
			})
			.try_flatten_stream()
	}

	fn parse_json_slice(
		room_id: Option<&RoomId>,
		(pdu_id, pdu): KeyVal<'_>,
	) -> Result<PdusIterItem> {
		let pdu_id: RawPduId = pdu_id.into();
		let pdu = match serde_json::from_slice::<PduEvent>(pdu) {
			| Ok(p) => p,
			| Err(e) => {
				conduwuit::warn!(
					"parse_json_slice failed: {e}. JSON: {}",
					String::from_utf8_lossy(pdu)
				);
				return Err(e.into());
			},
		};

		if let Some(expected_room) = room_id {
			let actual_room = pdu.room_id_or_hash();
			if let Some(actual_room) = actual_room {
				if actual_room != expected_room {
					return Err(conduwuit::err!(Database(
						"PDU does belong to room {actual_room} (expected {expected_room})"
					)));
				}
			} else {
				// v12 hashed-room PDUs may not contain room_id in the JSON.
				// We do not have ShortRoomId here for the expected room, but
				// we are called from an iterator that already filtered by it.
			}
		}

		Ok((pdu_id.pdu_count(), pdu))
	}

	pub(super) fn increment_notification_counts(
		&self,
		room_id: &RoomId,
		notifies: Vec<OwnedUserId>,
		highlights: Vec<OwnedUserId>,
	) {
		let _cork = self.db.cork();

		for user in notifies {
			let mut userroom_id = user.as_bytes().to_vec();
			userroom_id.push(0xFF);
			userroom_id.extend_from_slice(room_id.as_bytes());
			increment(&self.userroomid_notificationcount, &userroom_id);
		}

		for user in highlights {
			let mut userroom_id = user.as_bytes().to_vec();
			userroom_id.push(0xFF);
			userroom_id.extend_from_slice(room_id.as_bytes());
			increment(&self.userroomid_highlightcount, &userroom_id);
		}
	}

	async fn count_to_id(
		&self,
		room_id: &RoomId,
		shorteventid: PduCount,
		_dir: Direction,
	) -> Result<RawPduId> {
		let shortroomid: ShortRoomId = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

		let pdu_id = PduId { shortroomid, shorteventid };

		Ok(pdu_id.into())
	}

	async fn seek_topo_key(
		&self,
		room_id: &RoomId,
		token: PduCount,
		current: &RawPduId,
		dir: Direction,
	) -> Result<Vec<u8>> {
		use futures::StreamExt;

		if token == PduCount::max() {
			Ok(Self::topo_pducount_key(current, u64::MAX))
		} else if token == PduCount::min() {
			Ok(Self::topo_pducount_key(current, 0))
		} else {
			let token_pdu_id = self.count_to_id(room_id, token, dir).await?;
			if let Ok(mut key) = self.pdu_id_to_topo_key(&token_pdu_id).await {
				key[16..24].copy_from_slice(&current.as_ref()[8..]);
				return Ok(key);
			}

			// Fallback: find the nearest existing event in the requested direction
			let prefix = current.shortroomid();

			let nearest_pdu_id = if dir == Direction::Forward {
				let mut stream = Box::pin(
					self.room_pducount_eventid
						.raw_stream_from(&token_pdu_id)
						.ready_try_take_while(|(k, _)| Ok(k.starts_with(&prefix))),
				);
				stream
					.next()
					.await
					.map(|res| res.map(|(k, _)| RawPduId::from(k)))
			} else {
				let mut stream = Box::pin(
					self.room_pducount_eventid
						.rev_raw_stream_from(&token_pdu_id)
						.ready_try_take_while(|(k, _)| Ok(k.starts_with(&prefix))),
				);
				stream
					.next()
					.await
					.map(|res| res.map(|(k, _)| RawPduId::from(k)))
			};

			if let Some(Ok(nearest_id)) = nearest_pdu_id {
				let mut key = self.pdu_id_to_topo_key(&nearest_id).await?;
				key[16..24].copy_from_slice(&current.as_ref()[8..]);
				Ok(key)
			} else if dir == Direction::Forward {
				Ok(Self::topo_pducount_key(current, u64::MAX))
			} else {
				Ok(Self::topo_pducount_key(current, 0))
			}
		}
	}

	fn parse_topo_stream<'a>(
		&'a self,
		stream: impl Stream<Item = Result<KeyVal<'a>>> + Send + 'a,
		prefix: Vec<u8>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		use conduwuit::utils::stream::TryWidebandExt;

		stream
			.ready_try_take_while(move |(key, _)| Ok(key.starts_with(&prefix)))
			.wide_and_then(move |(topo_key, event_id_bytes)| async move {
				let pdu_id = Self::topo_key_to_pdu_id(topo_key);
				let json_bytes = self.eventid_pdu.get(event_id_bytes).await?;
				Self::parse_json_slice(None, (pdu_id.as_ref(), json_bytes.as_ref()))
			})
	}
}

//TODO: this is an ABA
fn increment(db: &Arc<Map>, key: &[u8]) {
	let old = db.get_blocking(key);
	let new = utils::increment(old.ok().as_deref());
	db.insert(key, new);
}
