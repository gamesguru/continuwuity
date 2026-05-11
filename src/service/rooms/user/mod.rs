use std::sync::Arc;

use conduwuit::{Result, debug, implement, utils::MutexMap};
use database::{Deserialized, Map};
use ruma::{OwnedUserId, RoomId, UserId};

use crate::{Dep, globals};

pub struct Service {
	pub notification_mutex: MutexMap<Vec<u8>, ()>,
	db: Data,
	services: Services,
}

struct Data {
	userroomid_notificationcount: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	roomuserid_lastnotificationread: Arc<Map>,
}

struct Services {
	globals: Dep<globals::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			notification_mutex: MutexMap::new(),
			db: Data {
				userroomid_notificationcount: args.db["userroomid_notificationcount"].clone(),
				userroomid_highlightcount: args.db["userroomid_highlightcount"].clone(),
				roomuserid_lastnotificationread: args.db["roomuserid_lastnotificationread"]
					.clone(),
			},

			services: Services {
				globals: args.depend::<globals::Service>("globals"),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub async fn reset_notification_counts(&self, user_id: &UserId, room_id: &RoomId) {
	let mut key = user_id.as_bytes().to_vec();
	key.push(0xFF);
	key.extend_from_slice(room_id.as_bytes());
	let _lock = self.notification_mutex.lock(&*key).await;

	debug!(%user_id, %room_id, "Resetting notification counts");

	let userroom_id = (user_id, room_id);
	self.db.userroomid_highlightcount.put(userroom_id, 0_u64);
	self.db.userroomid_notificationcount.put(userroom_id, 0_u64);

	let roomuser_id = (room_id, user_id);
	let count = self.services.globals.next_count().unwrap();
	self.db
		.roomuserid_lastnotificationread
		.put(roomuser_id, count);
}

#[implement(Service)]
pub async fn increment_notification_counts(
	&self,
	room_id: &RoomId,
	notifies: Vec<OwnedUserId>,
	highlights: Vec<OwnedUserId>,
) {
	for user_id in notifies {
		let mut userroom_id = user_id.as_bytes().to_vec();
		userroom_id.push(0xFF);
		userroom_id.extend_from_slice(room_id.as_bytes());

		let _lock = self.notification_mutex.lock(&*userroom_id).await;

		let old = self
			.db
			.userroomid_notificationcount
			.get_blocking(&userroom_id);
		let new = conduwuit::utils::increment(old.as_ref().ok().map(|v| &**v));
		self.db
			.userroomid_notificationcount
			.insert(&userroom_id, new);

		debug!(
			%user_id,
			%room_id,
			old = ?old.as_ref().ok().map(|v| conduwuit::utils::u64_from_bytes(v)),
			new = ?conduwuit::utils::u64_from_bytes(&new),
			"Incremented notification count"
		);
	}

	for user_id in highlights {
		let mut userroom_id = user_id.as_bytes().to_vec();
		userroom_id.push(0xFF);
		userroom_id.extend_from_slice(room_id.as_bytes());

		let _lock = self.notification_mutex.lock(&*userroom_id).await;

		let old = self.db.userroomid_highlightcount.get_blocking(&userroom_id);
		let new = conduwuit::utils::increment(old.as_ref().ok().map(|v| &**v));
		self.db.userroomid_highlightcount.insert(&userroom_id, new);

		debug!(
			%user_id,
			%room_id,
			old = ?old.as_ref().ok().map(|v| conduwuit::utils::u64_from_bytes(v)),
			new = ?conduwuit::utils::u64_from_bytes(&new),
			"Incremented highlight count"
		);
	}
}

#[implement(Service)]
pub async fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (user_id, room_id);
	self.db
		.userroomid_notificationcount
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}

#[implement(Service)]
pub async fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (user_id, room_id);
	self.db
		.userroomid_highlightcount
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}

#[implement(Service)]
pub async fn last_notification_read(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (room_id, user_id);
	self.db
		.roomuserid_lastnotificationread
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}
