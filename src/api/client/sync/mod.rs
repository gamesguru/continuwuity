mod v3;
mod v5;

use std::collections::VecDeque;

use conduwuit::{
	Event, PduCount, Result, debug_warn, err, info,
	matrix::pdu::PduEvent,
	ref_at,
	utils::stream::{BroadbandExt, ReadyExt, TryIgnore, WidebandExt},
	warn,
};
use conduwuit_service::Services;
use futures::{StreamExt, TryStreamExt};
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
				info!(
					target: "timeline_debug",
					"load_timeline early return for {}: last_timeline_count={:?} <= \
					 starting_count={:?}",
					room_id, last_timeline_count, starting_count
				);
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
				.inspect_err(|e| warn!("sync timeline pdus_rev error for {room_id}: {e}"))
				.ignore_err()
				.inspect(move |(pducount, _)| {
					info!(
						target: "timeline_debug",
						"sync filter check for {}: pducount={:?}, starting_count={:?}, \
						 passes={:?}",
						room_id,
						pducount,
						starting_count,
						*pducount > starting_count
					);
				})
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
				.inspect_err(|e| warn!("sync initial timeline pdus_rev error for {room_id}: {e}"))
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

	let mut pdus = pdu_stream
		.by_ref()
		.take(limit)
		.ready_fold(VecDeque::with_capacity(limit), |mut pdus, item| {
			pdus.push_front(item);
			pdus
		})
		.await;

	let mut limited = false;

	if starting_count.is_some() {
		// Traverse newest to oldest to find the first topological gap backwards
		for (i, (_, pdu)) in pdus.iter().enumerate().rev() {
			let mut gap_found = false;
			for prev_id in pdu.prev_events() {
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

			if gap_found {
				// We found a gap BEFORE this PDU. Keep this PDU, but drop anything before.
				info!(
					"Topological gap in timeline for {} before PDU {}. Truncating.",
					room_id,
					pdu.event_id()
				);
				pdus.drain(0..i);
				limited = true;
				break;
			}
		}
	}

	// The timeline is limited if there are still more PDUs in the stream
	if !limited {
		limited = pdu_stream.next().await.is_some();
	}

	if pdus.is_empty() && starting_count.is_some() {
		info!(
			target: "timeline_debug",
			"sync: 0 timeline pdus for {} from {:?} to {:?} (limited = {:?})",
			room_id, starting_count, ending_count, limited,
		);
	} else {
		info!(
			target: "timeline_debug",
			"sync: {:?} timeline pdus for {} from {:?} to {:?} (limited = {:?})",
			pdus.len(),
			room_id,
			starting_count,
			ending_count,
			limited,
		);
	}

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
