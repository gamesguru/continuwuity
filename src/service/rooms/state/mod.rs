use std::{collections::HashMap, fmt::Write, iter::once, sync::Arc};

use async_trait::async_trait;
use conduwuit::{RoomVersion, debug, info};
use conduwuit_core::{
	Event, PduEvent, Result, err,
	result::FlatOk,
	state_res::{self, StateMap},
	utils::{
		IterStream, MutexMap, MutexMapGuard, ReadyExt, calculate_hash,
		stream::{BroadbandExt, TryIgnore},
	},
	warn,
};
use conduwuit_database::{Deserialized, Ignore, Interfix, Map};
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::join_all, pin_mut,
};
use ruma::{
	EventId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId, UserId,
	events::{
		AnyStrippedStateEvent, StateEventType, TimelineEventType,
		room::create::RoomCreateEventContent,
	},
	serde::Raw,
};

use crate::{
	Dep, globals, rooms,
	rooms::{
		short::{ShortEventId, ShortStateHash},
		state_compressor::{CompressedState, parse_compressed_state_event},
	},
};

pub struct Service {
	pub mutex: RoomMutexMap,
	services: Services,
	db: Data,
}

struct Services {
	globals: Dep<globals::Service>,
	short: Dep<rooms::short::Service>,
	spaces: Dep<rooms::spaces::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	state_compressor: Dep<rooms::state_compressor::Service>,
	timeline: Dep<rooms::timeline::Service>,
	outlier: Dep<rooms::outlier::Service>,
}

struct Data {
	shorteventid_shortstatehash: Arc<Map>,
	roomid_shortstatehash: Arc<Map>,
	roomid_pduleaves: Arc<Map>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
pub type RoomMutexGuard = MutexMapGuard<OwnedRoomId, ()>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			mutex: RoomMutexMap::new(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				spaces: args.depend::<rooms::spaces::Service>("rooms::spaces"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
				outlier: args.depend::<rooms::outlier::Service>("rooms::outlier"),
			},
			db: Data {
				shorteventid_shortstatehash: args.db["shorteventid_shortstatehash"].clone(),
				roomid_shortstatehash: args.db["roomid_shortstatehash"].clone(),
				roomid_pduleaves: args.db["roomid_pduleaves"].clone(),
			},
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex = self.mutex.len();
		writeln!(out, "state_mutex: {mutex}")?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Set the room to the given statehash and update caches.
	pub async fn force_state(
		&self,
		room_id: &RoomId,
		shortstatehash: u64,
		statediffnew: Arc<CompressedState>,
		statediffremoved: Arc<CompressedState>,
		state_lock: &RoomMutexGuard,
	) -> Result {
		info!(
			target: "force_state",
			"processing {} new, {} removed state events for {room_id}",
			statediffnew.len(),
			statediffremoved.len()
		);

		let new_event_ids = statediffnew
			.iter()
			.stream()
			.map(|&new| parse_compressed_state_event(new).1)
			.then(|shorteventid| {
				self.services
					.short
					.get_eventid_from_short::<Box<_>>(shorteventid)
			})
			.ignore_err();

		let removed_event_ids = statediffremoved
			.iter()
			.stream()
			.map(|&old| parse_compressed_state_event(old).1)
			.then(|shorteventid| {
				self.services
					.short
					.get_eventid_from_short::<Box<_>>(shorteventid)
			})
			.ignore_err();

		let mut new_processed = 0_usize;
		let mut new_members = 0_usize;
		let mut new_skipped = 0_usize;
		pin_mut!(new_event_ids);
		while let Some(event_id) = new_event_ids.next().await {
			new_processed = new_processed.saturating_add(1);
			let Ok(pdu) = self
				.services
				.timeline
				.get_pdu_in_room(Some(room_id), &event_id)
				.await
			else {
				new_skipped = new_skipped.saturating_add(1);
				continue;
			};

			match pdu.kind {
				| TimelineEventType::RoomMember => {
					let Some(user_id) = pdu.state_key.as_ref().map(UserId::parse).flat_ok()
					else {
						continue;
					};

					self.services
						.state_cache
						.update_membership(room_id, user_id, &pdu, false)
						.await?;
					new_members = new_members.saturating_add(1);
					if new_members.is_multiple_of(1000) {
						info!(
							target: "force_state",
							"processed {new_members} members, {new_processed} total, {new_skipped} skipped"
						);
					}
				},
				| TimelineEventType::SpaceChild => {
					self.services
						.spaces
						.roomid_spacehierarchy_cache
						.lock()
						.await
						.remove(room_id);
				},
				| _ => continue,
			}
		}
		info!(
			target: "force_state",
			"new events done: {new_processed} processed, {new_members} members, {new_skipped} skipped"
		);

		pin_mut!(removed_event_ids);
		while let Some(event_id) = removed_event_ids.next().await {
			// When state is removed, we demote it to an outlier instead of deleting it.
			// This keeps the PDU locally but removes it from the official room state.
			let pdu_json = self.services.timeline.get_pdu_json(&event_id).await;
			if let Ok(pdu_json) = &pdu_json {
				self.services
					.outlier
					.add_pdu_outlier(&event_id, pdu_json, Some(room_id));
				// NOTE: do NOT remove from timeline here. Removed state events
				// should remain as historical timeline entries. Only the state
				// pointer (shortstatehash) changes.
			}

			let Ok(pdu) = self
				.services
				.timeline
				.get_pdu_in_room(Some(room_id), &event_id)
				.await
				.or_else(|_| {
					pdu_json.and_then(|j| {
						PduEvent::from_id_val(&event_id, j, Some(room_id))
							.map_err(|e| err!(Database("Invalid PDU: {e:?}")))
					})
				})
			else {
				continue;
			};

			match pdu.kind {
				| TimelineEventType::RoomMember => {
					let Some(user_id) = pdu.state_key.as_ref().map(UserId::parse).flat_ok()
					else {
						continue;
					};

					// Re-sync membership from the NEW state to update the cache correctly.
					if let Ok(new_pdu) = self
						.services
						.state_accessor
						.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
						.await
					{
						self.services
							.state_cache
							.update_membership(room_id, user_id, &new_pdu, false)
							.await?;
					} else {
						// User is no longer in the room at all in the new state
						self.services
							.state_cache
							.mark_as_left(user_id, room_id, None)
							.await;
					}
				},
				| TimelineEventType::SpaceChild => {
					self.services
						.spaces
						.roomid_spacehierarchy_cache
						.lock()
						.await
						.remove(room_id);
				},
				| _ => continue,
			}
		}
		info!(target: "force_state", "removed events done, updating joined count");
		self.services.state_cache.update_joined_count(room_id).await;

		self.set_room_state(room_id, shortstatehash, state_lock);

		info!(target: "force_state", "complete for {room_id}");
		Ok(())
	}

	/// Reset forward extremities to all events in the given state snapshot.
	///
	/// This is an intentionally destructive operation for admin-level DAG
	/// repair. It breaks the room's DAG continuity by replacing extremities
	/// with the full state set, forcing the room to "restart" from the given
	/// state. Only call this from admin commands, never from normal federation
	/// intake.
	pub async fn reset_extremities_to_state(
		&self,
		room_id: &RoomId,
		shortstatehash: u64,
		state_lock: &RoomMutexGuard,
	) {
		let new_extremities: Vec<OwnedEventId> = self
			.services
			.state_accessor
			.state_full_ids(shortstatehash)
			.map(|(_, id)| id)
			.collect()
			.await;

		info!(
			target: "force_state",
			"Admin: resetting {room_id} extremities to {} state events",
			new_extremities.len()
		);

		self.set_forward_extremities(
			room_id,
			new_extremities.iter().map(AsRef::as_ref),
			state_lock,
		)
		.await;
	}

	/// Generates a new StateHash and associates it with the incoming event.
	///
	/// This adds all current state events (not including the incoming event)
	/// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
	#[tracing::instrument(skip(self, state_ids_compressed), level = "debug")]
	pub async fn set_event_state(
		&self,
		event_id: &EventId,
		room_id: &RoomId,
		state_ids_compressed: Arc<CompressedState>,
	) -> Result<ShortStateHash> {
		const KEY_LEN: usize = size_of::<ShortEventId>();
		const VAL_LEN: usize = size_of::<ShortStateHash>();

		let shorteventid = self
			.services
			.short
			.get_or_create_shorteventid(event_id)
			.await;

		let previous_shortstatehash = self.get_room_shortstatehash(room_id).await;

		let state_hash = calculate_hash(state_ids_compressed.iter().map(|s| &s[..]));

		let (shortstatehash, already_existed) = self
			.services
			.short
			.get_or_create_shortstatehash(&state_hash)
			.await;

		if !already_existed {
			let states_parents = match previous_shortstatehash {
				| Ok(p) =>
					self.services
						.state_compressor
						.load_shortstatehash_info(p)
						.await?,
				| _ => Vec::new(),
			};

			let (statediffnew, statediffremoved) =
				if let Some(parent_stateinfo) = states_parents.last() {
					let statediffnew: CompressedState = state_ids_compressed
						.difference(
							parent_stateinfo
								.full_state
								.as_ref()
								.expect("top frame must have full_state"),
						)
						.copied()
						.collect();

					let statediffremoved: CompressedState = parent_stateinfo
						.full_state
						.as_ref()
						.expect("top frame must have full_state")
						.difference(&state_ids_compressed)
						.copied()
						.collect();

					(Arc::new(statediffnew), Arc::new(statediffremoved))
				} else {
					(state_ids_compressed, Arc::new(CompressedState::new()))
				};
			self.services.state_compressor.save_state_from_diff(
				shortstatehash,
				statediffnew,
				statediffremoved,
				1_000_000, // high number because no state will be based on this one
				states_parents,
			)?;
		}

		self.db
			.shorteventid_shortstatehash
			.aput::<KEY_LEN, VAL_LEN, _, _>(shorteventid, shortstatehash);

		Ok(shortstatehash)
	}

	/// Overwrites the shortstatehash for a specific event. Used by admin
	/// commands to fix stale pdu_shortstatehash entries after force-setting
	/// room state.
	pub fn set_pdu_shortstatehash(&self, shorteventid: u64, shortstatehash: u64) {
		const BUFSIZE: usize = size_of::<u64>();

		self.db
			.shorteventid_shortstatehash
			.aput::<BUFSIZE, BUFSIZE, _, _>(shorteventid, shortstatehash);
	}

	/// Generates a new StateHash and associates it with the incoming event.
	///
	/// This adds all current state events (not including the incoming event)
	/// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
	#[tracing::instrument(skip(self, new_pdu), level = "debug")]
	pub async fn append_to_state(&self, new_pdu: &PduEvent, room_id: &RoomId) -> Result<u64> {
		const BUFSIZE: usize = size_of::<u64>();

		let shorteventid = self
			.services
			.short
			.get_or_create_shorteventid(&new_pdu.event_id)
			.await;

		let previous_shortstatehash = self.get_room_shortstatehash(room_id).await;

		if let Ok(p) = previous_shortstatehash {
			self.db
				.shorteventid_shortstatehash
				.aput::<BUFSIZE, BUFSIZE, _, _>(shorteventid, p);
		}

		match &new_pdu.state_key {
			| Some(state_key) => {
				let states_parents = match previous_shortstatehash {
					| Ok(p) =>
						self.services
							.state_compressor
							.load_shortstatehash_info(p)
							.await?,
					| _ => Vec::new(),
				};

				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(&new_pdu.kind.to_string().into(), state_key)
					.await;

				let new = self
					.services
					.state_compressor
					.compress_state_event(shortstatekey, &new_pdu.event_id)
					.await;

				let replaces = states_parents
					.last()
					.map(|info| {
						info.full_state
							.as_ref()
							.expect("top frame must have full_state")
							.iter()
							.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
					})
					.unwrap_or_default();

				if Some(&new) == replaces {
					return Ok(previous_shortstatehash.expect("must exist"));
				}

				// TODO: statehash with deterministic inputs
				let shortstatehash = self.services.globals.next_count()?;

				let mut statediffnew = CompressedState::new();
				statediffnew.insert(new);

				let mut statediffremoved = CompressedState::new();
				if let Some(replaces) = replaces {
					statediffremoved.insert(*replaces);
				}

				self.services.state_compressor.save_state_from_diff(
					shortstatehash,
					Arc::new(statediffnew),
					Arc::new(statediffremoved),
					2,
					states_parents,
				)?;

				Ok(shortstatehash)
			},
			| _ =>
				Ok(previous_shortstatehash.expect("first event in room must be a state event")),
		}
	}

	#[tracing::instrument(skip_all, level = "debug")]
	pub async fn summary_stripped<'a, E>(
		&self,
		event: &'a E,
		room_id: &RoomId,
	) -> Vec<Raw<AnyStrippedStateEvent>>
	where
		E: Event + Send + Sync,
		&'a E: Event + Send,
	{
		let cells = [
			(&StateEventType::RoomCreate, ""),
			(&StateEventType::RoomJoinRules, ""),
			(&StateEventType::RoomCanonicalAlias, ""),
			(&StateEventType::RoomName, ""),
			(&StateEventType::RoomAvatar, ""),
			(&StateEventType::RoomMember, event.sender().as_str()), // Add recommended events
			(&StateEventType::RoomEncryption, ""),
			(&StateEventType::RoomTopic, ""),
		];

		let fetches = cells.into_iter().map(|(event_type, state_key)| {
			self.services
				.state_accessor
				.room_state_get(room_id, event_type, state_key)
		});

		join_all(fetches)
			.await
			.into_iter()
			.filter_map(Result::ok)
			.map(Event::into_format)
			.chain(once(event.to_format()))
			.collect()
	}

	/// Set the state hash to a new version, but does not update state_cache.
	#[tracing::instrument(skip(self, _mutex_lock), level = "debug")]
	pub fn set_room_state(
		&self,
		room_id: &RoomId,
		shortstatehash: u64,
		// Take mutex guard to make sure users get the room state mutex
		_mutex_lock: &RoomMutexGuard,
	) {
		const BUFSIZE: usize = size_of::<u64>();

		self.db
			.roomid_shortstatehash
			.raw_aput::<BUFSIZE, _, _>(room_id, shortstatehash);
	}

	/// Returns the room's version.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn get_room_version(&self, room_id: &RoomId) -> Result<RoomVersionId> {
		self.services
			.state_accessor
			.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
			.await
			.map(|content: RoomCreateEventContent| content.room_version)
			.map_err(|e| err!(Request(NotFound("No create event found: {e:?}"))))
	}

	pub async fn get_room_shortstatehash(&self, room_id: &RoomId) -> Result<ShortStateHash> {
		self.db
			.roomid_shortstatehash
			.get(room_id)
			.await
			.deserialized()
	}

	pub fn get_forward_extremities<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = &'a EventId> + Send + 'a {
		let prefix = (room_id, Interfix);

		self.db
			.roomid_pduleaves
			.keys_prefix(&prefix)
			.map_ok(|(_, event_id): (Ignore, &EventId)| event_id)
			.ignore_err()
	}

	/// Returns true if the given event_id is a current forward extremity
	/// (DAG tip) for the room.
	pub async fn is_forward_extremity(&self, room_id: &RoomId, event_id: &EventId) -> bool {
		self.get_forward_extremities(room_id)
			.any(|eid| futures::future::ready(eid == event_id))
			.await
	}

	pub async fn set_forward_extremities<'a, I>(
		&'a self,
		room_id: &'a RoomId,
		event_ids: I,
		_state_lock: &'a RoomMutexGuard,
	) where
		I: Iterator<Item = &'a EventId> + Send + 'a,
	{
		let prefix = (room_id, Interfix);
		self.db
			.roomid_pduleaves
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| self.db.roomid_pduleaves.remove(key))
			.await;

		for event_id in event_ids {
			let key = (room_id, event_id);
			self.db.roomid_pduleaves.put_raw(key, event_id);
		}
	}

	/// This fetches auth events from the current state.
	#[tracing::instrument(skip(self, content, room_version), level = "trace")]
	pub async fn get_auth_events(
		&self,
		room_id: &RoomId,
		kind: &TimelineEventType,
		sender: &UserId,
		state_key: Option<&str>,
		content: &serde_json::value::RawValue,
		room_version: &RoomVersion,
	) -> Result<StateMap<PduEvent>> {
		let Ok(shortstatehash) = self.get_room_shortstatehash(room_id).await else {
			return Ok(HashMap::new());
		};

		let auth_types =
			state_res::auth_types_for_event(kind, sender, state_key, content, room_version)?;
		debug!(?auth_types, "Auth types for event");
		let sauthevents: HashMap<_, _> = auth_types
			.iter()
			.stream()
			.broad_filter_map(|(event_type, state_key)| {
				self.services
					.short
					.get_shortstatekey(event_type, state_key)
					.map_ok(move |ssk| (ssk, (event_type, state_key)))
					.map(Result::ok)
			})
			.collect()
			.await;
		debug!(?sauthevents, "Auth events to fetch");

		let (state_keys, event_ids): (Vec<_>, Vec<_>) = self
			.services
			.state_accessor
			.state_full_shortids(shortstatehash)
			.ready_filter_map(Result::ok)
			.ready_filter_map(|(shortstatekey, shorteventid)| {
				sauthevents
					.get(&shortstatekey)
					.map(|(ty, sk)| ((ty, sk), shorteventid))
			})
			.unzip()
			.await;
		debug!(?state_keys, ?event_ids, "Auth events found in state");
		self.services
			.short
			.multi_get_eventid_from_short(event_ids.into_iter().stream())
			.zip(state_keys.into_iter().stream())
			.ready_filter_map(|(event_id, (ty, sk))| Some(((ty, sk), event_id.ok()?)))
			.broad_filter_map(|((ty, sk), event_id): (_, OwnedEventId)| async move {
				self.services
					.timeline
					.get_pdu(&event_id)
					.await
					.map(move |pdu| (((*ty).clone(), (*sk).clone()), pdu))
					.inspect_err(|e| warn!("Failed to get auth event {event_id}: {e:?}"))
					.ok()
			})
			.collect()
			.map(Ok)
			.await
	}
}
