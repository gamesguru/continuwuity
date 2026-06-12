use std::sync::Arc;

use conduwuit::{
	Result, implement, info,
	matrix::{Event, PduEvent},
	utils::stream::{BroadbandExt, ReadyExt, TryIgnore},
};
use database::{Deserialized, Json, Map};
use futures::Stream;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId, RoomId};

use crate::{Dep, globals, rooms};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	eventid_receivecount: Arc<Map>,
	roomid_outliereventid: Arc<Map>,
	eventid_pdu: Arc<Map>,
	eventid_metadata: Arc<Map>,
}

struct Services {
	globals: Dep<globals::Service>,
	#[allow(dead_code)]
	timeline: Dep<rooms::timeline::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_receivecount: args.db["eventid_receivecount"].clone(),
				roomid_outliereventid: args.db["roomid_outliereventid"].clone(),
				eventid_pdu: args.db["eventid_pdu"].clone(),
				eventid_metadata: args.db["eventid_metadata"].clone(),
			},
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.db
		.eventid_pdu
		.get(event_id.as_bytes())
		.await
		.deserialized()
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_pdu_outlier(&self, event_id: &EventId) -> Result<PduEvent> {
	self.db
		.eventid_pdu
		.get(event_id.as_bytes())
		.await
		.deserialized()
}

#[implement(Service)]
pub fn stream_keys(&self) -> impl Stream<Item = OwnedEventId> + Send + '_ {
	self.db
		.eventid_metadata
		.stream::<OwnedEventId, rooms::timeline::EventMetadata>()
		.ignore_err()
		.broad_filter_map(|(eid, meta)| async move { meta.is_outlier.then_some(eid) })
}

#[implement(Service)]
pub fn stream(&self) -> impl Stream<Item = (OwnedEventId, PduEvent)> + Send + '_ {
	self.stream_keys().broad_filter_map(move |eid| async move {
		let pdu = self.get_pdu_outlier(&eid).await.ok()?;
		Some((eid, pdu))
	})
}

#[implement(Service)]
pub fn room_stream<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = (OwnedEventId, PduEvent)> + Send + 'a {
	let mut prefix = room_id.as_bytes().to_vec();
	prefix.push(0xFF);

	self.db
		.roomid_outliereventid
		.raw_stream_from(&prefix)
		.ignore_err()
		.ready_take_while(move |kv| kv.0.starts_with(&prefix))
		.broad_filter_map(move |kv| async move {
			let event_id_str = std::str::from_utf8(kv.1).ok()?;
			let event_id = OwnedEventId::try_from(event_id_str).ok()?;
			let pdu = self.get_pdu_outlier(&event_id).await.ok()?;
			Some((event_id, pdu))
		})
}

/// Returns the receive_count for an event, if it has been stamped.
#[implement(Service)]
pub async fn get_receive_count(&self, event_id: &EventId) -> Result<u64> {
	self.db
		.eventid_receivecount
		.get(event_id)
		.await
		.deserialized()
}

/// Stamp an event with its receive order, if not already stamped.
/// This is write-once: rescue, reorder, and table moves never change it.
#[implement(Service)]
pub fn stamp_receive_count(&self, event_id: &EventId) {
	if self.db.eventid_receivecount.get_blocking(event_id).is_err() {
		if let Ok(count) = self.services.globals.next_count() {
			self.db
				.eventid_receivecount
				.insert(event_id, count.to_be_bytes());
		}
	}
}

/// Append the PDU as an outlier.
#[implement(Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub fn add_pdu_outlier(
	&self,
	event_id: &EventId,
	pdu: &CanonicalJsonObject,
	room_id: Option<&RoomId>,
) {
	let mut batch = database::rocksdb::WriteBatch::default();
	self.add_pdu_outlier_batch(&mut batch, event_id, pdu, room_id);
	self.db.eventid_pdu.apply_batch(&batch);
	self.db.eventid_pdu.wake(event_id.as_bytes());
}

/// Append the PDU as an outlier using a WriteBatch.
#[implement(Service)]
#[tracing::instrument(skip(self, batch, pdu), level = "debug")]
pub fn add_pdu_outlier_batch(
	&self,
	batch: &mut database::rocksdb::WriteBatch,
	event_id: &EventId,
	pdu: &CanonicalJsonObject,
	room_id: Option<&RoomId>,
) {
	self.stamp_receive_count(event_id);

	let mut pdu = pdu.clone();
	pdu.insert("event_id".to_owned(), CanonicalJsonValue::String(event_id.as_str().to_owned()));

	let room_id_from_pdu = pdu
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|r| <&RoomId>::try_from(r).ok())
		.map(ToOwned::to_owned)
		.or_else(|| room_id.map(ToOwned::to_owned))
		.or_else(|| {
			let is_create =
				pdu.get("type").and_then(CanonicalJsonValue::as_str) == Some("m.room.create");
			is_create
				.then(|| event_id.as_str().replace('$', "!"))
				.and_then(|r| OwnedRoomId::parse(r).ok())
		});

	// --- Phase 1: Write ---
	self.db
		.eventid_pdu
		.raw_put_into_batch(batch, event_id.as_bytes(), Json(&pdu));

	if let Some(room_id) = room_id_from_pdu {
		let room_id: &RoomId = &room_id;
		let mut key = room_id.as_bytes().to_vec();
		key.push(0xFF);
		key.extend_from_slice(event_id.as_bytes());
		self.db
			.roomid_outliereventid
			.insert_into_batch(batch, &key, event_id);
	}

	if let Ok(parsed_pdu) =
		serde_json::from_value::<PduEvent>(serde_json::to_value(&pdu).unwrap())
	{
		let metadata = rooms::timeline::EventMetadata {
			short_room_id: 0,
			is_outlier: true,
			origin_server_ts: parsed_pdu.origin_server_ts().0,
			depth: parsed_pdu.depth(),
			soft_failed: false,
			rejected: parsed_pdu.rejected(),
			redacted_by: parsed_pdu.redacts().map(ToOwned::to_owned),
			short_state_hash: None,
		};
		if let Ok(metadata_bytes) = bincode::serialize(&metadata) {
			self.db.eventid_metadata.insert_into_batch(
				batch,
				event_id.as_bytes(),
				&metadata_bytes,
			);
		}
	}
}

/// Apply a batch of outlier insertions
#[implement(Service)]
pub fn apply_outlier_batch(&self, batch: &database::rocksdb::WriteBatch) {
	self.db.eventid_pdu.apply_batch(batch);
}

/// Remove the PDU from the outlier tree. When the caller knows the
/// room_id (hot path), pass it for O(1) index cleanup. Otherwise the
/// room_id is derived from the stored PDU JSON.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn remove_outlier(&self, event_id: &EventId, provided_room_id: Option<&RoomId>) {
	// Fast path: caller provides room_id → O(1) index delete
	let resolved_room_id: Option<OwnedRoomId> = if let Some(room_id) = provided_room_id {
		Some(room_id.to_owned())
	} else if let Ok(json) = self
		.db
		.eventid_pdu
		.get(event_id.as_bytes())
		.await
		.deserialized::<CanonicalJsonObject>()
	{
		// Derive room_id from PDU JSON using the same fallback chain
		// as add_pdu_outlier.
		json.get("room_id")
			.and_then(CanonicalJsonValue::as_str)
			.and_then(|r| <&RoomId>::try_from(r).ok())
			.map(ToOwned::to_owned)
			.or_else(|| {
				let is_create = json.get("type").and_then(CanonicalJsonValue::as_str)
					== Some("m.room.create");
				is_create
					.then(|| event_id.as_str().replace('$', "!"))
					.and_then(|r| OwnedRoomId::parse(r).ok())
			})
	} else {
		None
	};

	if let Some(room_id) = resolved_room_id {
		let room_id: &RoomId = &room_id;
		let mut key = room_id.as_bytes().to_vec();
		key.push(0xFF);
		key.extend_from_slice(event_id.as_bytes());
		self.db.roomid_outliereventid.remove(&key);
	}
	// If room_id can't be resolved, the ~80 byte roomid_outliereventid
	// entry may remain as a harmless orphan. room_stream filters by
	// room prefix and ignores orphaned entries.

	if !self
		.services
		.timeline
		.non_outlier_pdu_exists(event_id)
		.await
	{
		self.db.eventid_pdu.remove(event_id.as_bytes());
		self.db.eventid_metadata.remove(event_id.as_bytes());
	}
}

#[implement(Service)]
pub async fn fix_pdu_event_ids(&self) -> Result<usize> { Ok(0) }

#[implement(Service)]
#[tracing::instrument(skip(self), level = "info")]
pub async fn startup_janitor(&self) {
	info!("Outlier janitor is disabled.");
}
