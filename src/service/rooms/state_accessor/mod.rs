mod room_state;
mod server_can;
mod state;
mod user_can;

use std::{fmt::Write, sync::Arc};

use async_trait::async_trait;
use conduwuit::{Pdu, Result, SyncMutex, err, utils::math::usize_from_f64};
use database::Map;
use lru_cache::LruCache;
use ruma::{
	EventEncryptionAlgorithm, JsOption, OwnedRoomAliasId, RoomId, UserId,
	events::{
		StateEventType,
		room::{
			avatar::RoomAvatarEventContent,
			canonical_alias::RoomCanonicalAliasEventContent,
			create::RoomCreateEventContent,
			encryption::RoomEncryptionEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::RoomMemberEventContent,
			name::RoomNameEventContent,
			topic::RoomTopicEventContent,
		},
	},
	room::RoomType,
};

use crate::{Dep, rooms};

pub struct Service {
	services: Services,
	db: Data,
	room_state_cache: SyncMutex<RoomStateLruCache>,
}

type RoomStateCacheKey = (ruma::OwnedRoomId, StateEventType, String);
type RoomStateLruCache = LruCache<RoomStateCacheKey, Option<Pdu>>;

struct Services {
	short: Dep<rooms::short::Service>,
	state: Dep<rooms::state::Service>,
	state_compressor: Dep<rooms::state_compressor::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	timeline: Dep<rooms::timeline::Service>,
}

struct Data {
	shorteventid_shortstatehash: Arc<Map>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let cache_capacity =
			f64::from(config.stateinfo_cache_capacity) * config.cache_capacity_modifier;
		Ok(Arc::new(Self {
			services: Services {
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
			},
			db: Data {
				shorteventid_shortstatehash: args.db["shorteventid_shortstatehash"].clone(),
			},
			room_state_cache: LruCache::new(usize_from_f64(cache_capacity)?).into(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let cache_len = self.room_state_cache.lock().len();
		writeln!(out, "room_state_cache: {cache_len}")?;
		Ok(())
	}

	async fn clear_cache(&self) { self.room_state_cache.lock().clear(); }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub async fn get_name(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomName, "")
			.await
			.map(|c: RoomNameEventContent| c.name)
	}

	pub async fn get_avatar(&self, room_id: &RoomId) -> JsOption<RoomAvatarEventContent> {
		let content = self
			.room_state_get_content(room_id, &StateEventType::RoomAvatar, "")
			.await
			.ok();

		JsOption::from_option(content)
	}

	pub async fn get_member(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<RoomMemberEventContent> {
		self.room_state_get_content(room_id, &StateEventType::RoomMember, user_id.as_str())
			.await
	}

	/// Checks if guests are able to view room content without joining
	pub async fn is_world_readable(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
			.await
			.map(|c: RoomHistoryVisibilityEventContent| {
				c.history_visibility == HistoryVisibility::WorldReadable
			})
			.unwrap_or(false)
	}

	/// Checks if guests are able to join a given room
	pub async fn guest_can_join(&self, room_id: &RoomId) -> bool {
		self.room_state_get_content(room_id, &StateEventType::RoomGuestAccess, "")
			.await
			.map(|c: RoomGuestAccessEventContent| c.guest_access == GuestAccess::CanJoin)
			.unwrap_or(false)
	}

	/// Gets the primary alias from canonical alias event
	pub async fn get_canonical_alias(&self, room_id: &RoomId) -> Result<OwnedRoomAliasId> {
		self.room_state_get_content(room_id, &StateEventType::RoomCanonicalAlias, "")
			.await
			.and_then(|c: RoomCanonicalAliasEventContent| {
				c.alias
					.ok_or_else(|| err!(Request(NotFound("No alias found in event content."))))
			})
	}

	/// Gets the room topic
	pub async fn get_room_topic(&self, room_id: &RoomId) -> Result<String> {
		self.room_state_get_content(room_id, &StateEventType::RoomTopic, "")
			.await
			.map(|c: RoomTopicEventContent| c.topic)
	}

	/// Returns the join rules for a given room (`JoinRule` type). Will default
	/// to Invite if doesnt exist or invalid
	pub async fn get_join_rules(&self, room_id: &RoomId) -> JoinRule {
		self.room_state_get_content(room_id, &StateEventType::RoomJoinRules, "")
			.await
			.map_or(JoinRule::Invite, |c: RoomJoinRulesEventContent| c.join_rule)
	}

	pub async fn get_room_type(&self, room_id: &RoomId) -> Result<RoomType> {
		self.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
			.await
			.and_then(|content: RoomCreateEventContent| {
				content
					.room_type
					.ok_or_else(|| err!(Request(NotFound("No type found in event content"))))
			})
	}

	/// Gets the room's encryption algorithm if `m.room.encryption` state event
	/// is found
	pub async fn get_room_encryption(
		&self,
		room_id: &RoomId,
	) -> Result<EventEncryptionAlgorithm> {
		self.room_state_get_content(room_id, &StateEventType::RoomEncryption, "")
			.await
			.map(|content: RoomEncryptionEventContent| content.algorithm)
	}

	pub async fn is_encrypted_room(&self, room_id: &RoomId) -> bool {
		self.room_state_get(room_id, &StateEventType::RoomEncryption, "")
			.await
			.is_ok()
	}

	pub fn invalidate_room_state(
		&self,
		room_id: &RoomId,
		event_type: &StateEventType,
		state_key: &str,
	) {
		self.room_state_cache.lock().remove(&(
			room_id.to_owned(),
			event_type.clone(),
			state_key.to_owned(),
		));
	}
}
