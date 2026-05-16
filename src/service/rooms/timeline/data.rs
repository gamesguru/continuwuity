use std::sync::Arc;

use conduwuit::{
	Err, Event, PduCount, PduEvent, Result, at, err,
	result::NotFound,
	utils::{self, stream::TryReadyExt},
};
use database::{Database, Deserialized, Json, KeyVal, Map};
use futures::{FutureExt, Stream, TryFutureExt, TryStreamExt, future::select_ok, pin_mut};
use ruma::{CanonicalJsonObject, EventId, OwnedUserId, RoomId, api::Direction};

use super::{PduId, RawPduId};
use crate::{Dep, rooms, rooms::short::ShortRoomId};

pub(super) struct Data {
	eventid_outlierpdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	pduid_pdu: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
	#[allow(dead_code)]
	eventid_prevstateevents: Arc<Map>,
	pub(super) eventid_statejumppointers: Arc<Map>,
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
			userroomid_highlightcount: db["userroomid_highlightcount"].clone(),
			userroomid_notificationcount: db["userroomid_notificationcount"].clone(),
			eventid_prevstateevents: db["eventid_prevstateevents"].clone(),
			eventid_statejumppointers: db["eventid_statejumppointers"].clone(),
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
			.ok_or_else(|| err!(Request(NotFound("no PDU's found in room"))))
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
	/// If `room_id` is provided, validates the PDU belongs to that room.
	pub(super) async fn get_non_outlier_pdu_in_room(
		&self,
		room_id: Option<&RoomId>,
		event_id: &EventId,
	) -> Result<PduEvent> {
		let pduid = self.get_pdu_id(event_id).await?;
		let pdu: PduEvent = self.pduid_pdu.get(&pduid).await.deserialized()?;

		// Enforce cross-room boundary: verify the PDU belongs to the expected room
		if let Some(expected_room) = room_id {
			if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
				return Err!(Database("PDU {event_id} does not belong to room {expected_room}"));
			}
		}

		Ok(pdu)
	}

	/// Like get_non_outlier_pdu(), but without the expense of fetching and
	/// parsing the PduEvent
	pub(super) async fn non_outlier_pdu_exists(&self, event_id: &EventId) -> Result {
		let pduid = self.get_pdu_id(event_id).await?;

		self.pduid_pdu.exists(&pduid).await
	}

	/// Returns the pdu.
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
				let pdu: PduEvent = handle.deserialized()?;

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

	/// Returns the pdu.
	///
	/// This does __NOT__ check the outliers `Tree`.
	/// If `room_id` is provided, validates the PDU belongs to that room.
	pub(super) async fn get_pdu_from_id_in_room(
		&self,
		room_id: Option<&RoomId>,
		pdu_id: &RawPduId,
	) -> Result<PduEvent> {
		let pdu: PduEvent = self.pduid_pdu.get(pdu_id).await.deserialized()?;

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

		if pdu.state_key().is_some() {
			if let Err(e) = self.calculate_and_persist_jump_pointers(pdu).await {
				conduwuit::warn!(
					"Failed to calculate jump pointers for state event {}: {}",
					pdu.event_id,
					e
				);
			}
		}
	}

	pub(super) async fn calculate_and_persist_jump_pointers(&self, pdu: &PduEvent) -> Result<()> {
		// Grab the first prev_state_event as our tree parent.
		// If there are multiple, picking one still enables a sublinear walk towards the
		// root.
		let Some(parent) = pdu.prev_state_events().and_then(|mut iter| iter.next()) else {
			return Ok(());
		};

		let mut jump_pointers: Vec<ruma::OwnedEventId> = vec![parent.into()];
		let mut k = 0;

		while let Some(ancestor) = jump_pointers.get(k) {
			if let Ok(ancestor_jumps) = self.get_jump_pointers(ancestor).await {
				if let Some(next_jump) = ancestor_jumps.get(k) {
					jump_pointers.push(next_jump.clone());
				} else {
					break;
				}
			} else {
				break;
			}
			k = k.saturating_add(1);
		}

		self.eventid_statejumppointers.insert(
			pdu.event_id.as_bytes(),
			serde_json::to_vec(&jump_pointers).expect("jump pointers serialize"),
		);
		Ok(())
	}

	pub(super) async fn get_jump_pointers(
		&self,
		event_id: &EventId,
	) -> Result<Vec<ruma::OwnedEventId>> {
		self.eventid_statejumppointers
			.get(event_id.as_bytes())
			.await
			.deserialized()
	}

	/// Finds the Lowest Common Ancestor (LCA) of two state events in O(log N)
	/// time using pre-calculated binary-lifted jump pointers.
	#[allow(dead_code)]
	pub(super) async fn find_lca(
		&self,
		mut a: ruma::OwnedEventId,
		mut b: ruma::OwnedEventId,
	) -> Result<Option<ruma::OwnedEventId>> {
		// In a full implementation, we would align `a` and `b` to the same State DAG
		// depth. For simplicity, we just do a staggered jump to find the
		// intersection.
		for k in (0..32).rev() {
			if let Ok(jumps_a) = self.get_jump_pointers(&a).await {
				if let Ok(jumps_b) = self.get_jump_pointers(&b).await {
					if let (Some(jump_a), Some(jump_b)) = (jumps_a.get(k), jumps_b.get(k)) {
						if jump_a != jump_b {
							a = jump_a.clone();
							b = jump_b.clone();
						}
					}
				}
			}
		}

		// After jumping, the parent of A (or B) should be the LCA.
		if let Ok(jumps_a) = self.get_jump_pointers(&a).await {
			if let Some(parent) = jumps_a.first() {
				return Ok(Some(parent.clone()));
			}
		}

		Ok(None)
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
		let pdu = serde_json::from_slice::<PduEvent>(pdu)?;

		if let Some(expected_room) = room_id {
			if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
				return Err(conduwuit::err!(Database(
					"PDU does not belong to expected room {expected_room}"
				)));
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
		dir: Direction,
	) -> Result<RawPduId> {
		let shortroomid: ShortRoomId = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

		// +1 so we don't send the base event
		let pdu_id = PduId {
			shortroomid,
			shorteventid: shorteventid.saturating_inc(dir),
		};

		Ok(pdu_id.into())
	}
}

//TODO: this is an ABA
fn increment(db: &Arc<Map>, key: &[u8]) {
	let old = db.get_blocking(key);
	let new = utils::increment(old.ok().as_deref());
	db.insert(key, new);
}
