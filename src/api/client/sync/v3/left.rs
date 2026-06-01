use conduwuit::{
	Event, PduCount, PduEvent, Result, debug_warn,
	matrix::{event::Matches, pdu::EventHash},
	trace,
	utils::{self, IterStream, ReadyExt as _, future::ReadyEqExt, stream::WidebandExt as _},
};
use futures::{StreamExt, future::join};
use ruma::{
	EventId, OwnedRoomId, RoomId,
	api::client::sync::sync_events::v3::{LeftRoom, RoomAccountData, State, Timeline},
	events::{AnySyncStateEvent, StateEventType, TimelineEventType},
	serde::Raw,
	uint,
};
use serde_json::value::RawValue;
use service::{Services, rooms::short::ShortStateHash};

use crate::client::{
	TimelinePdus, ignored_filter,
	sync::{
		load_timeline,
		v3::{
			DEFAULT_TIMELINE_LIMIT, SyncContext, prepare_lazily_loaded_members,
			state::build_state_initial,
		},
	},
};

#[tracing::instrument(
	name = "left",
	level = "debug",
	skip_all,
	fields(
		room_id = %room_id,
	),
)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn load_left_room(
	services: &Services,
	sync_context: SyncContext<'_>,
	ref room_id: OwnedRoomId,
	leave_membership_event: Option<PduEvent>,
) -> Result<Option<(LeftRoom, Vec<Raw<AnySyncStateEvent>>)>> {
	let SyncContext {
		syncing_user,
		last_sync_end_count,
		current_count,
		filter,
		..
	} = sync_context;

	// the global count as of the moment the user left the room
	let Some(left_count) = services
		.rooms
		.state_cache
		.get_left_count(room_id, syncing_user)
		.await
		.ok()
	else {
		// if we get here, the membership cache is incorrect, likely due to a state
		// reset
		debug_warn!("attempting to sync left room but no left count exists");
		return Ok(None);
	};

	// return early if:
	// - this is an incremental sync and we've already synced the leave, or
	// - this is an initial sync and the room filter doesn't include leaves
	if let Some(last_sync_end_count) = last_sync_end_count {
		if last_sync_end_count >= left_count {
			return Ok(None);
		}
	} else if !filter.room.include_leave {
		return Ok(None);
	}

	if let Some(ref leave_membership_event) = leave_membership_event {
		debug_assert_eq!(
			leave_membership_event.kind,
			TimelineEventType::RoomMember,
			"leave PDU should be m.room.member"
		);
	}

	let does_not_exist = services.rooms.metadata.exists(room_id).eq(&false).await;

	let (timeline, state_events, leave_shortstatehash) = match leave_membership_event.clone() {
		| Some(leave_membership_event) if does_not_exist => {
			/*
			we have none PDUs with left beef for this room, likely because it was a rejected invite to a room
			which nobody on this homeserver is in. `leave_pdu` is the remote-assisted outlier leave event for the room,
			which is all we can send to the client.

			if this is an initial sync, don't include this room at all to keep the client from asking for
			state that we don't have.
			*/

			if last_sync_end_count.is_none() {
				return Ok(None);
			}

			// the global count as of the moment the user left the room
			let Some(left_count) = services
				.rooms
				.state_cache
				.get_left_count(room_id, syncing_user)
				.await
				.ok()
			else {
				return Ok(None);
			};

			// return early if we've already synced the leave
			if last_sync_end_count
				.is_some_and(|last_sync_end_count| last_sync_end_count >= left_count)
			{
				return Ok(None);
			}

			trace!("syncing remote-assisted leave PDU");
			(
				TimelinePdus {
					pdus: vec![(PduCount::max(), leave_membership_event)].into(),
					limited: false,
				},
				Vec::new(),
				None,
			)
		},
		| Some(leave_membership_event) => {
			// we have this room in our DB, and can fetch the state and timeline from when
			// the user left.

			let leave_state_key = syncing_user;
			debug_assert_eq!(
				Some(leave_state_key.as_str()),
				leave_membership_event.state_key(),
				"leave PDU should be for the user requesting the sync"
			);

			// the shortstatehash of the state _immediately before_ the syncing user left
			// this room. the state represented here _does not_ include
			// `leave_membership_event`.
			let leave_shortstatehash = services
				.rooms
				.state_accessor
				.pdu_shortstatehash(&leave_membership_event.event_id)
				.await?;

			let prev_membership_event = services
				.rooms
				.state_accessor
				.state_get(
					leave_shortstatehash,
					&StateEventType::RoomMember,
					leave_state_key.as_str(),
				)
				.await?;

			let (timeline, state_events) = build_left_state_and_timeline(
				services,
				sync_context,
				room_id,
				leave_membership_event,
				leave_shortstatehash,
				prev_membership_event,
			)
			.await?;

			(timeline, state_events, Some(leave_shortstatehash))
		},
		| None => {
			/*
			no leave event was actually sent in this room, but we still need to pretend
			like the user left it. this is usually because the room was banned by a server admin.

			if this is an incremental sync, generate a fake leave event to make the room vanish from clients.
			otherwise we don't tell the client about this room at all.
			*/
			if last_sync_end_count.is_none() {
				return Ok(None);
			}

			trace!("syncing dummy leave event");
			(
				TimelinePdus::default(),
				vec![create_dummy_leave_event(services, sync_context, room_id)],
				None,
			)
		},
	};

	let state_after = if services.config.experimental_features.msc4222_enabled {
		if let Some(shortstatehash) = leave_shortstatehash {
			let lazily_loaded_members = prepare_lazily_loaded_members(
				services,
				sync_context,
				room_id,
				timeline.senders(),
			)
			.await;

			build_state_initial(
				services,
				syncing_user,
				room_id,
				shortstatehash,
				lazily_loaded_members.as_ref(),
			)
			.await?
			.into_iter()
			.map(Event::into_format)
			.collect()
		} else {
			Vec::new()
		}
	} else {
		Vec::new()
	};

	let mut raw_timeline_pdus = Vec::with_capacity(timeline.pdus.len());
	let mut in_timeline = false;

	let TimelinePdus { pdus, limited } = timeline;

	let mut stream = pdus
		.into_iter()
		.stream()
		// filter out ignored events from the timeline
		.wide_filter_map(|item| ignored_filter(services, item, syncing_user))
		.ready_filter(|(_, pdu): &(PduCount, PduEvent)| {
			let matches = (&filter.room.timeline).matches(pdu);
			tracing::info!(
				"Checking timeline event {} of type {}: filter.types = {:?}, matches = {}",
				pdu.event_id,
				pdu.event_type(),
				filter.room.timeline.types,
				matches
			);
			matches
		});

	while let Some((_, pdu)) = stream.next().await {
		if pdu.event_type() == &TimelineEventType::RoomMember
			&& pdu.state_key() == Some(syncing_user.as_str())
		{
			in_timeline = true;
		}
		raw_timeline_pdus.push(pdu.into_format());
	}

	let mut state_events_raw: Vec<_> = state_events
		.into_iter()
		.filter(|pdu: &PduEvent| (&filter.room.state).matches(pdu))
		.map(Event::into_format)
		.collect();

	if !in_timeline && last_sync_end_count.is_none_or(|c| c < left_count) {
		if let Some(leave_pdu) = leave_membership_event {
			if (&filter.room.state).matches(&leave_pdu) {
				state_events_raw.push(leave_pdu.into_format());
			}
		}
	}

	Ok(Some((
		LeftRoom {
			account_data: RoomAccountData { events: Vec::new() },
			timeline: Timeline {
				limited,
				prev_batch: Some(current_count.to_string()),
				events: raw_timeline_pdus,
			},
			state: State { events: state_events_raw },
		},
		state_after,
	)))
}

async fn build_left_state_and_timeline(
	services: &Services,
	sync_context: SyncContext<'_>,
	room_id: &RoomId,
	leave_membership_event: PduEvent,
	leave_shortstatehash: ShortStateHash,
	prev_membership_event: PduEvent,
) -> Result<(TimelinePdus, Vec<PduEvent>)> {
	let SyncContext { syncing_user, filter, .. } = sync_context;

	let timeline_start_count = services
		.rooms
		.timeline
		.get_pdu_count(&prev_membership_event.event_id)
		.await?;

	// end the timeline at the user's leave event
	let timeline_end_count = services
		.rooms
		.timeline
		.get_pdu_count(leave_membership_event.event_id())
		.await?;

	// limit the timeline using the same logic as for joined rooms
	let timeline_limit = filter
		.room
		.timeline
		.limit
		.and_then(|limit| limit.try_into().ok())
		.unwrap_or(DEFAULT_TIMELINE_LIMIT);

	let timeline = load_timeline(
		services,
		syncing_user,
		room_id,
		Some(timeline_start_count),
		Some(timeline_end_count),
		timeline_limit,
	)
	.await?;

	let timeline_start_shortstatehash = async {
		if let Some((_, pdu)) = timeline.pdus.front() {
			if let Ok(shortstatehash) = services
				.rooms
				.state_accessor
				.pdu_shortstatehash(&pdu.event_id)
				.await
			{
				return shortstatehash;
			}
		}

		// the timeline generally should not be empty (see the TODO further down),
		// but in case it is we use `leave_shortstatehash` as the state to
		// send
		leave_shortstatehash
	};

	let lazily_loaded_members =
		prepare_lazily_loaded_members(services, sync_context, room_id, timeline.senders());

	let (timeline_start_shortstatehash, lazily_loaded_members) =
		join(timeline_start_shortstatehash, lazily_loaded_members).await;

	// TODO: calculate incremental state for incremental syncs.
	// always calculating initial state _works_ but returns more data and does
	// more processing than strictly necessary.
	let mut state = build_state_initial(
		services,
		syncing_user,
		room_id,
		timeline_start_shortstatehash,
		lazily_loaded_members.as_ref(),
	)
	.await?;

	/*
	remove membership events for the syncing user from state.
	usually, `state` should include a `join` membership event and `timeline` should include a `leave` one.
	however, the matrix-js-sdk gets confused when this happens (see [1]) and doesn't process the room leave,
	so we have to filter out the membership from `state`.

	NOTE: we are sending more information than synapse does in this scenario, because we always
	calculate `state` for initial syncs, even when the sync being performed is incremental.
	however, the specification does not forbid sending extraneous events in `state`.

	TODO: there is an additional bug at play here. sometimes `load_joined_room` syncs the `leave` event
	before `load_left_room` does, which means the `timeline` we sync immediately after a leave is empty.
	this shouldn't happen -- `timeline` should always include the `leave` event. this is probably
	a race condition with the membership state cache.

	[1]: https://github.com/matrix-org/matrix-js-sdk/issues/5071
	*/

	// `state` should only ever include one membership event for the syncing user
	let membership_event_index = state.iter().position(|pdu| {
		*pdu.event_type() == TimelineEventType::RoomMember
			&& pdu.state_key() == Some(syncing_user.as_str())
	});

	if let Some(index) = membership_event_index {
		// the ordering of events in `state` does not matter
		state.swap_remove(index);
	}

	trace!(
		%timeline_start_count,
		%timeline_end_count,
		"syncing {} timeline events (limited = {}) and {} state events",
		timeline.pdus.len(),
		timeline.limited,
		state.len()
	);

	Ok((timeline, state))
}

fn create_dummy_leave_event(
	services: &Services,
	SyncContext { syncing_user, .. }: SyncContext<'_>,
	room_id: &RoomId,
) -> PduEvent {
	// TODO: because this event ID is random, it could cause caching issues with
	// clients. perhaps a database table could be created to hold these dummy
	// events, or they could be stored as outliers?
	PduEvent {
		event_id: EventId::new(services.globals.server_name()),
		sender: syncing_user.to_owned(),
		origin: None,
		origin_server_ts: utils::millis_since_unix_epoch()
			.try_into()
			.expect("Timestamp is valid js_int value"),
		kind: TimelineEventType::RoomMember,
		content: RawValue::from_string(r#"{"membership": "leave"}"#.to_owned()).unwrap(),
		state_key: Some(syncing_user.as_str().into()),
		unsigned: None,
		// The following keys are dropped on conversion
		room_id: Some(room_id.to_owned()),
		prev_events: vec![],
		depth: uint!(1),
		auth_events: vec![],
		redacts: None,
		hashes: EventHash { sha256: String::new() },
		signatures: None,
		rejected: false,
	}
}
