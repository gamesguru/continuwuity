use std::sync::Arc;

use conduwuit::{
	Result, implement, info,
	matrix::{Event, PduEvent},
	utils::{
		ReadyExt,
		stream::{BroadbandExt, TryIgnore},
	},
};
use database::{Deserialized, Json, Map};
use futures::{FutureExt, Stream, StreamExt};
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId, RoomId};

use crate::{Dep, rooms, rooms::short::ShortRoomId};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	eventid_pdu: Arc<Map>,
	eventid_metadata: Arc<Map>,
	shorteventid_shortprevevents: Arc<Map>,
	shorteventid_shortauthevents: Arc<Map>,
}

struct Services {
	short: Dep<rooms::short::Service>,
	#[allow(dead_code)]
	timeline: Dep<rooms::timeline::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_pdu: args.db["eventid_pdu"].clone(),
				eventid_metadata: args.db["eventid_metadata"].clone(),
				shorteventid_shortprevevents: args.db["shorteventid_shortprevevents"].clone(),
				shorteventid_shortauthevents: args.db["shorteventid_shortauthevents"].clone(),
			},
			services: Services {
				short: args.depend::<rooms::short::Service>("rooms::short"),
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
		.get_nocache(event_id.as_bytes())
		.await
		.deserialized()
}

#[implement(Service)]
pub fn stream_keys(&self) -> impl Stream<Item = OwnedEventId> + Send + '_ {
	self.db
		.eventid_metadata
		.raw_stream()
		.ignore_err()
		.ready_filter_map(|(key, val)| {
			let eid = OwnedEventId::try_from(std::str::from_utf8(key).ok()?).ok()?;
			let meta: rooms::timeline::EventMetadata = bincode::deserialize(val).ok()?;
			meta.is_outlier.then_some(eid)
		})
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
	let short_room_id = self
		.services
		.short
		.get_shortroomid(room_id)
		.map(std::result::Result::ok);

	futures::stream::once(short_room_id)
		.filter_map(|opt| async move { opt })
		.flat_map(move |target_short: ShortRoomId| {
			self.db
				.eventid_metadata
				.raw_stream()
				.ignore_err()
				.ready_filter_map(move |(key, val)| {
					let eid = OwnedEventId::try_from(std::str::from_utf8(key).ok()?).ok()?;
					let meta: rooms::timeline::EventMetadata = bincode::deserialize(val).ok()?;
					(meta.is_outlier && meta.short_room_id == target_short).then_some(eid)
				})
		})
		.broad_filter_map(move |eid: OwnedEventId| async move {
			let pdu = self.get_pdu_outlier(&eid).await.ok()?;
			Some((eid, pdu))
		})
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
	// Guard: never overwrite an event that already has a timeline entry.
	// The eventid_pdu and eventid_metadata tables are shared between timeline
	// and outlier paths. Re-adding a timeline event as an outlier would set
	// is_outlier=true and zero out local_topological_depth, making the event
	// invisible to /sync's timeline iterator (the "stuck state" bug).
	if let Ok(existing_meta) = self.db.eventid_metadata.get_blocking(event_id.as_bytes()) {
		if let Ok(meta) = bincode::deserialize::<rooms::timeline::EventMetadata>(&existing_meta) {
			if !meta.is_outlier {
				info!(
					%event_id,
					"add_pdu_outlier: skipping, event already in timeline"
				);
				return;
			}
		}
	}

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

	if let Ok(parsed_pdu) =
		serde_json::from_value::<PduEvent>(serde_json::to_value(&pdu).unwrap())
	{
		let short_room_id = room_id_from_pdu
			.as_deref()
			.and_then(|rid| self.services.short.get_shortroomid(rid).now_or_never())
			.and_then(Result::ok)
			.unwrap_or(0);

		let metadata = rooms::timeline::EventMetadata {
			short_room_id,
			is_outlier: true,
			origin_server_ts: parsed_pdu.origin_server_ts().0,
			depth: parsed_pdu.depth(),
			soft_failed: false,
			rejected: parsed_pdu.rejected(),
			redacted_by: parsed_pdu.redacts().map(ToOwned::to_owned),
			short_state_hash: None,
			local_topological_depth: 0,
			pdu_count: None,
			soft_fail_reason: String::new(),
		};
		if let Ok(metadata_bytes) = bincode::serialize(&metadata) {
			self.db.eventid_metadata.insert_into_batch(
				batch,
				event_id.as_bytes(),
				&metadata_bytes,
			);
		}

		let short_event_id = self
			.services
			.short
			.get_or_create_shorteventid_blocking(event_id);
		let prev_shorts: Vec<rooms::short::ShortEventId> = parsed_pdu
			.prev_events()
			.map(|prev| {
				self.services
					.short
					.get_or_create_shorteventid_blocking(prev)
			})
			.collect();

		let key_bytes = short_event_id.to_be_bytes();
		let val_bytes = prev_shorts
			.iter()
			.flat_map(|s| s.to_be_bytes())
			.collect::<Vec<u8>>();

		self.db
			.shorteventid_shortprevevents
			.insert_into_batch(batch, &key_bytes, &val_bytes);

		let auth_shorts: Vec<rooms::short::ShortEventId> = parsed_pdu
			.auth_events()
			.map(|auth| {
				self.services
					.short
					.get_or_create_shorteventid_blocking(auth)
			})
			.collect();

		let auth_val_bytes = auth_shorts
			.iter()
			.flat_map(|s| s.to_be_bytes())
			.collect::<Vec<u8>>();

		self.db.shorteventid_shortauthevents.insert_into_batch(
			batch,
			&key_bytes,
			&auth_val_bytes,
		);
	}
}

/// Apply a batch of outlier insertions
#[implement(Service)]
pub fn apply_outlier_batch(&self, batch: &database::rocksdb::WriteBatch) {
	self.db.eventid_pdu.apply_batch(batch);
}

/// Remove the PDU from the outlier tree.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn remove_outlier(&self, event_id: &EventId) {
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
pub fn fix_pdu_event_ids(&self) -> Result<usize> { Ok(0) }

#[implement(Service)]
#[tracing::instrument(skip(self), level = "info")]
pub async fn startup_janitor(&self) {
	info!("Outlier janitor is disabled.");
}
