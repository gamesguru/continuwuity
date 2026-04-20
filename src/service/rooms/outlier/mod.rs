use std::sync::Arc;

use conduwuit::{
	Result, implement, info,
	matrix::PduEvent,
	utils::stream::{BroadbandExt, ReadyExt, TryIgnore},
};
use database::{Deserialized, Json, Map};
use futures::Stream;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId, RoomId};

use crate::{Dep, rooms};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	eventid_outlierpdu: Arc<Map>,
	roomid_outliereventid: Arc<Map>,
}

struct Services {
	timeline: Dep<rooms::timeline::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_outlierpdu: args.db["eventid_outlierpdu"].clone(),
				roomid_outliereventid: args.db["roomid_outliereventid"].clone(),
			},
			services: Services {
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
			self.db.roomid_outliereventid.remove(&key);
		}
	}
	self.db.eventid_outlierpdu.remove(event_id);
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "info")]
pub async fn startup_janitor(&self) {
	use futures::StreamExt;

	let mut count = 0_usize;
	let mut room_index_count = 0_usize;

	info!("Starting outlier janitor...");

	// Clean up stale entries in roomid_outliereventid index
	// and migrate old-format keys to new format (with 0xFF separator)
	let mut room_index = self.db.roomid_outliereventid.raw_stream();
	while let Some(Ok((key, value))) = room_index.next().await {
		let event_id_str = match std::str::from_utf8(value) {
			| Ok(s) => s,
			| Err(_) => continue,
		};
		let event_id = match OwnedEventId::try_from(event_id_str) {
			| Ok(id) => id,
			| Err(_) => continue,
		};

		if self
			.services
			.timeline
			.non_outlier_pdu_exists(&event_id)
			.await
		{
			self.db.roomid_outliereventid.remove(key);
			room_index_count = room_index_count.saturating_add(1);
			continue;
		}

		// Migration: if key doesn't contain 0xFF, it's the old format
		if !key.contains(&0xFF) {
			if let Ok(pdu) = self.get_pdu_outlier(&event_id).await {
				let room_id = pdu.room_id.clone().or_else(|| {
					(pdu.kind == ruma::events::TimelineEventType::RoomCreate)
						.then(|| pdu.event_id.as_str().replace('$', "!"))
						.and_then(|r| OwnedRoomId::parse(r).ok())
				});

				if let Some(room_id) = room_id {
					let mut new_key = room_id.as_bytes().to_vec();
					new_key.push(0xFF);
					new_key.extend_from_slice(event_id.as_bytes());

					if new_key != key {
						self.db.roomid_outliereventid.raw_put(&new_key, value);
						self.db.roomid_outliereventid.remove(key);
						conduwuit::debug!("Migrated outlier index key for {event_id}");
					}
				}
			}
		}
	}

	// Clean up stale entries in eventid_outlierpdu
	let mut outliers = self.db.eventid_outlierpdu.raw_stream();
	while let Some(Ok((event_id_bytes, _))) = outliers.next().await {
		let event_id_str = match std::str::from_utf8(event_id_bytes) {
			| Ok(s) => s,
			| Err(_) => continue,
		};
		let event_id = match OwnedEventId::try_from(event_id_str) {
			| Ok(id) => id,
			| Err(_) => continue,
		};
		if self
			.services
			.timeline
			.non_outlier_pdu_exists(&event_id)
			.await
		{
			self.db.eventid_outlierpdu.remove(event_id_bytes);
			count = count.saturating_add(1);
		}
	}

	if count > 0 || room_index_count > 0 {
		info!(
			"Outlier janitor finished. Cleaned up {count} stale outliers and {room_index_count} 			 stale room index entries."
		);
	} else {
		info!("Outlier janitor finished. No stale outliers found.");
	}
}
