use std::sync::Arc;

use conduwuit::{
	Result, debug_warn, utils,
	utils::{ReadyExt, mutex_map::MutexMap, stream::TryIgnore},
};
use database::{Deserialized, Json, Map};
use futures::Stream;
use moka::sync::Cache as MokaCache;
use ruma::{OwnedUserId, UInt, UserId, events::presence::PresenceEvent, presence::PresenceState};

use super::Presence;
use crate::{Dep, globals, users};

pub(crate) struct Data {
	presenceid_presence: Arc<Map>,
	userid_presenceid: Arc<Map>,
	cache: MokaCache<OwnedUserId, (u64, Presence)>,
	locks: MutexMap<OwnedUserId, ()>,
	services: Services,
}

struct Services {
	globals: Dep<globals::Service>,
	users: Dep<users::Service>,
}

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let config = &args.server.config;
		let cache_capacity = config.presence_cache_capacity;
		let cache_capacity = utils::math::usize_from_f64(
			f64::from(cache_capacity) * config.cache_capacity_modifier,
		)
		.expect("valid cache size");

		Self {
			presenceid_presence: args.db["presenceid_presence"].clone(),
			userid_presenceid: args.db["userid_presenceid"].clone(),
			cache: MokaCache::builder()
				.max_capacity(cache_capacity as u64)
				.build(),
			locks: MutexMap::new(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				users: args.depend::<users::Service>("users"),
			},
		}
	}

	/// Returns raw cached presence (w/o profile data), fetching from
	/// DB and populating cache if necessary.
	pub(super) async fn get_presence_raw(&self, user_id: &UserId) -> Result<(u64, Presence)> {
		if let Some(cached) = self.cache.get(&user_id.to_owned()) {
			return Ok(cached);
		}

		let count = self
			.userid_presenceid
			.get(user_id)
			.await
			.deserialized::<u64>()?;

		let key = presenceid_key(count, user_id);
		let bytes = self.presenceid_presence.get(&key).await?;
		let presence = Presence::from_json_bytes(&bytes)?;

		self.cache
			.insert(user_id.to_owned(), (count, presence.clone()));

		Ok((count, presence))
	}

	/// Returns the full `PresenceEvent` (including profile data from aux cache)
	pub(super) async fn get_presence(&self, user_id: &UserId) -> Result<(u64, PresenceEvent)> {
		let (count, presence) = self.get_presence_raw(user_id).await?;
		let event = presence
			.to_presence_event(user_id, &self.services.users)
			.await;

		Ok((count, event))
	}

	pub(super) async fn set_presence(
		&self,
		user_id: &UserId,
		presence_state: &PresenceState,
		currently_active: Option<bool>,
		last_active_ago: Option<UInt>,
		status_msg: Option<String>,
		_previous: Option<(u64, Presence)>,
	) -> Result<()> {
		let _lock = self.locks.lock(user_id).await;

		let last_presence = if let Some(prev) = _previous {
			Ok(prev)
		} else {
			self.get_presence_raw(user_id).await
		};

		let state_changed = match last_presence {
			| Err(_) => true,
			| Ok(ref presence) => presence.1.state != *presence_state,
		};

		let status_msg_changed = match last_presence {
			| Err(_) => true,
			| Ok(ref last_presence) => {
				let old_msg = last_presence.1.status_msg.clone().unwrap_or_default();

				let new_msg = status_msg.clone().unwrap_or_default();

				new_msg != old_msg
			},
		};

		let now = utils::millis_since_unix_epoch();
		let last_last_active_ts = match last_presence {
			| Err(_) => 0,
			| Ok((_, ref presence)) => presence.last_active_ts,
		};

		let last_active_ts = match last_active_ago {
			| None => now,
			| Some(last_active_ago) => now.saturating_sub(last_active_ago.into()),
		};

		// TODO: tighten for state flicker?
		if !status_msg_changed && !state_changed && last_active_ts < last_last_active_ts {
			debug_warn!(
				"presence spam {user_id:?} last_active_ts:{last_active_ts:?} < \
				 {last_last_active_ts:?}",
			);
			return Ok(());
		}

		let status_msg = if status_msg.as_ref().is_some_and(String::is_empty) {
			None
		} else {
			status_msg
		};

		let presence = Presence::new(
			presence_state.to_owned(),
			currently_active.unwrap_or(false),
			last_active_ts,
			status_msg,
		);

		let count = self.services.globals.next_count()?;
		let key = presenceid_key(count, user_id);

		self.presenceid_presence.raw_put(key, Json(&presence));
		self.userid_presenceid.raw_put(user_id, count);

		self.cache.insert(user_id.to_owned(), (count, presence));

		if let Ok((last_count, _)) = last_presence {
			let key = presenceid_key(last_count, user_id);
			self.presenceid_presence.remove(&key);
		}

		Ok(())
	}

	pub(super) async fn remove_presence(&self, user_id: &UserId) {
		let _lock = self.locks.lock(user_id).await;

		self.cache.invalidate(&user_id.to_owned());

		let Ok(count) = self
			.userid_presenceid
			.get(user_id)
			.await
			.deserialized::<u64>()
		else {
			return;
		};

		let key = presenceid_key(count, user_id);
		self.presenceid_presence.remove(&key);
		self.userid_presenceid.remove(user_id);
	}

	pub(super) fn clear_cache(&self) { self.cache.invalidate_all(); }

	#[inline]
	pub(super) fn presence_since(
		&self,
		since: u64,
	) -> impl Stream<Item = (&UserId, u64, &[u8])> + Send + '_ {
		self.presenceid_presence
			.raw_stream()
			.ignore_err()
			.ready_filter_map(move |(key, presence)| {
				let (count, user_id) = presenceid_parse(key).ok()?;
				(count > since).then_some((user_id, count, presence))
			})
	}
}

#[inline]
fn presenceid_key(count: u64, user_id: &UserId) -> Vec<u8> {
	let cap = size_of::<u64>().saturating_add(user_id.as_bytes().len());
	let mut key = Vec::with_capacity(cap);
	key.extend_from_slice(&count.to_be_bytes());
	key.extend_from_slice(user_id.as_bytes());
	key
}

#[inline]
fn presenceid_parse(key: &[u8]) -> Result<(u64, &UserId)> {
	let (count, user_id) = key.split_at(8);
	let user_id = user_id_from_bytes(user_id)?;
	let count = utils::u64_from_u8(count);

	Ok((count, user_id))
}

/// Parses a `UserId` from bytes.
fn user_id_from_bytes(bytes: &[u8]) -> Result<&UserId> {
	let str: &str = utils::str_from_bytes(bytes)?;
	let user_id: &UserId = str.try_into()?;

	Ok(user_id)
}

#[cfg(test)]
mod tests {
	use ruma::presence::PresenceState;

	use super::*;
	use crate::presence::Presence;

	/// encode, and then decode presence DB key
	#[test]
	fn presenceid_key_roundtrip() {
		let user_id: &UserId = "@alice:example.com".try_into().unwrap();
		let count = 42_u64;

		let key = presenceid_key(count, user_id);
		let (parsed_count, parsed_user_id) = presenceid_parse(&key).unwrap();

		assert_eq!(parsed_count, count);
		assert_eq!(parsed_user_id, user_id);
	}

	/// big-endian count & chronological key sorting
	#[test]
	fn presenceid_key_ordering() {
		let user_id: &UserId = "@bob:example.com".try_into().unwrap();

		let key_low = presenceid_key(1, user_id);
		let key_high = presenceid_key(1000, user_id);

		assert!(key_low < key_high, "keys must sort by count ascending");
	}

	/// boundary: count = 0
	#[test]
	fn presenceid_key_zero_count() {
		let user_id: &UserId = "@zero:example.com".try_into().unwrap();

		let key = presenceid_key(0, user_id);
		let (parsed_count, parsed_user_id) = presenceid_parse(&key).unwrap();

		assert_eq!(parsed_count, 0);
		assert_eq!(parsed_user_id, user_id);
	}

	/// boundary: count = u64::MAX
	#[test]
	fn presenceid_key_max_count() {
		let user_id: &UserId = "@max:example.com".try_into().unwrap();

		let key = presenceid_key(u64::MAX, user_id);
		let (parsed_count, parsed_user_id) = presenceid_parse(&key).unwrap();

		assert_eq!(parsed_count, u64::MAX);
		assert_eq!(parsed_user_id, user_id);
	}

	/// JSON round-trip with all fields populated
	#[test]
	fn presence_serde_roundtrip() {
		let presence = Presence::new(
			PresenceState::Online,
			true,
			1_700_000_000_000,
			Some("working".to_owned()),
		);

		let json = serde_json::to_vec(&presence).unwrap();
		let parsed = Presence::from_json_bytes(&json).unwrap();

		assert_eq!(parsed.state, PresenceState::Online);
		assert!(parsed.currently_active);
		assert_eq!(parsed.last_active_ts, 1_700_000_000_000);
		assert_eq!(parsed.status_msg.as_deref(), Some("working"));
	}

	/// JSON round-trip w/o status_msg
	#[test]
	fn presence_serde_roundtrip_no_status() {
		let presence = Presence::new(PresenceState::Unavailable, false, 1_600_000_000_000, None);

		let json = serde_json::to_vec(&presence).unwrap();
		let parsed = Presence::from_json_bytes(&json).unwrap();

		assert_eq!(parsed.state, PresenceState::Unavailable);
		assert!(!parsed.currently_active);
		assert_eq!(parsed.last_active_ts, 1_600_000_000_000);
		assert!(parsed.status_msg.is_none());
	}
}
