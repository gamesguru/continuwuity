use std::sync::Arc;

use conduwuit::{
	Err, Event, PduCount, PduEvent, Result, at, err, result::NotFound, utils::stream::TryReadyExt,
};
use database::{Database, Deserialized, Get, Json, KeyVal, Map};
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::select_ok, pin_mut,
};
use ruma::{CanonicalJsonObject, EventId, OwnedEventId, RoomId, api::Direction};

use super::{PduId, RawPduId};
use crate::{Dep, rooms, rooms::short::ShortRoomId};

pub(super) struct Data {
	eventid_outlierpdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	pduid_pdu: Arc<Map>,
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
			eventid_outlierpdu: db["eventid_outlierpdu"].clone(),
			eventid_pduid: db["eventid_pduid"].clone(),
			pduid_pdu: db["pduid_pdu"].clone(),
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
		let outlier = self
			.eventid_outlierpdu
			.get(event_id)
			.map(Deserialized::deserialized)
			.boxed();

		select_ok([accepted, outlier]).await.map(at!(0))
	}

	/// Returns the json of a pdu.
	pub(super) async fn get_non_outlier_pdu_json(
		&self,
		event_id: &EventId,
	) -> Result<CanonicalJsonObject> {
		let pduid = self.get_pdu_id(event_id).await?;

		self.pduid_pdu.get(&pduid).await.deserialized()
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
	/// Populates room_id if missing and validates the PDU belongs to that room.
	pub(super) async fn get_non_outlier_pdu_in_room(
		&self,
		room_id: Option<&RoomId>,
		event_id: &EventId,
	) -> Result<PduEvent> {
		let pduid = self.get_pdu_id(event_id).await?;
		let mut pdu: PduEvent = self.pduid_pdu.get(&pduid).await.deserialized()?;

		if pdu.room_id.is_none() {
			if let Some(room_id) = room_id {
				pdu.room_id = Some(room_id.to_owned());
			}
		}

		// Enforce cross-room boundary: verify the PDU belongs to the expected room
		if let Some(expected_room) = room_id {
			if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
				return Err!(Database("PDU {event_id} does not belong to room {expected_room}"));
			}
		}

		Ok(pdu)
	}

	pub(super) fn multi_get_pdu_ids<'a, S>(
		&'a self,
		event_ids: S,
	) -> impl Stream<Item = Result<RawPduId>> + Send + 'a
	where
		S: Stream<Item = OwnedEventId> + Send + 'a,
	{
		event_ids
			.get(&self.eventid_pduid)
			.map(|handle| handle.map(|h| RawPduId::from(&*h)))
	}

	pub(super) fn multi_get_pdus<'a, S>(
		&'a self,
		room_id: Option<&'a RoomId>,
		pdu_ids: S,
	) -> impl Stream<Item = Result<PduEvent>> + Send + 'a
	where
		S: Stream<Item = RawPduId> + Send + 'a,
	{
		pdu_ids.get(&self.pduid_pdu).map(move |res| {
			let mut pdu: PduEvent = res.deserialized()?;
			if pdu.room_id.is_none() {
				if let Some(room_id) = room_id {
					pdu.room_id = Some(room_id.to_owned());
				}
			}

			if let Some(expected_room) = room_id {
				if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
					return Err!(Database("PDU does not belong to room {expected_room}"));
				}
			}
			Ok(pdu)
		})
	}

	/// Like get_non_outlier_pdu(), but without the expense of fetching and
	/// parsing the PduEvent
	pub(super) async fn non_outlier_pdu_exists(&self, event_id: &EventId) -> Result {
		let pduid = self.get_pdu_id(event_id).await?;

		self.pduid_pdu.exists(&pduid).await
	}

	/// Returns the pdu, populating room_id.
	///
	/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
	/// If `room_id` is provided, validates the PDU belongs to that room.
	pub(super) async fn get_pdu_in_room(
		&self,
		room_id: Option<&RoomId>,
		event_id: &EventId,
	) -> Result<PduEvent> {
		let accepted = self.get_non_outlier_pdu_in_room(room_id, event_id).boxed();
		let outlier = self
			.eventid_outlierpdu
			.get(event_id)
			.map(move |handle| {
				let mut pdu: PduEvent = handle.deserialized()?;

				if pdu.room_id.is_none() {
					if let Some(room_id) = room_id {
						pdu.room_id = Some(room_id.to_owned());
					}
				}

				// Enforce cross-room boundary
				if let Some(expected_room) = room_id {
					if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
						return Err(conduwuit::err!(Database(
							"Outlier PDU {event_id} does not belong to room {expected_room}"
						)));
					}
				}

				Ok(pdu)
			})
			.boxed();

		select_ok([accepted, outlier]).await.map(at!(0))
	}

	/// Like get_non_outlier_pdu(), but without the expense of fetching and
	/// parsing the PduEvent
	#[inline]
	pub(super) async fn outlier_pdu_exists(&self, event_id: &EventId) -> Result {
		self.eventid_outlierpdu.exists(event_id).await
	}

	/// Like get_pdu(), but without the expense of fetching and parsing the data
	pub(super) async fn pdu_exists(&self, event_id: &EventId) -> Result {
		let non_outlier = self.non_outlier_pdu_exists(event_id).boxed();
		let outlier = self.outlier_pdu_exists(event_id).boxed();

		select_ok([non_outlier, outlier]).await.map(at!(0))
	}

	/// Returns the pdu, populating room_id.
	///
	/// This does __NOT__ check the outliers `Tree`.
	/// If `room_id` is provided, validates the PDU belongs to that room.
	pub(super) async fn get_pdu_from_id_in_room(
		&self,
		room_id: Option<&RoomId>,
		pdu_id: &RawPduId,
	) -> Result<PduEvent> {
		let mut pdu: PduEvent = self.pduid_pdu.get(pdu_id).await.deserialized()?;

		if pdu.room_id.is_none() {
			if let Some(room_id) = room_id {
				pdu.room_id = Some(room_id.to_owned());
			}
		}

		if let Some(expected_room) = room_id {
			if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
				return Err!(Database("PDU does not belong to room {expected_room}"));
			}
		}

		Ok(pdu)
	}

	/// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
	pub(super) async fn get_pdu_json_from_id(
		&self,
		pdu_id: &RawPduId,
	) -> Result<CanonicalJsonObject> {
		self.pduid_pdu.get(pdu_id).await.deserialized()
	}

	pub(super) async fn append_pdu(
		&self,
		pdu_id: &RawPduId,
		pdu: &PduEvent,
		json: &CanonicalJsonObject,
		count: PduCount,
	) {
		debug_assert!(matches!(count, PduCount::Normal(_)), "PduCount not Normal");

		self.pduid_pdu.raw_put(pdu_id, Json(json));
		self.eventid_pduid.insert(pdu.event_id.as_bytes(), pdu_id);
		self.eventid_outlierpdu.remove(pdu.event_id.as_bytes());
	}

	pub(super) fn prepend_backfill_pdu(
		&self,
		pdu_id: &RawPduId,
		event_id: &EventId,
		json: &CanonicalJsonObject,
	) {
		self.pduid_pdu.raw_put(pdu_id, Json(json));
		self.eventid_pduid.insert(event_id, pdu_id);
		self.eventid_outlierpdu.remove(event_id);
	}

	/// Removes a pdu and creates a new one with the same id.
	pub(super) async fn replace_pdu(
		&self,
		pdu_id: &RawPduId,
		pdu_json: &CanonicalJsonObject,
	) -> Result {
		if self.pduid_pdu.get(pdu_id).await.is_not_found() {
			return Err!(Request(NotFound("PDU does not exist.")));
		}

		self.pduid_pdu.raw_put(pdu_id, Json(pdu_json));

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
		self.count_to_id(room_id, until, Direction::Backward)
			.map_ok(move |current| {
				let prefix = current.shortroomid();
				self.pduid_pdu
					.rev_raw_stream_from(&current)
					.ready_try_take_while(move |(key, _)| Ok(key.starts_with(&prefix)))
					.ready_and_then(move |kv| Self::parse_json_slice(Some(room_id), kv))
			})
			.try_flatten_stream()
	}

	pub(super) fn pdus<'a>(
		&'a self,
		room_id: &'a RoomId,
		from: PduCount,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.count_to_id(room_id, from, Direction::Forward)
			.map_ok(move |current| {
				let prefix = current.shortroomid();
				self.pduid_pdu
					.raw_stream_from(&current)
					.ready_try_take_while(move |(key, _)| Ok(key.starts_with(&prefix)))
					.ready_and_then(move |kv| Self::parse_json_slice(Some(room_id), kv))
			})
			.try_flatten_stream()
	}

	fn parse_json_slice(
		room_id: Option<&RoomId>,
		(pdu_id, pdu): KeyVal<'_>,
	) -> Result<PdusIterItem> {
		let pdu_id: RawPduId = pdu_id.into();
		let mut pdu = serde_json::from_slice::<PduEvent>(pdu)?;

		if pdu.room_id.is_none() {
			if let Some(room_id) = room_id {
				pdu.room_id = Some(room_id.to_owned());
			}
		}

		if let Some(expected_room) = room_id {
			if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
				return Err(conduwuit::err!(Database(
					"PDU does not belong to expected room {expected_room}"
				)));
			}
		}

		Ok((pdu_id.pdu_count(), pdu))
	}

	pub(super) async fn prev_timeline_count(&self, before: &PduId) -> Result<PduCount> {
		let before_pdu =
			Self::pdu_count_to_id(before.shortroomid, before.shorteventid, Direction::Backward)?;

		let prefix = before_pdu.shortroomid();
		let pdu_ids = self
			.pduid_pdu
			.rev_keys_raw_from(&before_pdu)
			.ready_try_take_while(move |pdu_bytes: &&[u8]| Ok(pdu_bytes.starts_with(&prefix)))
			.ready_and_then(|pdu_bytes: &[u8]| {
				let pdu_id = RawPduId::from(pdu_bytes);
				Ok(pdu_id.pdu_count())
			});

		pin_mut!(pdu_ids);
		pdu_ids
			.try_next()
			.await?
			.ok_or_else(|| err!(Request(NotFound("No earlier PDUs found in room"))))
	}

	pub(super) async fn next_timeline_count(&self, after: &PduId) -> Result<PduCount> {
		let after_pdu =
			Self::pdu_count_to_id(after.shortroomid, after.shorteventid, Direction::Forward)?;

		let prefix = after_pdu.shortroomid();
		let pdu_ids = self
			.pduid_pdu
			.keys_raw_from(&after_pdu)
			.ready_try_take_while(move |pdu_bytes: &&[u8]| Ok(pdu_bytes.starts_with(&prefix)))
			.ready_and_then(|pdu_bytes: &[u8]| {
				let pdu_id = RawPduId::from(pdu_bytes);
				Ok(pdu_id.pdu_count())
			});

		pin_mut!(pdu_ids);
		pdu_ids
			.try_next()
			.await?
			.ok_or_else(|| err!(Request(NotFound("No more PDUs found in room"))))
	}

	fn pdu_count_to_id(
		shortroomid: ShortRoomId,
		shorteventid: PduCount,
		dir: Direction,
	) -> Result<RawPduId> {
		// inc/sub to make the query exclusive (don't return the base event)
		let shorteventid = shorteventid
			.checked_inc(dir)
			.map_err(|_| err!(Request(NotFound("No more PDUs found in room"))))?;

		let pdu_id = PduId { shortroomid, shorteventid };

		Ok(pdu_id.into())
	}

	async fn count_to_id(
		&self,
		room_id: &RoomId,
		shorteventid: PduCount,
		dir: Direction,
	) -> Result<RawPduId> {
		let shortroomid: ShortRoomId = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

		Self::pdu_count_to_id(shortroomid, shorteventid, dir)
	}
}
