mod watch;

use std::{
	collections::{BTreeMap, BTreeSet},
	sync::Arc,
	time::Duration,
};

use conduwuit::{Result, Server, SyncMutex};
use database::Map;
use moka::sync::Cache;
use ruma::{
	OwnedDeviceId, OwnedRoomId, OwnedUserId,
	api::client::sync::sync_events::{
		self,
		v4::{ExtensionsConfig, SyncRequestList},
		v5,
	},
};

use crate::{Dep, rooms};

pub struct Service {
	db: Data,
	services: Services,
	connections: DbConnections<DbConnectionsKey, DbConnectionsVal>,
	snake_connections: DbConnections<SnakeConnectionsKey, SnakeConnectionsVal>,
}

pub struct Data {
	todeviceid_events: Arc<Map>,
	userroomid_joined: Arc<Map>,
	userroomid_invitestate: Arc<Map>,
	userroomid_leftstate: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	pduid_pdu: Arc<Map>,
	keychangeid_userid: Arc<Map>,
	roomusertype_roomuserdataid: Arc<Map>,
	readreceiptid_readreceipt: Arc<Map>,
	userid_lastonetimekeyupdate: Arc<Map>,
}

struct Services {
	server: Arc<Server>,
	short: Dep<rooms::short::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	typing: Dep<rooms::typing::Service>,
}

struct SlidingSyncCache {
	lists: BTreeMap<String, SyncRequestList>,
	subscriptions: BTreeMap<OwnedRoomId, sync_events::v4::RoomSubscription>,
	// For every room, the roomsince number
	known_rooms: BTreeMap<String, BTreeMap<OwnedRoomId, u64>>,
	extensions: ExtensionsConfig,
}

#[derive(Default)]
struct SnakeSyncCache {
	known_rooms: BTreeMap<String, BTreeMap<OwnedRoomId, u64>>,
	timeline_limits: BTreeMap<OwnedRoomId, usize>,
}

type DbConnections<K, V> = Cache<K, V>;
type DbConnectionsKey = (OwnedUserId, OwnedDeviceId, String);
type DbConnectionsVal = Arc<SyncMutex<SlidingSyncCache>>;
type SnakeConnectionsKey = (OwnedUserId, OwnedDeviceId, Option<String>);
type SnakeConnectionsVal = Arc<SyncMutex<SnakeSyncCache>>;

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				todeviceid_events: args.db["todeviceid_events"].clone(),
				userroomid_joined: args.db["userroomid_joined"].clone(),
				userroomid_invitestate: args.db["userroomid_invitestate"].clone(),
				userroomid_leftstate: args.db["userroomid_leftstate"].clone(),
				userroomid_notificationcount: args.db["userroomid_notificationcount"].clone(),
				userroomid_highlightcount: args.db["userroomid_highlightcount"].clone(),
				pduid_pdu: args.db["pduid_pdu"].clone(),
				keychangeid_userid: args.db["keychangeid_userid"].clone(),
				roomusertype_roomuserdataid: args.db["roomusertype_roomuserdataid"].clone(),
				readreceiptid_readreceipt: args.db["readreceiptid_readreceipt"].clone(),
				userid_lastonetimekeyupdate: args.db["userid_lastonetimekeyupdate"].clone(),
			},
			services: Services {
				server: args.server.clone(),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				typing: args.depend::<rooms::typing::Service>("rooms::typing"),
			},
			connections: Cache::builder()
				.time_to_idle(Duration::from_secs(12 * 60 * 60))
				.build(),
			snake_connections: Cache::builder()
				.time_to_idle(Duration::from_secs(12 * 60 * 60))
				.build(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub fn snake_connection_cached(&self, key: &SnakeConnectionsKey) -> bool {
		self.snake_connections.contains_key(key)
	}

	pub fn forget_snake_sync_connection(&self, key: &SnakeConnectionsKey) {
		self.snake_connections.invalidate(key);
	}

	pub fn remembered(&self, key: &DbConnectionsKey) -> bool {
		self.connections.contains_key(key)
	}

	pub fn forget_sync_request_connection(&self, key: &DbConnectionsKey) {
		self.connections.invalidate(key);
	}

	pub fn update_snake_sync_request_with_cache(
		&self,
		snake_key: &SnakeConnectionsKey,
		_request: &mut v5::Request,
	) -> (BTreeMap<String, BTreeMap<OwnedRoomId, u64>>, BTreeMap<OwnedRoomId, usize>) {
		let cached_arc = self
			.snake_connections
			.get_with(snake_key.clone(), || Arc::new(SyncMutex::new(SnakeSyncCache::default())));
		let cached = cached_arc.lock();
		(cached.known_rooms.clone(), cached.timeline_limits.clone())
	}

	pub fn update_sync_request_with_cache(
		&self,
		key: &SnakeConnectionsKey,
		request: &mut sync_events::v4::Request,
	) -> BTreeMap<String, BTreeMap<OwnedRoomId, u64>> {
		let Some(conn_id) = request.conn_id.clone() else {
			return BTreeMap::new();
		};

		let key = into_db_key(key.0.clone(), key.1.clone(), conn_id);
		let cached_arc = self.connections.get_with(key, || {
			Arc::new(SyncMutex::new(SlidingSyncCache {
				lists: BTreeMap::new(),
				subscriptions: BTreeMap::new(),
				known_rooms: BTreeMap::new(),
				extensions: ExtensionsConfig::default(),
			}))
		});
		let mut cached = cached_arc.lock();

		for (list_id, list) in &mut request.lists {
			if let Some(cached_list) = cached.lists.get(list_id) {
				list_or_sticky(&mut list.sort, &cached_list.sort);
				list_or_sticky(
					&mut list.room_details.required_state,
					&cached_list.room_details.required_state,
				);
				some_or_sticky(
					&mut list.room_details.timeline_limit,
					cached_list.room_details.timeline_limit,
				);
				some_or_sticky(
					&mut list.include_old_rooms,
					cached_list.include_old_rooms.clone(),
				);
				match (&mut list.filters, cached_list.filters.clone()) {
					| (Some(filter), Some(cached_filter)) => {
						some_or_sticky(&mut filter.is_dm, cached_filter.is_dm);
						list_or_sticky(&mut filter.spaces, &cached_filter.spaces);
						some_or_sticky(&mut filter.is_encrypted, cached_filter.is_encrypted);
						some_or_sticky(&mut filter.is_invite, cached_filter.is_invite);
						list_or_sticky(&mut filter.room_types, &cached_filter.room_types);
						// Should be made possible to change
						list_or_sticky(&mut filter.not_room_types, &cached_filter.not_room_types);
						some_or_sticky(&mut filter.room_name_like, cached_filter.room_name_like);
						list_or_sticky(&mut filter.tags, &cached_filter.tags);
						list_or_sticky(&mut filter.not_tags, &cached_filter.not_tags);
					},
					| (_, Some(cached_filters)) => list.filters = Some(cached_filters),
					| (Some(list_filters), _) => list.filters = Some(list_filters.clone()),
					| (..) => {},
				}
				list_or_sticky(&mut list.bump_event_types, &cached_list.bump_event_types);
			}
			cached.lists.insert(list_id.clone(), list.clone());
		}

		cached
			.subscriptions
			.extend(request.room_subscriptions.clone());
		request
			.room_subscriptions
			.extend(cached.subscriptions.clone());

		request.extensions.e2ee.enabled = request
			.extensions
			.e2ee
			.enabled
			.or(cached.extensions.e2ee.enabled);

		request.extensions.to_device.enabled = request
			.extensions
			.to_device
			.enabled
			.or(cached.extensions.to_device.enabled);

		request.extensions.account_data.enabled = request
			.extensions
			.account_data
			.enabled
			.or(cached.extensions.account_data.enabled);
		request.extensions.account_data.lists = request
			.extensions
			.account_data
			.lists
			.clone()
			.or_else(|| cached.extensions.account_data.lists.clone());
		request.extensions.account_data.rooms = request
			.extensions
			.account_data
			.rooms
			.clone()
			.or_else(|| cached.extensions.account_data.rooms.clone());

		cached.extensions = request.extensions.clone();

		cached.known_rooms.clone()
	}

	pub fn update_sync_subscriptions(
		&self,
		key: &DbConnectionsKey,
		subscriptions: BTreeMap<OwnedRoomId, sync_events::v4::RoomSubscription>,
	) {
		let cached_arc = self.connections.get_with(key.clone(), || {
			Arc::new(SyncMutex::new(SlidingSyncCache {
				lists: BTreeMap::new(),
				subscriptions: BTreeMap::new(),
				known_rooms: BTreeMap::new(),
				extensions: ExtensionsConfig::default(),
			}))
		});
		let mut cached = cached_arc.lock();

		cached.subscriptions = subscriptions;
	}

	pub fn update_sync_known_rooms(
		&self,
		key: &DbConnectionsKey,
		list_id: String,
		new_cached_rooms: BTreeSet<OwnedRoomId>,
		globalsince: u64,
	) {
		let cached_arc = self.connections.get_with(key.clone(), || {
			Arc::new(SyncMutex::new(SlidingSyncCache {
				lists: BTreeMap::new(),
				subscriptions: BTreeMap::new(),
				known_rooms: BTreeMap::new(),
				extensions: ExtensionsConfig::default(),
			}))
		});
		let mut cached = cached_arc.lock();

		for (room_id, lastsince) in cached
			.known_rooms
			.entry(list_id.clone())
			.or_default()
			.iter_mut()
		{
			if !new_cached_rooms.contains(room_id) {
				*lastsince = 0;
			}
		}
		let list = cached.known_rooms.entry(list_id).or_default();
		for room_id in new_cached_rooms {
			list.insert(room_id, globalsince);
		}
	}

	pub fn update_snake_sync_known_rooms(
		&self,
		key: &SnakeConnectionsKey,
		list_id: String,
		new_cached_rooms: BTreeSet<OwnedRoomId>,
		globalsince: u64,
	) {
		assert!(key.2.is_some(), "Some(conn_id) required for this call");
		let cached_arc = self
			.snake_connections
			.get_with(key.clone(), || Arc::new(SyncMutex::new(SnakeSyncCache::default())));
		let mut cached = cached_arc.lock();

		for (room_id, lastsince) in cached
			.known_rooms
			.entry(list_id.clone())
			.or_default()
			.iter_mut()
		{
			if !new_cached_rooms.contains(room_id) {
				*lastsince = 0;
			}
		}
		let list = cached.known_rooms.entry(list_id).or_default();
		for room_id in new_cached_rooms {
			list.insert(room_id, globalsince);
		}
	}

	pub fn update_snake_sync_timeline_limits(
		&self,
		key: &SnakeConnectionsKey,
		limits: BTreeMap<OwnedRoomId, usize>,
	) {
		let cached_arc = self
			.snake_connections
			.get_with(key.clone(), || Arc::new(SyncMutex::new(SnakeSyncCache::default())));
		let mut cached = cached_arc.lock();
		for (room_id, limit) in limits {
			cached.timeline_limits.insert(room_id, limit);
		}
	}
}

#[inline]
pub fn into_snake_key<U, D, C>(user_id: U, device_id: D, conn_id: C) -> SnakeConnectionsKey
where
	U: Into<OwnedUserId>,
	D: Into<OwnedDeviceId>,
	C: Into<Option<String>>,
{
	(user_id.into(), device_id.into(), conn_id.into())
}

#[inline]
pub fn into_db_key<U, D, C>(user_id: U, device_id: D, conn_id: C) -> DbConnectionsKey
where
	U: Into<OwnedUserId>,
	D: Into<OwnedDeviceId>,
	C: Into<String>,
{
	(user_id.into(), device_id.into(), conn_id.into())
}

/// load params from cache if body doesn't contain it, as long as it's allowed
/// in some cases we may need to allow an empty list as an actual value
fn list_or_sticky<T: Clone>(target: &mut Vec<T>, cached: &Vec<T>) {
	if target.is_empty() {
		target.clone_from(cached);
	}
}

fn some_or_sticky<T>(target: &mut Option<T>, cached: Option<T>) {
	if target.is_none() {
		*target = cached;
	}
}
