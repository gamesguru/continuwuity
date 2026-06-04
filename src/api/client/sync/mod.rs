mod v3;
mod v5;

use std::collections::VecDeque;

use conduwuit::{
	Event, PduCount, Result, debug_warn, err,
	matrix::pdu::PduEvent,
	ref_at, trace,
	utils::stream::{BroadbandExt, ReadyExt, TryIgnore, WidebandExt},
};
use conduwuit_service::Services;
use futures::StreamExt;
use ruma::{
	OwnedUserId, RoomId, UserId,
	events::TimelineEventType::{
		self, Beacon, CallInvite, PollStart, RoomEncrypted, RoomMessage, Sticker,
	},
};

pub(crate) use self::{v3::sync_events_route, v5::sync_events_v5_route};

pub(crate) const DEFAULT_BUMP_TYPES: &[TimelineEventType; 6] =
	&[CallInvite, PollStart, Beacon, RoomEncrypted, RoomMessage, Sticker];

#[derive(Default)]
pub(crate) struct TimelinePdus {
	pub pdus: VecDeque<(PduCount, PduEvent)>,
	pub limited: bool,
}

impl TimelinePdus {
	fn senders(&self) -> impl Iterator<Item = OwnedUserId> {
		self.pdus
			.iter()
			.map(ref_at!(1))
			.map(Event::sender)
			.map(Into::into)
	}
}

/// Load up to `limit` PDUs in the range (starting_count, ending_count].
async fn load_timeline(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	starting_count: Option<PduCount>,
	ending_count: Option<PduCount>,
	limit: usize,
) -> Result<TimelinePdus> {
	let mut pdu_stream = match starting_count {
		| Some(starting_count) => {
			let last_timeline_count = services
				.rooms
				.timeline
				.last_timeline_count(room_id)
				.await
				.map_err(|err| {
					err!(Database(warn!("Failed to fetch end of room timeline: {}", err)))
				})?;

			if last_timeline_count <= starting_count {
				// no messages have been sent in this room since `starting_count`
				return Ok(TimelinePdus::default());
			}

			// for incremental sync, stream from the DB all PDUs which were sent after
			// `starting_count` but before `ending_count`, including `ending_count` but
			// not `starting_count`. this code is pretty similar to the initial sync
			// branch, they're separate to allow for future optimization
			services
				.rooms
				.timeline
				.pdus_rev(room_id, ending_count.map(|count| count.saturating_add(1)))
				.ignore_err()
				.ready_take_while(move |&(pducount, _)| pducount > starting_count)
				.map(move |mut pdu| {
					pdu.1.set_unsigned(Some(sender_user));
					pdu
				})
				.wide_then(move |mut pdu| async move {
					if let Err(e) = services
						.rooms
						.pdu_metadata
						.add_bundled_aggregations_to_pdu(sender_user, &mut pdu.1)
						.await
					{
						debug_warn!("Failed to add bundled aggregations: {e}");
					}
					pdu
				})
				.boxed()
		},
		| None => {
			// For initial sync, stream from the DB all PDUs before and including
			// `ending_count` in reverse order
			services
				.rooms
				.timeline
				.pdus_rev(room_id, ending_count.map(|count| count.saturating_add(1)))
				.ignore_err()
				.map(move |mut pdu| {
					pdu.1.set_unsigned(Some(sender_user));
					pdu
				})
				.wide_then(move |mut pdu| async move {
					if let Err(e) = services
						.rooms
						.pdu_metadata
						.add_bundled_aggregations_to_pdu(sender_user, &mut pdu.1)
						.await
					{
						debug_warn!("Failed to add bundled aggregations: {e}");
					}
					pdu
				})
				.boxed()
		},
	};

	let mut pdus = VecDeque::with_capacity(limit);
	let mut limited = false;

	while let Some(item) = pdu_stream.next().await {
		// Check for a topological gap BEFORE this event
		let mut gap_found = false;
		if starting_count.is_some() {
			for prev_id in item.1.prev_events() {
				if services
					.rooms
					.timeline
					.get_pdu_count(prev_id)
					.await
					.is_err()
				{
					gap_found = true;
					break;
				}
			}
		}

		pdus.push_front(item);

		if gap_found {
			limited = true;
			break;
		}

		if pdus.len() >= limit {
			limited = pdu_stream.next().await.is_some();
			break;
		}
	}

	trace!(
		"syncing {:?} timeline pdus from {:?} to {:?} (limited = {:?})",
		pdus.len(),
		starting_count,
		ending_count,
		limited,
	);

	Ok(TimelinePdus { pdus, limited })
}

async fn share_encrypted_room(
	services: &Services,
	sender_user: &UserId,
	user_id: &UserId,
	ignore_room: Option<&RoomId>,
) -> bool {
	services
		.rooms
		.state_cache
		.get_shared_rooms(sender_user, user_id)
		.ready_filter(|&room_id| Some(room_id) != ignore_room)
		.map(ToOwned::to_owned)
		.broad_any(|other_room_id| async move {
			services
				.rooms
				.state_accessor
				.is_encrypted_room(&other_room_id)
				.await
		})
		.await
}
