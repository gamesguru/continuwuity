use std::sync::Arc;

use conduwuit::{
	Err, Event, PduCount, PduEvent, Result, at, err,
	result::NotFound,
	utils::{self, stream::TryReadyExt},
};
use database::{Database, Deserialized, Json, KeyVal, Map};
use futures::{FutureExt, Stream, TryFutureExt, TryStreamExt, future::select_ok, pin_mut};
use ruma::{CanonicalJsonObject, EventId, OwnedEventId, OwnedUserId, RoomId, api::Direction};
use serde::{Deserialize, Serialize};

use super::{PduId, RawPduId};
use crate::{Dep, rooms, rooms::short::ShortRoomId};

pub(super) struct Data {
	eventid_outlierpdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	pduid_pdu: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
	pub(super) eventid_statejumppointers: Arc<Map>,
	pub(super) db: Arc<Database>,
	services: Services,
}

struct Services {
	short: Dep<rooms::short::Service>,
}

pub type PdusIterItem = (PduCount, PduEvent);

/// State DAG jump pointer data stored per state event (MSC4242).
///
/// Each state event stores its absolute depth in the State DAG
/// (distinct from the Event DAG depth) along with binary-lifted
/// jump pointers: `jumps[k]` points to the 2^k-th ancestor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StateJumpData {
	pub depth: u64,
	pub jumps: Vec<OwnedEventId>,
}

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			eventid_outlierpdu: db["eventid_outlierpdu"].clone(),
			eventid_pduid: db["eventid_pduid"].clone(),
			pduid_pdu: db["pduid_pdu"].clone(),
			userroomid_highlightcount: db["userroomid_highlightcount"].clone(),
			userroomid_notificationcount: db["userroomid_notificationcount"].clone(),
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

		if pdu.state_key().is_some() && pdu.prev_state_events().is_some() {
			if let Err(e) = self.calculate_and_persist_jump_pointers(pdu).await {
				conduwuit::warn!(
					"Failed to calculate jump pointers for state event {}: {}",
					pdu.event_id,
					e
				);
			}
		}
	}

	/// Calculate and persist binary-lifted jump pointers with depth tracking
	/// for a state event in the State DAG (MSC4242).
	///
	/// Jump pointers enable O(log N) LCA queries. Each entry `jumps[k]`
	/// points to the 2^k-th ancestor in the State DAG.
	pub(super) async fn calculate_and_persist_jump_pointers(&self, pdu: &PduEvent) -> Result<()> {
		// Grab the first prev_state_event as our tree parent.
		// If there are multiple (merge), picking one still enables a sublinear walk.
		let Some(parent_id) = pdu.prev_state_events().and_then(|mut iter| iter.next()) else {
			// Root event (create) — depth 0, no jumps
			let data = StateJumpData { depth: 0, jumps: vec![] };
			self.eventid_statejumppointers.insert(
				pdu.event_id.as_bytes(),
				serde_json::to_vec(&data).expect("StateJumpData serializes"),
			);
			return Ok(());
		};

		// Parent's depth + 1
		let parent_data = self
			.get_jump_data(parent_id)
			.await
			.unwrap_or(StateJumpData { depth: 0, jumps: vec![] });
		let my_depth = parent_data.depth.saturating_add(1);

		// Build jump table: jumps[k] = 2^k-th ancestor
		let mut jumps: Vec<OwnedEventId> = vec![parent_id.into()];
		let mut k = 0;

		while let Some(ancestor_id) = jumps.get(k).cloned() {
			if let Ok(ancestor_data) = self.get_jump_data(&ancestor_id).await {
				if let Some(next) = ancestor_data.jumps.get(k) {
					jumps.push(next.clone());
				} else {
					break;
				}
			} else {
				break;
			}
			k = k.saturating_add(1);
		}

		let data = StateJumpData { depth: my_depth, jumps };
		self.eventid_statejumppointers.insert(
			pdu.event_id.as_bytes(),
			serde_json::to_vec(&data).expect("StateJumpData serializes"),
		);
		Ok(())
	}

	/// Retrieve the State DAG jump data for an event.
	pub(super) async fn get_jump_data(&self, event_id: &EventId) -> Result<StateJumpData> {
		self.eventid_statejumppointers
			.get(event_id.as_bytes())
			.await
			.deserialized()
	}

	/// Finds the Lowest Common Ancestor (LCA) of two state events in O(log N)
	/// time using pre-calculated binary-lifted jump pointers.
	///
	/// Requires that both events have persisted `StateJumpData` with correct
	/// depth values. The algorithm:
	/// 1. Align both nodes to the same depth by lifting the deeper one.
	/// 2. If they're the same node, return it.
	/// 3. Binary lift both in parallel to find the last point of divergence.
	/// 4. Return their common parent.
	#[allow(dead_code)]
	pub(super) async fn find_lca(
		&self,
		mut a: OwnedEventId,
		mut b: OwnedEventId,
	) -> Result<Option<OwnedEventId>> {
		let mut data_a = self.get_jump_data(&a).await?;
		let mut data_b = self.get_jump_data(&b).await?;

		// Step 1: Align depths — lift the deeper node
		if data_a.depth > data_b.depth {
			let result = self.lift_to_depth(a, data_a, data_b.depth).await?;
			a = result.0;
			data_a = result.1;
		} else if data_b.depth > data_a.depth {
			let result = self.lift_to_depth(b, data_b, data_a.depth).await?;
			b = result.0;
			data_b = result.1;
		}

		// Step 2: If already the same event, return it
		if a == b {
			return Ok(Some(a));
		}

		// Step 3: Binary lift in parallel — find the last point where they differ
		let max_k = data_a.jumps.len().max(data_b.jumps.len());
		for k in (0..max_k).rev() {
			let jump_a = data_a.jumps.get(k);
			let jump_b = data_b.jumps.get(k);
			if let (Some(ja), Some(jb)) = (jump_a, jump_b) {
				if ja != jb {
					a = ja.clone();
					b = jb.clone();
					data_a = self.get_jump_data(&a).await?;
					data_b = self.get_jump_data(&b).await?;
				}
			}
		}

		// Step 4: a and b are now children of the LCA — return their parent
		Ok(data_a.jumps.first().cloned())
	}

	/// Lift an event to a target depth using binary decomposition of the
	/// depth difference. E.g., if diff=5 (binary 101), jump 2^0 then 2^2.
	#[allow(dead_code)]
	async fn lift_to_depth(
		&self,
		mut id: OwnedEventId,
		mut data: StateJumpData,
		target_depth: u64,
	) -> Result<(OwnedEventId, StateJumpData)> {
		let mut diff = data.depth.saturating_sub(target_depth);
		let mut k = 0;
		while diff > 0 {
			if diff & 1 == 1 {
				if let Some(jump) = data.jumps.get(k) {
					id = jump.clone();
					data = self.get_jump_data(&id).await?;
				} else {
					// Jump table incomplete — cannot lift further
					break;
				}
			}
			diff >>= 1;
			k = k.saturating_add(1);
		}
		Ok((id, data))
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
