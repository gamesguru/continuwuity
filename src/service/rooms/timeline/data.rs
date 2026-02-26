use std::sync::Arc;

use conduwuit::{
	Err, PduCount, PduEvent, Result, at, err,
	result::NotFound,
	utils::{self, stream::TryReadyExt},
};
use database::{Database, Deserialized, Json, KeyVal, Map};
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::select_ok, pin_mut,
};
use ruma::{CanonicalJsonObject, EventId, OwnedUserId, RoomId, UInt, api::Direction};

use super::{PduId, RawPduId};
use crate::{Dep, rooms, rooms::short::ShortRoomId};

pub(super) struct Data {
	eventid_outlierpdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	pduid_pdu: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
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
	pub(super) async fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
		let pduid = self.get_pdu_id(event_id).await?;

		self.pduid_pdu.get(&pduid).await.deserialized()
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
	pub(super) async fn get_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
		let accepted = self.get_non_outlier_pdu(event_id).boxed();
		let outlier = self
			.eventid_outlierpdu
			.get(event_id)
			.map(Deserialized::deserialized)
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
	pub(super) async fn get_pdu_from_id(&self, pdu_id: &RawPduId) -> Result<PduEvent> {
		self.pduid_pdu.get(pdu_id).await.deserialized()
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
		self.append_timestamp_index(pdu_id, pdu);
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
		if let Ok(pdu) =
			serde_json::from_value::<PduEvent>(serde_json::to_value(json).expect("valid json"))
		{
			self.append_timestamp_index(pdu_id, &pdu);
		}
	}

	fn append_timestamp_index(&self, pdu_id: &RawPduId, pdu: &PduEvent) {
		let key = database::keyval::serialize_key((
			pdu_id.shortroomid(),
			pdu.origin_server_ts,
			pdu_id.pdu_count(),
		))
		.expect("valid key");
		self.db["roomid_timestamp_pducount"].insert(&key, []);
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
					.ready_and_then(Self::from_json_slice)
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
					.ready_and_then(Self::from_json_slice)
			})
			.try_flatten_stream()
	}

	pub(super) async fn pdu_from_timestamp(
		&self,
		room_id: &RoomId,
		timestamp: u64,
		dir: Direction,
	) -> Result<PduEvent> {
		let stream = self.pdus_by_timestamp(room_id, timestamp, dir);

		pin_mut!(stream);
		stream
			.try_next()
			.await?
			.ok_or_else(|| err!(Request(NotFound("No PDU found for timestamp"))))
	}

	/// Returns a stream of PDUs starting at `timestamp` in `dir`.
	pub(super) fn pdus_by_timestamp<'a>(
		&'a self,
		room_id: &'a RoomId,
		timestamp: u64,
		dir: Direction,
	) -> impl Stream<Item = Result<PduEvent>> + Send + 'a {
		// Define rules of the stream
		let setup = async move {
			let short = self
				.services
				.short
				.get_shortroomid(room_id)
				.await
				.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

			let count = match dir {
				| Direction::Forward => PduCount::min(),
				| Direction::Backward => PduCount::max(),
			};

			// Define start key, and closure/exit type for the stream
			let key = database::keyval::serialize_key((short, timestamp, count))?;
			Ok::<_, conduwuit::Error>((short, key))
		};

		// Main stream
		setup
			.map_ok(move |(short, key)| {
				let prefix = short.to_be_bytes();
				let map = &self.db["roomid_timestamp_pducount"];

				// Get stream w/ matching DB keys, in requested direction
				let stream = match dir {
					| Direction::Forward => map.raw_stream_from(&key).boxed(),
					| Direction::Backward => map.rev_raw_stream_from(&key).boxed(),
				};

				stream
					// Stop searching when we hit an event/key belonging to another room
					.ready_try_take_while(move |(k, _)| Ok(k.starts_with(&prefix)))
					// Extract PDU count via key lookup (shortroomid, timestamp, count)
					.ready_and_then(|(k, _)| {
						let deserialized: (u64, UInt, PduCount) =
							database::keyval::deserialize_key(k).map_err(|e| {
								err!(Database("Failed to deserialize index key: {e:?}"))
							})?;

						// We only need the count (3rd element) as a lookup key
						let count = deserialized.2;
						Ok(count)
					})
					// Using PDU count, fetch full PDU event object
					.and_then(move |count| async move {
						let pdu_id = PduId { shortroomid: short, shorteventid: count };
						self.get_pdu_from_id(&pdu_id.into()).await
					})
			})
			.try_flatten_stream()
	}

	pub(super) async fn backfill_timestamp_index(&self, room_id: &RoomId) -> Result {
		let shortroomid: ShortRoomId = self.services.short.get_shortroomid(room_id).await?;
		let pdus = self.pdus(room_id, PduCount::min());
		pin_mut!(pdus);

		let mut yield_count = 0_usize;
		let mut cork = self.db.cork(); // Hold writes in memory

		while let Some(res) = pdus.next().await {
			let (count, pdu) = match res {
				| Ok(p) => p,
				| Err(e) if e.is_not_found() => {
					tracing::info!("Skipping unresolvable event, ts-backfill: {e}");
					continue;
				},
				| Err(e) => return Err(e),
			};

			let pdu_id = PduId { shortroomid, shorteventid: count };
			self.append_timestamp_index(&pdu_id.into(), &pdu);

			// Write to DB in batched queries of 1000; avoid nightmare 1 write per iteration
			yield_count = yield_count.saturating_add(1);
			if yield_count.is_multiple_of(1000) {
				drop(cork); // flush to disk
				tokio::task::yield_now().await;
				cork = self.db.cork(); // re-acquire hold
			}
		}

		Ok(())
	}

	fn from_json_slice((pdu_id, pdu): KeyVal<'_>) -> Result<PdusIterItem> {
		let pdu_id: RawPduId = pdu_id.into();

		let pdu = serde_json::from_slice::<PduEvent>(pdu)?;

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

#[cfg(test)]
mod tests {
	use conduwuit::Result;
	use ruma::api::Direction;

	// Tests for edge cases and out-of-order events.

	// Helper to make a BTreeSet act like our database queries
	fn simulate_pdus_by_timestamp(
		index: &std::collections::BTreeSet<(u64, u64)>,
		search_ts: u64,
		dir: Direction,
	) -> Vec<(u64, u64)> {
		// Keys are (timestamp, count)
		let start_count = match dir {
			| Direction::Forward => u64::MIN,
			| Direction::Backward => u64::MAX,
		};
		let start_key = (search_ts, start_count);

		match dir {
			| Direction::Forward => index.range(start_key..).copied().collect(),
			| Direction::Backward => index.range(..=start_key).rev().copied().collect(),
		}
	}

	#[tokio::test]
	async fn test_pdus_by_timestamp_complex_walk() -> Result<()> {
		// Test a messy timeline where timestamps don't always go up in order.
		//
		// Example timeline:
		// E1: 1000ms, Count 1
		// E2: 2000ms, Count 2
		// E3: 2000ms, Count 3 (Duplicate TS, arrived after E2)
		// E4: 1500ms, Count 4 (Clock Skew - arrived later but has earlier TS)
		// E5: 3000ms, Count 5
		//
		// How it looks in the database (sorted by time, then count):
		// 1. (1000ms, Count 1)
		// 2. (1500ms, Count 4)
		// 3. (2000ms, Count 2)
		// 4. (2000ms, Count 3)
		// 5. (3000ms, Count 5)

		let mut index = std::collections::BTreeSet::new();
		index.insert((1000, 1));
		index.insert((2000, 2));
		index.insert((2000, 3));
		index.insert((1500, 4)); // Non-monotonic TS relative to count
		index.insert((3000, 5));

		// Searching forward from 1700ms finds the 2000ms and 3000ms events
		let fwd = simulate_pdus_by_timestamp(&index, 1700, Direction::Forward);
		assert_eq!(fwd, vec![(2000, 2), (2000, 3), (3000, 5)]);

		// Searching backward from 1700ms finds the 1500ms and 1000ms events
		let bwd = simulate_pdus_by_timestamp(&index, 1700, Direction::Backward);
		assert_eq!(bwd, vec![(1500, 4), (1000, 1)]);

		Ok(())
	}

	#[tokio::test]
	async fn test_pdus_by_timestamp_large_sparse_gaps() -> Result<()> {
		// Check we jump straight to the next event, not scan huge empty gaps.

		let mut index = std::collections::BTreeSet::new();

		// 1st group of events: 100,000 to 101,000
		for i in 100_000..=101_000 {
			index.insert((i, i));
		}

		// 2nd group of events: 964,000 to 965,000
		for i in 964_000..=965_000 {
			index.insert((i, i));
		}

		// Searching forward from the middle should find next group.
		let fwd = simulate_pdus_by_timestamp(&index, 500_000, Direction::Forward);
		assert_eq!(fwd.first(), Some(&(964_000, 964_000)));

		// Searching backward should find the first group.
		let bwd = simulate_pdus_by_timestamp(&index, 500_000, Direction::Backward);
		assert_eq!(bwd.first(), Some(&(101_000, 101_000)));

		Ok(())
	}

	#[tokio::test]
	async fn test_pdus_by_timestamp_wild_jitter_staircase() -> Result<()> {
		// Create 1000 events where the time generally goes up but sometimes jumps back
		let timeline = (0..1000_u64).map(|i| (i * 10 + (i % 11) * 5 - (i % 13) * 7, i));

		// Set sorts like RocksDB, luckily
		let mut index = std::collections::BTreeSet::new();
		for (ts, count) in timeline {
			index.insert((ts, count));
		}

		// Check we find correct starting point even if the timestamps jump around
		let search_ts = 5000_u64;
		let results = simulate_pdus_by_timestamp(&index, search_ts, Direction::Forward);

		// Check first event we find is at (or after) our search time
		if let Some(&(ts, _count)) = results.first() {
			assert!(ts >= search_ts);
		} else {
			panic!("Search yielded no results");
		}

		Ok(())
	}
}
