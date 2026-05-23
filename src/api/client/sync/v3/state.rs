use std::collections::HashSet;

use conduwuit::{
	Result, at,
	matrix::{Event, pdu::PduEvent},
	utils::{
		IterStream, ReadyExt, TryFutureExtExt,
		stream::{BroadbandExt, TryIgnore},
	},
};
use conduwuit_service::{
	Services,
	rooms::{lazy_loading::MemberSet, short::ShortStateHash},
};
use futures::StreamExt;
use ruma::{OwnedEventId, UserId, events::StateEventType};
use tracing::trace;

use crate::client::TimelinePdus;

/// Calculate the state events to include in an initial sync response.
///
/// If lazy-loading is enabled (`lazily_loaded_members` is Some), the returned
/// Vec will include the membership events of exclusively the members in
/// `lazily_loaded_members`.
#[tracing::instrument(
	name = "initial",
	level = "trace",
	skip_all,
	fields(current_shortstatehash)
)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn build_state_initial(
	services: &Services,
	sender_user: &UserId,
	timeline_start_shortstatehash: ShortStateHash,
	timeline_end_shortstatehash: ShortStateHash,
	use_state_after: bool,
	lazily_loaded_members: Option<&MemberSet>,
) -> Result<Vec<PduEvent>> {
	// load the keys and event IDs of the state events at the start of the timeline
	let (_shortstatekeys, event_ids): (Vec<_>, Vec<_>) = services
		.rooms
		.state_accessor
		.state_full_ids(if use_state_after {
			timeline_end_shortstatehash
		} else {
			timeline_start_shortstatehash
		})
		.unzip()
		.await;

	// fetch the PDU contents of the state events, and filter out any that the user
	// shouldn't see
	let state_events: Vec<PduEvent> = event_ids
		.into_iter()
		.stream()
		.broad_filter_map(|event_id: OwnedEventId| async move {
			services.rooms.timeline.get_pdu(&event_id).await.ok()
		})
		.broad_filter_map(|pdu: PduEvent| async move {
			let room_id = pdu.room_id()?;
			services
				.rooms
				.state_accessor
				.user_can_see_event(sender_user, room_id, pdu.event_id())
				.await
				.then_some(pdu)
		})
		.collect()
		.await;

	// if lazy-loading is enabled, filter the state events based on the members that
	// were requested by the caller
	if let Some(lazily_loaded_members) = lazily_loaded_members {
		let state_events = state_events
			.into_iter()
			.filter(|pdu| {
				if pdu.kind == StateEventType::RoomMember.into() {
					let state_key = pdu.state_key().expect("member event has state key");
					let user_id = UserId::parse(state_key).expect("invalid user ID");
					lazily_loaded_members.contains::<UserId>(user_id)
				} else {
					true
				}
			})
			.collect();

		return Ok(state_events);
	}

	Ok(state_events)
}

/// Calculate the state events to include in an incremental sync response.
///
/// If lazy-loading is enabled (`lazily_loaded_members` is Some), the returned
/// Vec will include the membership events of all the members in
/// `lazily_loaded_members`.
#[tracing::instrument(name = "incremental", level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn build_state_incremental<'a>(
	services: &Services,
	sender_user: &'a UserId,
	last_sync_end_shortstatehash: ShortStateHash,
	timeline_start_shortstatehash: ShortStateHash,
	timeline_end_shortstatehash: ShortStateHash,
	timeline: &TimelinePdus,
	use_state_after: bool,
	lazily_loaded_members: Option<&'a MemberSet>,
) -> Result<Vec<PduEvent>> {
	let mut state_event_ids: HashSet<OwnedEventId> = HashSet::new();

	trace!(
		%use_state_after,
		%last_sync_end_shortstatehash,
		%timeline_start_shortstatehash,
		%timeline_end_shortstatehash,
		"computing state for incremental sync"
	);

	// Fetch lazy-loaded membership events if lazy-loading is enabled
	if let Some(lazily_loaded_members) = lazily_loaded_members
		&& !lazily_loaded_members.is_empty()
	{
		trace!("including lazy membership events for members: {:?}", lazily_loaded_members);

		services
			.rooms
			.short
			.multi_get_eventid_from_short::<'_, OwnedEventId, _>(
				lazily_loaded_members
					.iter()
					.stream()
					.broad_filter_map(|user_id| async move {
						if user_id == sender_user {
							return None;
						}

						services
							.rooms
							.state_accessor
							.state_get_shortid(
								timeline_start_shortstatehash,
								&StateEventType::RoomMember,
								user_id.as_str(),
							)
							.ok()
							.await
					}),
			)
			.ignore_err()
			.ready_for_each(|event_id| {
				state_event_ids.insert(event_id);
			})
			.await;
	}

	// Fetch the state events added since the last sync.
	services
		.rooms
		.short
		.multi_get_eventid_from_short::<'_, OwnedEventId, _>(
			services
				.rooms
				.state_accessor
				.state_added((last_sync_end_shortstatehash, timeline_end_shortstatehash))
				.await?
				.into_iter()
				.stream()
				.map(at!(1)),
		)
		.ignore_err()
		.ready_for_each(|event_id| {
			state_event_ids.insert(event_id);
		})
		.await;

	if !use_state_after {
		// If state_after isn't enabled, filter out state events which also exist
		// in the timeline. If splits exist in the DAG, this may not be exactly the same
		// thing as the state diff ending at the start of the timeline, but Synapse
		// also does this and it's technically more useful behavior anyway.
		// See: https://github.com/element-hq/synapse/issues/16941

		for (_, pdu) in &timeline.pdus {
			state_event_ids.remove(pdu.event_id());
		}
	}

	// Finally, fetch the PDU contents and collect them into a vec
	let state_diff_pdus = state_event_ids
		.into_iter()
		.stream()
		.broad_filter_map(|event_id| async move {
			services
				.rooms
				.timeline
				.get_non_outlier_pdu(&event_id)
				.await
				.ok()
		})
		.collect()
		.await;

	Ok(state_diff_pdus)
}
