use std::{borrow::Borrow, fmt::Debug, mem::size_of_val, sync::Arc};

pub use conduwuit::matrix::pdu::{ShortEventId, ShortId, ShortRoomId, ShortStateKey};
use conduwuit::{
	Result, err, implement,
	matrix::StateKey,
	pair_of,
	utils::{self, IterStream, ReadyExt},
};
use database::{Deserialized, Get, Map, Qry};
use futures::{
	Stream, StreamExt,
	stream::{self},
};
use ruma::{EventId, OwnedEventId, RoomId, events::StateEventType};
use serde::Deserialize;

use crate::{Dep, globals};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	eventid_shorteventid: Arc<Map>,
	shorteventid_eventid: Arc<Map>,
	statekey_shortstatekey: Arc<Map>,
	shortstatekey_statekey: Arc<Map>,
	roomid_shortroomid: Arc<Map>,
	statehash_shortstatehash: Arc<Map>,
}

struct Services {
	globals: Dep<globals::Service>,
}

pub type ShortStateHash = ShortId;

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_shorteventid: args.db["eventid_shorteventid"].clone(),
				shorteventid_eventid: args.db["shorteventid_eventid"].clone(),
				statekey_shortstatekey: args.db["statekey_shortstatekey"].clone(),
				shortstatekey_statekey: args.db["shortstatekey_statekey"].clone(),
				roomid_shortroomid: args.db["roomid_shortroomid"].clone(),
				statehash_shortstatehash: args.db["statehash_shortstatehash"].clone(),
			},
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub async fn get_or_create_shorteventid(&self, event_id: &EventId) -> ShortEventId {
	if let Ok(shorteventid) = self.get_shorteventid(event_id).await {
		return shorteventid;
	}

	self.create_shorteventid(event_id)
}

#[implement(Service)]
pub fn multi_get_or_create_shorteventid<'a, I>(
	&'a self,
	event_ids: I,
) -> impl Stream<Item = ShortEventId> + Send + 'a
where
	I: Iterator<Item = &'a EventId> + Clone + Debug + Send + 'a,
{
	event_ids
		.clone()
		.stream()
		.get(&self.db.eventid_shorteventid)
		.zip(event_ids.into_iter().stream())
		.ready_chunks(256)
		.map(move |chunk| {
			use std::collections::HashMap;

			const BUFSIZE: usize = size_of::<ShortEventId>();
			let mut chunk_map = HashMap::<&EventId, ShortEventId>::with_capacity(chunk.len());
			let mut missing = Vec::with_capacity(chunk.len());

			for (result, event_id) in &chunk {
				match result {
					| Ok(ref short) => {
						chunk_map.insert(event_id, utils::u64_from_u8(short));
					},
					| Err(_) => {
						missing.push(*event_id);
					},
				}
			}

			// Deduplicate missing IDs within the chunk to avoid over-allocating next_id
			missing.retain(|id| !chunk_map.contains_key(id));
			missing.sort_unstable();
			missing.dedup();

			let mut next_id = if !missing.is_empty() {
				self.services
					.globals
					.next_count_batch(u64::try_from(missing.len()).unwrap())
					.unwrap()
			} else {
				0
			};

			let mut seen = HashMap::with_capacity(chunk.len());
			let mut results = Vec::with_capacity(chunk.len());
			for (result, event_id) in chunk {
				if let Some(&short) = seen.get(event_id) {
					results.push(short);
					continue;
				}

				match result {
					| Ok(ref short) => {
						let short = utils::u64_from_u8(short);
						seen.insert(event_id, short);
						results.push(short);
					},
					| Err(_) => {
						let short = next_id.saturating_add(1);
						next_id = short;

						self.db
							.eventid_shorteventid
							.raw_aput::<BUFSIZE, _, _>(event_id, short);
						self.db
							.shorteventid_eventid
							.aput_raw::<BUFSIZE, _, _>(short, event_id);

						seen.insert(event_id, short);
						results.push(short);
					},
				}
			}
			IterStream::stream(results.into_iter())
		})
		.flatten()
}

#[implement(Service)]
pub async fn get_shorteventid(&self, event_id: &EventId) -> Result<ShortEventId> {
	const BUFSIZE: usize = size_of::<ShortEventId>();
	self.db
		.eventid_shorteventid
		.aqry::<BUFSIZE, _>(event_id)
		.await
		.deserialized()
}

#[implement(Service)]
pub fn multi_get_shorteventid<'a, I>(
	&'a self,
	event_ids: I,
) -> impl Stream<Item = Result<ShortEventId>> + Send + 'a
where
	I: Iterator<Item = &'a EventId> + Send + 'a,
{
	event_ids
		.stream()
		.get(&self.db.eventid_shorteventid)
		.map(|res| res.deserialized())
}

#[implement(Service)]
pub async fn get_eventid_from_short(&self, shorteventid: ShortEventId) -> Result<OwnedEventId> {
	const BUFSIZE: usize = size_of::<ShortEventId>();
	self.db
		.shorteventid_eventid
		.aqry::<BUFSIZE, _>(&shorteventid)
		.await
		.deserialized()
}

#[implement(Service)]
pub fn multi_get_eventid_from_short<'a, I>(
	&'a self,
	shorteventids: I,
) -> impl Stream<Item = Result<OwnedEventId>> + Send + 'a
where
	I: Iterator<Item = ShortEventId> + Send + 'a,
{
	const BUFSIZE: usize = size_of::<ShortEventId>();
	shorteventids
		.stream()
		.get_aqry::<BUFSIZE, _, _>(&self.db.shorteventid_eventid)
		.map(|res| res.deserialized())
}

#[implement(Service)]
pub fn create_shorteventid(&self, event_id: &EventId) -> ShortEventId {
	const BUFSIZE: usize = size_of::<ShortEventId>();
	let shorteventid = self.services.globals.next_count().unwrap();
	self.db
		.eventid_shorteventid
		.raw_aput::<BUFSIZE, _, _>(event_id, shorteventid);
	self.db
		.shorteventid_eventid
		.aput_raw::<BUFSIZE, _, _>(shorteventid, event_id);
	shorteventid
}

#[implement(Service)]
pub async fn get_or_create_shortstatekey(
	&self,
	event_type: &StateEventType,
	state_key: &str,
) -> ShortStateKey {
	if let Ok(shortstatekey) = self.get_shortstatekey(event_type, state_key).await {
		return shortstatekey;
	}

	let shortstatekey = self.services.globals.next_count().unwrap();
	self.db
		.statekey_shortstatekey
		.put((event_type, state_key), shortstatekey);
	self.db
		.shortstatekey_statekey
		.put(shortstatekey, (event_type, state_key));
	shortstatekey
}

#[implement(Service)]
pub async fn get_shortstatekey(
	&self,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<ShortStateKey> {
	self.db
		.statekey_shortstatekey
		.qry(&(event_type, state_key))
		.await
		.deserialized()
}

#[implement(Service)]
pub fn multi_get_statekey_from_short<'a, I>(
	&'a self,
	shortstatekeys: I,
) -> impl Stream<Item = Result<(StateEventType, String)>> + Send + 'a
where
	I: Iterator<Item = ShortStateKey> + Send + 'a,
{
	shortstatekeys
		.stream()
		.get(&self.db.shortstatekey_statekey)
		.map(|res| res.deserialized())
}

#[implement(Service)]
pub async fn get_statekey_from_short(
	&self,
	shortstatekey: ShortStateKey,
) -> Result<(StateEventType, String)> {
	self.db
		.shortstatekey_statekey
		.get(&shortstatekey)
		.await
		.deserialized()
}

#[implement(Service)]
pub async fn get_or_create_shortroomid(&self, room_id: &RoomId) -> ShortRoomId {
	if let Ok(shortroomid) = self.get_shortroomid(room_id).await {
		return shortroomid;
	}

	let shortroomid = self.services.globals.next_count().unwrap();
	self.db.roomid_shortroomid.put(room_id, shortroomid);
	shortroomid
}

#[implement(Service)]
pub async fn get_shortroomid(&self, room_id: &RoomId) -> Result<ShortRoomId> {
	self.db.roomid_shortroomid.qry(room_id).await.deserialized()
}

#[implement(Service)]
pub async fn set_shortstatehash(&self, room_id: &RoomId, shortstatehash: ShortStateHash) {
	self.db
		.statehash_shortstatehash
		.put(room_id, shortstatehash);
}

#[implement(Service)]
pub async fn get_shortstatehash(&self, room_id: &RoomId) -> Result<ShortStateHash> {
	self.db
		.statehash_shortstatehash
		.qry(room_id)
		.await
		.deserialized()
}
