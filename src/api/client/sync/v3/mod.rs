mod joined;
mod left;
mod state;

use std::{
	cmp::{self},
	collections::{BTreeMap, HashMap, HashSet},
	time::Duration,
};

use axum::{extract::State, response::IntoResponse};
use axum_client_ip::InsecureClientIp;
use conduwuit::{
	Result, at, extract_variant,
	utils::{
		ReadyExt, TryFutureExtExt,
		stream::{BroadbandExt, Tools, WidebandExt},
	},
	warn,
};
use conduwuit_service::Services;
use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{OptionFuture, join3, join4},
};
use ruma::{
	DeviceId, OwnedUserId, RoomId, UserId,
	api::{
		OutgoingResponse,
		client::{
			filter::FilterDefinition,
			sync::sync_events::{
				self, DeviceLists,
				v3::{
					Filter, GlobalAccountData, InviteState, InvitedRoom, KnockState, KnockedRoom,
					Presence, Rooms, ToDevice,
				},
			},
			uiaa::UiaaResponse,
		},
	},
	events::{
		AnyGlobalAccountDataEvent, AnyRawAccountDataEvent,
		presence::{PresenceEvent, PresenceEventContent},
	},
	serde::Raw,
};
use service::rooms::lazy_loading::{self, MemberSet, Options as _};

use super::{load_timeline, share_encrypted_room};
use crate::{
	Ruma, RumaResponse,
	client::{
		is_ignored_invite,
		sync::v3::{joined::load_joined_room, left::load_left_room},
	},
};

/// The default maximum number of events to return in the `timeline` key of
/// joined and left rooms. If the number of events sent since the last sync
/// exceeds this number, the `timeline` will be `limited`.
const DEFAULT_TIMELINE_LIMIT: usize = 30;

/// A collection of updates to users' device lists, used for E2EE.
struct DeviceListUpdates {
	changed: HashSet<OwnedUserId>,
	left: HashSet<OwnedUserId>,
}

impl DeviceListUpdates {
	fn new() -> Self {
		Self {
			changed: HashSet::new(),
			left: HashSet::new(),
		}
	}

	fn merge(&mut self, other: Self) {
		self.changed.extend(other.changed);
		self.left.extend(other.left);
	}

	fn is_empty(&self) -> bool { self.changed.is_empty() && self.left.is_empty() }
}

impl From<DeviceListUpdates> for DeviceLists {
	fn from(val: DeviceListUpdates) -> Self {
		Self {
			changed: val.changed.into_iter().collect(),
			left: val.left.into_iter().collect(),
		}
	}
}

/// References to common data needed to calculate the sync response.
#[derive(Clone, Copy)]
struct SyncContext<'a> {
	/// The ID of the user requesting this sync.
	syncing_user: &'a UserId,
	/// The ID of the device requesting this sync, which will belong to
	/// `syncing_user`.
	syncing_device: &'a DeviceId,
	/// The global count at the end of the previous sync response.
	/// The previous sync's `current_count` will become the next sync's
	/// `last_sync_end_count`. This will be None if no `since` query parameter
	/// was specified, indicating an initial sync.
	last_sync_end_count: Option<u64>,
	/// The global count as of when we started building the sync response.
	/// This is used as an upper bound when querying the database to ensure the
	/// response represents a snapshot in time and doesn't include data which
	/// appeared while the response was being built.
	current_count: u64,
	/// The `full_state` query parameter, used when syncing state for joined and
	/// left rooms.
	full_state: bool,
	/// The sync filter, which the client uses to specify what data should be
	/// included in the sync response.
	filter: &'a FilterDefinition,
}

impl<'a> SyncContext<'a> {
	fn lazy_loading_context(&self, room_id: &'a RoomId) -> lazy_loading::Context<'a> {
		lazy_loading::Context {
			user_id: self.syncing_user,
			device_id: Some(self.syncing_device),
			room_id,
			token: self.last_sync_end_count,
			options: Some(&self.filter.room.state.lazy_load_options),
		}
	}

	#[inline]
	fn lazy_loading_enabled(&self) -> bool {
		(self.filter.room.state.lazy_load_options.is_enabled()
			|| self.filter.room.timeline.lazy_load_options.is_enabled())
			&& !self.full_state
	}
}

type PresenceUpdates = HashMap<OwnedUserId, PresenceEventContent>;

/// # `GET /_matrix/client/r0/sync`
///
/// Synchronize the client's state with the latest state on the server.
///
/// - This endpoint takes a `since` parameter which should be the `next_batch`
///   value from a previous request for incremental syncs.
///
/// Calling this endpoint without a `since` parameter returns:
/// - Some of the most recent events of each timeline
/// - Notification counts for each room
/// - Joined and invited member counts, heroes
/// - All state events
///
/// Calling this endpoint with a `since` parameter from a previous `next_batch`
/// returns: For joined rooms:
/// - Some of the most recent events of each timeline that happened after since
/// - If user joined the room after since: All state events (unless lazy loading
///   is activated) and all device list updates in that room
/// - If the user was already in the room: A list of all events that are in the
///   state now, but were not in the state at `since`
/// - If the state we send contains a member event: Joined and invited member
///   counts, heroes
/// - Device list updates that happened after `since`
/// - If there are events in the timeline we send or the user send updated his
///   read mark: Notification counts
/// - EDUs that are active now (read receipts, typing updates, presence)
/// - TODO: Allow multiple sync streams to support Pantalaimon
///
/// For invited rooms:
/// - If the user was invited after `since`: A subset of the state of the room
///   at the point of the invite
///
/// For left rooms:
/// - If the user left after `since`: `prev_batch` token, empty state (TODO:
///   subset of the state at the point of the leave)
#[tracing::instrument(
	name = "sync",
	level = "debug",
	skip_all,
	fields(
		since = %body.body.since.as_deref().unwrap_or_default(),
    )
)]
pub(crate) async fn sync_events_route(
	State(services): State<crate::State>,
	InsecureClientIp(client_ip): InsecureClientIp,
	body: Ruma<sync_events::v3::Request>,
) -> Result<axum::response::Response, RumaResponse<UiaaResponse>> {
	let (sender_user, sender_device) = body.sender();

	// Presence update
	if services.config.allow_local_presence {
		services
			.presence
			.ping_presence(sender_user, &body.body.set_presence)
			.await?;
	}

	// Increment the "device last active" metadata
	services
		.users
		.update_device_last_seen(sender_user, Some(sender_device), client_ip)
		.await;

	// Setup watchers, so if there's no response, we can wait for them
	let watcher = services.sync.watch(sender_user, sender_device);

	let response = build_sync_events(&services, &body).await?;
	if body.body.since.is_none() || body.body.full_state || !is_sync_response_empty(&response) {
		return Ok(axum::Json(response).into_response());
	}

	// Hang a few seconds so requests are not spammed
	// Stop hanging if new info arrives
	let default = Duration::from_secs(30);
	let duration = cmp::min(body.body.timeout.unwrap_or(default), default);
	_ = tokio::time::timeout(duration, watcher).await;

	// Retry returning data
	let response = build_sync_events(&services, &body).await?;
	Ok(axum::Json(response).into_response())
}

fn is_sync_response_empty(val: &serde_json::Value) -> bool {
	let Some(obj) = val.as_object() else {
		return true;
	};

	let rooms_empty = obj.get("rooms").is_none();
	let presence_empty = obj.get("presence").is_none();
	let account_data_empty = obj.get("account_data").is_none();
	let to_device_empty = obj.get("to_device").is_none();
	let device_lists_empty = obj.get("device_lists").is_none();

	rooms_empty && presence_empty && account_data_empty && to_device_empty && device_lists_empty
}

pub(crate) async fn build_sync_events(
	services: &Services,
	body: &Ruma<sync_events::v3::Request>,
) -> Result<serde_json::Value, RumaResponse<UiaaResponse>> {
	let (syncing_user, syncing_device) = body.sender();

	let current_count = services.globals.current_count()?;

	// the `since` token is the last sync end count stringified
	let last_sync_end_count = body
		.body
		.since
		.as_ref()
		.and_then(|string| string.parse().ok());

	let full_state = body.body.full_state;

	// FilterDefinition is very large (0x1000 bytes), let's put it on the heap
	let filter = Box::new(match body.body.filter.as_ref() {
		// use the default filter if none was specified
		| None => FilterDefinition::default(),
		// use inline filters directly
		| Some(Filter::FilterDefinition(filter)) => filter.clone(),
		// look up filter IDs from the database
		| Some(Filter::FilterId(filter_id)) => services
			.users
			.get_filter(syncing_user, filter_id)
			.await
			.unwrap_or_default(),
	});

	let context = SyncContext {
		syncing_user,
		syncing_device,
		last_sync_end_count,
		current_count,
		full_state,
		filter: &filter,
	};

	let joined_rooms = services
		.rooms
		.state_cache
		.rooms_joined(syncing_user)
		.map(ToOwned::to_owned)
		.broad_filter_map(|room_id| async {
			let joined_room = load_joined_room(services, context, room_id.clone()).await;

			match joined_room {
				| Ok((room, state_after, updates, _)) =>
					Some((room_id, room, state_after, updates)),
				| Err(err) => {
					warn!(?err, %room_id, "error loading joined room");
					None
				},
			}
		})
		.ready_fold(
			(BTreeMap::new(), BTreeMap::new(), DeviceListUpdates::new()),
			|(mut joined_rooms, mut joined_state_after, mut all_updates),
			 (room_id, joined_room, state_after, updates)| {
				all_updates.merge(updates);

				if !joined_room.is_empty() || context.last_sync_end_count.is_none() {
					joined_rooms.insert(room_id.clone(), joined_room);
					if !state_after.is_empty() {
						joined_state_after.insert(room_id, state_after);
					}
				}

				(joined_rooms, joined_state_after, all_updates)
			},
		);

	let left_rooms = services
		.rooms
		.state_cache
		.rooms_left(syncing_user)
		.broad_filter_map(|(room_id, leave_pdu)| async {
			let left_room = load_left_room(services, context, room_id.clone(), leave_pdu).await;

			match left_room {
				| Ok(Some((room, state_after))) => Some((room_id, room, state_after)),
				| Ok(None) => None,
				| Err(err) => {
					warn!(?err, %room_id, "error loading joined room");
					None
				},
			}
		})
		.fold(
			(BTreeMap::new(), BTreeMap::new()),
			|(mut left_rooms, mut left_state_after), (room_id, left_room, state_after)| async move {
				left_rooms.insert(room_id.clone(), left_room);
				if !state_after.is_empty() {
					left_state_after.insert(room_id, state_after);
				}
				(left_rooms, left_state_after)
			},
		);

	let invited_rooms = services
		.rooms
		.state_cache
		.rooms_invited(syncing_user)
		.wide_filter_map(async |(room_id, invite_state)| {
			if is_ignored_invite(services, syncing_user, &room_id).await {
				None
			} else {
				Some((room_id, invite_state))
			}
		})
		.fold_default(|mut invited_rooms: BTreeMap<_, _>, (room_id, invite_state)| async move {
			let invite_count = services
				.rooms
				.state_cache
				.get_invite_count(&room_id, syncing_user)
				.await
				.ok();

			// only sync this invite if it was sent after the last /sync call
			if last_sync_end_count < invite_count {
				let invited_room = InvitedRoom {
					invite_state: InviteState { events: invite_state },
				};

				invited_rooms.insert(room_id, invited_room);
			}
			invited_rooms
		});

	let knocked_rooms = services
		.rooms
		.state_cache
		.rooms_knocked(syncing_user)
		.fold_default(|mut knocked_rooms: BTreeMap<_, _>, (room_id, knock_state)| async move {
			let knock_count = services
				.rooms
				.state_cache
				.get_knock_count(&room_id, syncing_user)
				.await
				.ok();

			// only sync this knock if it was sent after the last /sync call
			if last_sync_end_count < knock_count {
				let knocked_room = KnockedRoom {
					knock_state: KnockState { events: knock_state },
				};

				knocked_rooms.insert(room_id, knocked_room);
			}
			knocked_rooms
		});

	let (joined_rooms, left_rooms, invited_rooms, knocked_rooms) =
		join4(joined_rooms, left_rooms, invited_rooms, knocked_rooms).await;

	let (mut joined_rooms, mut joined_state_after, mut device_list_updates) = joined_rooms;
	let (left_rooms, left_state_after) = left_rooms;

	let presence_updates: OptionFuture<_> = services
		.config
		.allow_local_presence
		.then(|| process_presence_updates(services, last_sync_end_count, syncing_user))
		.into();

	let account_data: Vec<Raw<AnyGlobalAccountDataEvent>> = services
		.account_data
		.changes_since(None, syncing_user, last_sync_end_count, Some(current_count))
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Global))
		.collect()
		.await;

	// Look for device list updates of this account
	let keys_changed = services
		.users
		.keys_changed(syncing_user, last_sync_end_count, Some(current_count))
		.map(ToOwned::to_owned)
		.collect::<HashSet<_>>();

	let to_device_events = services
		.users
		.get_to_device_events(
			syncing_user,
			syncing_device,
			last_sync_end_count,
			Some(current_count),
		)
		.map(at!(1))
		.collect::<Vec<_>>();

	let device_one_time_keys_count = services
		.users
		.count_one_time_keys(syncing_user, syncing_device);

	// Remove all to-device events the device received *last time*
	let remove_to_device_events =
		services
			.users
			.remove_to_device_events(syncing_user, syncing_device, last_sync_end_count);

	let ephemeral = join3(remove_to_device_events, to_device_events, presence_updates);
	let top = join3(ephemeral, device_one_time_keys_count, keys_changed)
		.boxed()
		.await;

	let (ephemeral, device_one_time_keys_count, keys_changed) = top;
	let ((), to_device_events, presence_updates) = ephemeral;
	// #779: rooms_joined() uses a RocksDB prefix iterator that may miss
	// recently-joined rooms due to snapshot isolation. Check the in-memory
	// recently-joined set for any rooms that fell through.
	let recently_joined = services.rooms.state_cache.recently_joined_rooms(
		syncing_user,
		current_count,
		last_sync_end_count,
	);
	for room_id in recently_joined {
		if joined_rooms.contains_key(&room_id)
			|| left_rooms.contains_key(&room_id)
			|| invited_rooms.contains_key(&room_id)
		{
			continue;
		}
		warn!("#779: loading recently-joined room {room_id} missed by iterator");
		if let Ok((room, state_after, updates, _)) =
			load_joined_room(services, context, room_id.clone()).await
		{
			device_list_updates.merge(updates);

			if !room.is_empty() {
				joined_rooms.insert(room_id.clone(), room);
				if !state_after.is_empty() {
					joined_state_after.insert(room_id, state_after);
				}
			}
		}
	}

	let mut device_list_updates: DeviceLists = device_list_updates.into();
	device_list_updates.changed.extend(keys_changed);

	let ruma_response = sync_events::v3::Response {
		next_batch: current_count.to_string(),
		rooms: Rooms {
			leave: left_rooms,
			join: joined_rooms,
			invite: invited_rooms,
			knock: knocked_rooms,
		},
		presence: Presence {
			events: presence_updates
				.into_iter()
				.flat_map(IntoIterator::into_iter)
				.map(|(sender, content)| PresenceEvent { content, sender })
				.map(|ref event| Raw::new(event))
				.filter_map(Result::ok)
				.collect(),
		},
		account_data: GlobalAccountData { events: account_data },
		to_device: ToDevice { events: to_device_events },
		device_lists: device_list_updates,
		device_one_time_keys_count,
		device_unused_fallback_key_types: None,
	};

	let mut val: serde_json::Value = serde_json::from_slice(
		ruma_response
			.try_into_http_response::<bytes::BytesMut>()
			.expect("ruma response is valid")
			.body(),
	)
	.expect("ruma response is valid JSON");

	// Manually insert state_after data for MSC4222
	if let Some(join) = val.get_mut("rooms").and_then(|r| r.get_mut("join")) {
		for (room_id, state_after) in joined_state_after {
			if let Some(room) = join.get_mut(room_id.as_str()) {
				let state_after_obj = serde_json::json!({ "events": state_after });
				room.as_object_mut()
					.unwrap()
					.insert("state_after".to_owned(), state_after_obj.clone());
				room.as_object_mut()
					.unwrap()
					.insert("org.matrix.msc4222.state_after".to_owned(), state_after_obj);
			}
		}
	}

	if let Some(leave) = val.get_mut("rooms").and_then(|r| r.get_mut("leave")) {
		for (room_id, state_after) in left_state_after {
			if let Some(room) = leave.get_mut(room_id.as_str()) {
				let state_after_obj = serde_json::json!({ "events": state_after });
				room.as_object_mut()
					.unwrap()
					.insert("state_after".to_owned(), state_after_obj.clone());
				room.as_object_mut()
					.unwrap()
					.insert("org.matrix.msc4222.state_after".to_owned(), state_after_obj);
			}
		}
	}

	Ok(val)
}

#[tracing::instrument(name = "presence", level = "debug", skip_all)]
async fn process_presence_updates(
	services: &Services,
	last_sync_end_count: Option<u64>,
	syncing_user: &UserId,
) -> PresenceUpdates {
	services
		.presence
		.presence_since(last_sync_end_count.unwrap_or(0)) // send all presences on initial sync
		.filter(|(user_id, ..)| {
			services
				.rooms
				.state_cache
				.user_sees_user(syncing_user, user_id)
		})
		.filter_map(|(user_id, _, presence_bytes)| {
			services
				.presence
				.from_json_bytes_to_event(presence_bytes, user_id)
				.map_ok(move |event| (user_id, event))
				.ok()
		})
		.map(|(user_id, event)| (user_id.to_owned(), event.content))
		.collect()
		.await
}

/// Using the provided sync context and an iterator of user IDs in the
/// `timeline`, return a HashSet of user IDs whose membership events should be
/// sent to the client if lazy-loading is enabled.
#[allow(clippy::let_and_return)]
async fn prepare_lazily_loaded_members(
	services: &Services,
	sync_context: SyncContext<'_>,
	room_id: &RoomId,
	timeline_members: impl Iterator<Item = OwnedUserId>,
) -> Option<MemberSet> {
	let lazy_loading_context = &sync_context.lazy_loading_context(room_id);

	// reset lazy loading state on initial sync.
	// do this even if lazy loading is disabled so future lazy loads
	// will have the correct members.
	if sync_context.last_sync_end_count.is_none() {
		services
			.rooms
			.lazy_loading
			.reset(lazy_loading_context)
			.await;
	}

	// filter the input members through `retain_lazy_members`, which
	// contains the actual lazy loading logic.
	let lazily_loaded_members =
		OptionFuture::from(sync_context.lazy_loading_enabled().then(|| {
			services
				.rooms
				.lazy_loading
				.retain_lazy_members(timeline_members.collect(), lazy_loading_context)
		}))
		.await;

	lazily_loaded_members
}
