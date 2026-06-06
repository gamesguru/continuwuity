use std::{
	borrow::Borrow,
	fmt::Debug,
	mem::{size_of, size_of_val},
	sync::Arc,
};

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
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId, events::StateEventType};
use serde::Deserialize;

use crate::{Dep, globals};

pub struct Service {
	db: Data,
	services: Services,
	statekey_cache: dashmap::DashMap<ShortStateKey, (StateEventType, StateKey)>,
}

struct Data {
	eventid_shorteventid: Arc<Map>,
	shorteventid_eventid: Arc<Map>,
	statekey_shortstatekey: Arc<Map>,
	shortstatekey_statekey: Arc<Map>,
	roomid_shortroomid: Arc<Map>,
	roomid_roomversion: Arc<Map>,
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
				roomid_roomversion: args.db["roomid_roomversion"].clone(),
				statehash_shortstatehash: args.db["statehash_shortstatehash"].clone(),
			},
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
			},
			statekey_cache: dashmap::DashMap::new(),
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
			let missing_count = chunk.iter().filter(|(res, _)| res.is_err()).count() as u64;
			let mut next_id = if missing_count > 0 {
				self.services
					.globals
					.next_count_batch(missing_count)
					.unwrap()
			} else {
				0
			};

			let mut results = Vec::with_capacity(chunk.len());
			for (result, event_id) in chunk {
				match result {
					| Ok(ref short) => results.push(utils::u64_from_u8(short)),
					| Err(_) => {
						next_id += 1;
						let short = next_id;

						const BUFSIZE: usize = size_of::<ShortEventId>();
						self.db
							.eventid_shorteventid
							.raw_aput::<BUFSIZE, _, _>(event_id, short);
						self.db
							.shorteventid_eventid
							.aput_raw::<BUFSIZE, _, _>(short, event_id);

						results.push(short);
					},
				}
			}
			IterStream::stream(results.into_iter())
		})
		.flatten_stream()
}

#[implement(Service)]
fn create_shorteventid(&self, event_id: &EventId) -> ShortEventId {
	const BUFSIZE: usize = size_of::<ShortEventId>();

	let short = self.services.globals.next_count().unwrap();
	debug_assert!(size_of_val(&short) == BUFSIZE, "buffer requirement changed");

	self.db
		.eventid_shorteventid
		.raw_aput::<BUFSIZE, _, _>(event_id, short);

	self.db
		.shorteventid_eventid
		.aput_raw::<BUFSIZE, _, _>(short, event_id);

	short
}

#[implement(Service)]
pub async fn get_shorteventid(&self, event_id: &EventId) -> Result<ShortEventId> {
	self.db
		.eventid_shorteventid
		.get(event_id)
		.await
		.deserialized()
}

#[implement(Service)]
pub async fn get_or_create_shortstatekey(
	&self,
	event_type: &StateEventType,
	state_key: &str,
) -> ShortStateKey {
	const BUFSIZE: usize = size_of::<ShortStateKey>();

	if let Ok(shortstatekey) = self.get_shortstatekey(event_type, state_key).await {
		return shortstatekey;
	}

	let key = (event_type, state_key);
	let shortstatekey = self.services.globals.next_count().unwrap();
	debug_assert!(size_of_val(&shortstatekey) == BUFSIZE, "buffer requirement changed");

	self.db
		.statekey_shortstatekey
		.put_aput::<BUFSIZE, _, _>(key, shortstatekey);

	self.db
		.shortstatekey_statekey
		.aput_put::<BUFSIZE, _, _>(shortstatekey, key);

	let cached_key = (event_type.clone(), StateKey::from(state_key));
	self.statekey_cache.insert(shortstatekey, cached_key);

	shortstatekey
}

#[implement(Service)]
pub async fn get_shortstatekey(
	&self,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<ShortStateKey> {
	let key = (event_type, state_key);
	self.db
		.statekey_shortstatekey
		.qry(&key)
		.await
		.deserialized()
}

#[implement(Service)]
pub async fn get_eventid_from_short<Id>(&self, shorteventid: ShortEventId) -> Result<Id>
where
	Id: for<'de> Deserialize<'de> + Sized + ToOwned,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	const BUFSIZE: usize = size_of::<ShortEventId>();

	self.db
		.shorteventid_eventid
		.aqry::<BUFSIZE, _>(&shorteventid)
		.await
		.deserialized()
		.map_err(|e| err!(Database("Failed to find EventId from short {shorteventid:?}: {e:?}")))
}

#[implement(Service)]
pub fn multi_get_eventid_from_short<'a, Id, S>(
	&'a self,
	shorteventid: S,
) -> impl Stream<Item = Result<Id>> + Send + 'a
where
	S: Stream<Item = ShortEventId> + Send + 'a,
	Id: for<'de> Deserialize<'de> + Sized + ToOwned + 'a,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	shorteventid
		.qry(&self.db.shorteventid_eventid)
		.map(Deserialized::deserialized)
}

#[implement(Service)]
pub async fn get_statekey_from_short(
	&self,
	shortstatekey: ShortStateKey,
) -> Result<(StateEventType, StateKey)> {
	const BUFSIZE: usize = size_of::<ShortStateKey>();

	if let Some(cached) = self.statekey_cache.get(&shortstatekey) {
		return Ok(cached.clone());
	}

	let res: (StateEventType, StateKey) = self
		.db
		.shortstatekey_statekey
		.aqry::<BUFSIZE, _>(&shortstatekey)
		.await
		.deserialized()
		.map_err(|e| {
			err!(Database(
				"Failed to find (StateEventType, state_key) from short {shortstatekey:?}: {e:?}"
			))
		})?;

	self.statekey_cache.insert(shortstatekey, res.clone());
	Ok(res)
}

#[implement(Service)]
pub fn multi_get_statekey_from_short<'a, S>(
	&'a self,
	shortstatekey: S,
) -> impl Stream<Item = Result<(StateEventType, StateKey)>> + Send + 'a
where
	S: Stream<Item = ShortStateKey> + Send + 'a,
{
	shortstatekey
		.ready_chunks(256)
		.then(move |chunk| async move {
			let mut results = Vec::with_capacity(chunk.len());
			let mut misses = Vec::new();
			let mut miss_indices = Vec::new();

			for (i, key) in chunk.iter().copied().enumerate() {
				if let Some(cached) = self.statekey_cache.get(&key) {
					results.push(Some(Ok(cached.clone())));
				} else {
					results.push(None);
					misses.push(key);
					miss_indices.push(i);
				}
			}

			if !misses.is_empty() {
				let db_results: Vec<Result<(StateEventType, StateKey)>> =
					stream::iter(misses.clone())
						.qry(&self.db.shortstatekey_statekey)
						.map(|res| {
							res.and_then(|handle| {
								serde_json::from_slice(&handle).map_err(|e| {
									err!(Database("Failed to deserialize statekey: {e:?}"))
								})
							})
						})
						.collect()
						.await;

				for (idx, res) in miss_indices.into_iter().zip(db_results.into_iter()) {
					if let Ok(ref val) = res {
						self.statekey_cache.insert(misses[idx], val.clone());
					}
					results[idx] = Some(res);
				}
			}

			stream::iter(results.into_iter().map(Option::unwrap))
		})
		.flatten()
}

/// Returns (shortstatehash, already_existed)
#[implement(Service)]
pub async fn get_or_create_shortstatehash(&self, state_hash: &[u8]) -> (ShortStateHash, bool) {
	const BUFSIZE: usize = size_of::<ShortStateHash>();

	if let Ok(shortstatehash) = self
		.db
		.statehash_shortstatehash
		.get(state_hash)
		.await
		.deserialized()
	{
		return (shortstatehash, true);
	}

	let shortstatehash = self.services.globals.next_count().unwrap();
	debug_assert!(size_of_val(&shortstatehash) == BUFSIZE, "buffer requirement changed");

	self.db
		.statehash_shortstatehash
		.raw_aput::<BUFSIZE, _, _>(state_hash, shortstatehash);

	(shortstatehash, false)
}

#[implement(Service)]
pub async fn get_shortroomid(&self, room_id: &RoomId) -> Result<ShortRoomId> {
	self.db.roomid_shortroomid.get(room_id).await.deserialized()
}

#[implement(Service)]
pub async fn get_or_create_shortroomid(&self, room_id: &RoomId) -> ShortRoomId {
	self.db
		.roomid_shortroomid
		.get(room_id)
		.await
		.deserialized()
		.unwrap_or_else(|_| {
			const BUFSIZE: usize = size_of::<ShortRoomId>();

			let short = self.services.globals.next_count().unwrap();
			debug_assert!(size_of_val(&short) == BUFSIZE, "buffer requirement changed");

			self.db
				.roomid_shortroomid
				.raw_aput::<BUFSIZE, _, _>(room_id, short);

			short
		})
}

#[implement(Service)]
pub async fn multi_get_state_from_short<'a, S>(
	&'a self,
	short_state: S,
) -> impl Stream<Item = Result<((StateEventType, StateKey), OwnedEventId)>> + Send + 'a
where
	S: Stream<Item = (ShortStateKey, ShortEventId)> + Send + 'a,
{
	let (short_state_keys, short_event_ids): pair_of!(Vec<_>) = short_state.unzip().await;

	StreamExt::zip(
		self.multi_get_statekey_from_short(stream::iter(short_state_keys.into_iter())),
		self.multi_get_eventid_from_short(stream::iter(short_event_ids.into_iter())),
	)
	.ready_filter_map(|state_event| match state_event {
		| (Ok(state_key), Ok(event_id)) => Some(Ok((state_key, event_id))),
		| (Err(e), _) | (_, Err(e)) => Some(Err(e)),
	})
}

#[implement(Service)]
pub async fn get_room_version(&self, room_id: &RoomId) -> Result<RoomVersionId> {
	self.db.roomid_roomversion.get(room_id).await.deserialized()
}

#[implement(Service)]
pub fn set_room_version(&self, room_id: &RoomId, version: &RoomVersionId) {
	self.db.roomid_roomversion.insert(room_id, version);
}
