use std::collections::HashSet;

use conduwuit::{
	Result, at,
	matrix::{Event, pdu::PduEvent},
	utils::{
		BoolExt, IterStream, ReadyExt,
		stream::{BroadbandExt, TryIgnore},
	},
};
use conduwuit_service::{
	Services,
	rooms::{lazy_loading::MemberSet, short::ShortStateHash},
};
use futures::StreamExt;
use ruma::{OwnedEventId, RoomId, UserId, events::StateEventType};
use tracing::{debug, trace};

use crate::client::TimelinePdus;

/// Helper to get a membership event for a user in a room with fallbacks.
async fn get_member_pdu_best_effort(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	shortstatehash: ShortStateHash,
) -> Option<PduEvent> {
	// Try provided shortstatehash (spec-compliant for the start of the timeline)
	if let Ok(pdu) = services
		.rooms
		.state_accessor
		.state_get(shortstatehash, &StateEventType::RoomMember, user_id.as_str())
		.await
	{
		return Some(pdu);
	}

	// Try current room state (fallback if incomplete state at timeline start)
	if let Ok(pdu) = services
		.rooms
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await
	{
		debug!(%user_id, %room_id, "Lazy load: Found member in current room state (fallback)");
		return Some(pdu);
	}

	// Try left_state if they are no longer in the room
	if let Ok(Some(pdu)) = services
		.rooms
		.state_cache
		.left_state(user_id, room_id)
		.await
	{
		debug!(%user_id, %room_id, "Lazy load: Found member in left_state cache (fallback)");
		return Some(pdu);
	}

	None
}

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
pub(super) async fn build_state_initial(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	state_hash: ShortStateHash,
	timeline: &TimelinePdus,
	use_state_after: bool,
	lazily_loaded_members: Option<&MemberSet>,
) -> Result<Vec<PduEvent>> {
	let event_ids_in_timeline: HashSet<_> =
		timeline.pdus.iter().map(|pdu| &pdu.1.event_id).collect();

	// load the keys and event IDs of the state events at the target state hash
	let (shortstatekeys, event_ids): (Vec<_>, Vec<_>) = services
		.rooms
		.state_accessor
		.state_full_ids(state_hash)
		.ready_filter(|(_, event_id)| {
			use_state_after || !event_ids_in_timeline.contains(event_id)
		})
		.unzip()
		.await;

	trace!("performing initial sync of {} state events", event_ids.len());

	let filtered_event_ids: Vec<_> = services
		.rooms
		.short
		// look up the full state keys
		.multi_get_statekey_from_short(shortstatekeys.into_iter().stream())
		.zip(event_ids.into_iter().stream())
		.ready_filter_map(|item| Some((item.0.ok()?, item.1)))
		.ready_filter_map(|((event_type, state_key), event_id)| {
			if let Some(lazily_loaded_members) = lazily_loaded_members {
				/*
				if lazy loading is enabled, filter out membership events which aren't for a user
				included in `lazily_loaded_members` or for the user requesting the sync.
				*/
				let event_is_redundant = event_type == StateEventType::RoomMember
					&& state_key.as_str() != sender_user.as_str()
					&& !state_key.as_str().try_into().is_ok_and(|user_id: &UserId| lazily_loaded_members.contains(user_id));

				event_is_redundant.or_some(event_id)
			} else {
				Some(event_id)
			}
		})
		.collect()
		.await;

	let mut state_events = filtered_event_ids
		.into_iter()
		.stream()
		.broad_filter_map(|event_id: OwnedEventId| async move {
			services.rooms.timeline.get_pdu(&event_id).await.ok()
		})
		.collect::<Vec<_>>()
		.await;

	// if lazy loading is enabled, ensure ALL requested members are present.
	// missing members is a common federated room issue w/ partial state history
	if let Some(lazily_loaded_members) = lazily_loaded_members {
		let missing_members: Vec<_> = lazily_loaded_members
			.iter()
			.stream()
			.ready_filter(|user_id| {
				let already_present = state_events.iter().any(|pdu| {
					pdu.kind == StateEventType::RoomMember.into()
						&& pdu.state_key.as_deref() == Some(user_id.as_str())
				});
				!already_present
			})
			.broad_filter_map(|user_id| async move {
				get_member_pdu_best_effort(services, room_id, user_id, state_hash).await
			})
			.collect()
			.await;

		if !missing_members.is_empty() {
			trace!(
				"Added {} missing lazily loaded members to initial sync",
				missing_members.len()
			);
			state_events.extend(missing_members);
		}
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
	room_id: &RoomId,
	last_sync_end_shortstatehash: ShortStateHash,
	state_hash: ShortStateHash,
	timeline: &TimelinePdus,
	use_state_after: bool,
	lazily_loaded_members: Option<&'a MemberSet>,
) -> Result<Vec<PduEvent>> {
	let mut state_event_ids: HashSet<OwnedEventId> = HashSet::new();

	trace!(
		%use_state_after,
		%last_sync_end_shortstatehash,
		%state_hash,
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
								state_hash,
								&StateEventType::RoomMember,
								user_id.as_str(),
							)
							.await
							.ok()
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
				.state_added((last_sync_end_shortstatehash, state_hash))
				.await?
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
	let mut state_diff_pdus = state_event_ids
		.stream()
		.broad_filter_map(|event_id| async move {
			services
				.rooms
				.timeline
				.get_non_outlier_pdu(&event_id)
				.await
				.ok()
		})
		.collect::<Vec<_>>()
		.await;

	trace!(?state_diff_pdus, "collected state PDUs for incremental sync");

	// Ensure requested lazy-loaded members are present in the response
	if let Some(lazily_loaded_members) = lazily_loaded_members {
		let missing_members: Vec<_> = lazily_loaded_members
			.iter()
			.stream()
			.ready_filter(|user_id| {
				let already_present = state_diff_pdus.iter().any(|pdu| {
					pdu.kind == StateEventType::RoomMember.into()
						&& pdu.state_key.as_deref() == Some(user_id.as_str())
				});
				!already_present
			})
			.broad_filter_map(|user_id| async move {
				get_member_pdu_best_effort(services, room_id, user_id, state_hash).await
			})
			.collect()
			.await;

		if !missing_members.is_empty() {
			trace!(
				"Added {} missing lazily loaded members to incremental sync",
				missing_members.len()
			);
			state_diff_pdus.extend(missing_members);
		}
	}

	Ok(state_diff_pdus)
}
