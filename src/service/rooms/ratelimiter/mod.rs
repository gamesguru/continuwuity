use std::{
	collections::HashMap,
	ops::{Add, Sub},
	sync::Arc,
};

use async_trait::async_trait;
use conduwuit::SyncRwLock;
use ruma::{OwnedRoomId, OwnedUserId};

#[derive(Clone, Debug)]
pub struct RateLimitState {
	last_hit: std::time::Instant,
	resets: std::time::Instant,
	hits: u64,
}

pub struct Service {
	userid_roomid_ratelimiter:
		Arc<SyncRwLock<HashMap<(OwnedUserId, OwnedRoomId), RateLimitState>>>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(_: crate::Args<'_>) -> conduwuit::Result<Arc<Self>> {
		Ok(Arc::new(Self {
			userid_roomid_ratelimiter: Arc::new(SyncRwLock::new(HashMap::new())),
		}))
	}

	async fn clear_cache(&self) {
		let mut rl = self.userid_roomid_ratelimiter.write();
		rl.clear();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub fn hit(&self, user_id: OwnedUserId, room_id: OwnedRoomId) {
		let now = std::time::Instant::now();
		let key = (user_id, room_id);
		let mut rl = self.userid_roomid_ratelimiter.write();
		let state = rl.entry(key).or_insert_with(|| RateLimitState {
			last_hit: std::time::Instant::now(),
			resets: now.add(std::time::Duration::from_secs(10)),
			hits: 0,
		});
		if now > state.resets {
			state.hits = 0;
			state.resets = now.add(std::time::Duration::from_secs(10));
		}

		state.hits = state.hits.saturating_add(1);
		state.last_hit = std::time::Instant::now();
	}

	pub fn reset(&self, user_id: OwnedUserId, room_id: OwnedRoomId) {
		let key = (user_id, room_id);
		let mut rl = self.userid_roomid_ratelimiter.write();
		rl.remove(&key);
	}

	#[must_use]
	pub fn reset_after(
		&self,
		user_id: OwnedUserId,
		room_id: OwnedRoomId,
	) -> Option<std::time::Duration> {
		let key = (user_id, room_id);
		let rl = self.userid_roomid_ratelimiter.read();

		// If the user has more than 10 hits and the reset time is in the future,
		// return how long until they can hit again.
		if let Some(state) = rl.get(&key) {
			if state.resets > std::time::Instant::now() && state.hits > 10 {
				return Some(state.resets.sub(std::time::Instant::now()));
			}
		}
		None
	}
}
