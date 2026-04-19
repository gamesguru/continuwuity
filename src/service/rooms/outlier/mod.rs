use std::sync::Arc;

use conduwuit::{
	Result, implement,
	matrix::{Event, PduEvent},
	utils::stream::{BroadbandExt, ReadyExt, TryIgnore},
};
use database::{Deserialized, Json, Map};
use futures::Stream;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId};

pub struct Service {
	db: Data,
}

struct Data {
	eventid_outlierpdu: Arc<Map>,
	roomid_outliereventid: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_outlierpdu: args.db["eventid_outlierpdu"].clone(),
				roomid_outliereventid: args.db["roomid_outliereventid"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_pdu_outlier(&self, event_id: &EventId) -> Result<PduEvent> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

#[implement(Service)]
pub fn stream(&self) -> impl Stream<Item = (OwnedEventId, PduEvent)> + Send + '_ {
	self.db
		.eventid_outlierpdu
		.stream::<OwnedEventId, PduEvent>()
		.ignore_err()
}

#[implement(Service)]
pub fn room_stream<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = (OwnedEventId, PduEvent)> + Send + 'a {
	self.db
		.roomid_outliereventid
		.stream_from::<Vec<u8>, OwnedEventId, _>(room_id)
		.ignore_err()
		.ready_take_while(move |(k, _): &(_, _)| k.starts_with(room_id.as_bytes()))
		.ready_filter_map(|(_, v): (_, OwnedEventId)| Some(v))
		.broad_filter_map(move |event_id: OwnedEventId| async move {
			let pdu = self.get_pdu_outlier(&event_id).await.ok()?;
			Some((event_id, pdu))
		})
}

/// Append the PDU as an outlier.
#[implement(Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) {
	self.db.eventid_outlierpdu.raw_put(event_id, Json(pdu));

	if let Some(room_id) = pdu
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|r| <&RoomId>::try_from(r).ok())
	{
		let mut key = room_id.as_bytes().to_vec();
		key.push(0xFF);
		key.extend_from_slice(event_id.as_bytes());
		self.db.roomid_outliereventid.insert(&key, event_id);
	}
}

/// Remove the PDU from the outlier tree.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn remove_outlier(&self, event_id: &EventId) {
	if let Ok(pdu) = self
		.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized::<PduEvent>()
	{
		if let Some(room_id) = pdu.room_id.as_deref() {
			let mut key = room_id.as_bytes().to_vec();
			key.push(0xFF);
			key.extend_from_slice(event_id.as_bytes());
			self.db.roomid_outliereventid.remove(key);
		}
	}
	self.db.eventid_outlierpdu.remove(event_id);
}
