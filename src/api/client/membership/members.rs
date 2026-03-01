use axum::extract::State;
use conduwuit::{
	Err, Event, Pdu, PduCount, Result, at, err,
	utils::stream::{BroadbandExt, ReadyExt},
};
use futures::{FutureExt, StreamExt};
use ruma::{
	OwnedUserId,
	api::client::membership::{
		get_member_events::{self, v3::MembershipEventFilter},
		joined_members::{self, v3::RoomMember},
	},
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};

use crate::Ruma;

/// # `POST /_matrix/client/r0/rooms/{roomId}/members`
///
/// Lists all joined users in a room (TODO: at a specific point in time, with a
/// specific membership).
///
/// - Only works if the user is currently joined
pub(crate) async fn get_member_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_member_events::v3::Request>,
) -> Result<get_member_events::v3::Response> {
	let sender_user = body.sender_user();
	let membership = body.membership.as_ref();
	let not_membership = body.not_membership.as_ref();

	if !services
		.rooms
		.state_accessor
		.user_can_see_state_events(sender_user, &body.room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view this room.")));
	}

	if let Some(at) = body.at.as_deref() {
		let pdu_count: PduCount = at
			.parse()
			.map_err(|_| err!(Request(InvalidParam("Invalid 'at' token."))))?;

		let mut pdus_rev = services
			.rooms
			.timeline
			.pdus_rev(&body.room_id, Some(pdu_count))
			.boxed();

		let Some(Ok((_, pdu))) = pdus_rev.next().await else {
			return Err!(Request(NotFound("Point in time not found in timeline.")));
		};

		let shortstatehash = services
			.rooms
			.state_accessor
			.pdu_shortstatehash(pdu.event_id())
			.await?;

		// Collect into Vec<Pdu> to avoid HRTB/opaque-type conflicts with
		// room_state_full's impl Event stream used later in this function.
		let all_pdus: Vec<Pdu> = services
			.rooms
			.state_accessor
			.state_full_pdus(shortstatehash)
			.map(Event::into_pdu)
			.collect()
			.await;

		let chunk = all_pdus
			.into_iter()
			.filter(|pdu| *pdu.kind() == ruma::events::TimelineEventType::RoomMember)
			.filter_map(|pdu| membership_filter(pdu, membership, not_membership))
			.map(Event::into_format)
			.collect();

		return Ok(get_member_events::v3::Response { chunk });
	}

	Ok(get_member_events::v3::Response {
		chunk: services
			.rooms
			.state_accessor
			.room_state_full(&body.room_id)
			.ready_filter_map(Result::ok)
			.ready_filter(|((ty, _), _)| *ty == StateEventType::RoomMember)
			.map(at!(1))
			.ready_filter_map(|pdu| membership_filter(pdu, membership, not_membership))
			.map(Event::into_format)
			.collect()
			.boxed()
			.await,
	})
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/joined_members`
///
/// Lists all members of a room.
///
/// - The sender user must be in the room
/// - TODO: An appservice just needs a puppet joined
pub(crate) async fn joined_members_route(
	State(services): State<crate::State>,
	body: Ruma<joined_members::v3::Request>,
) -> Result<joined_members::v3::Response> {
	if !services
		.rooms
		.state_accessor
		.user_can_see_state_events(body.sender_user(), &body.room_id)
		.await
	{
		return Err!(Request(Forbidden("You don't have permission to view this room.")));
	}

	let shortstatehash = services
		.rooms
		.state
		.get_room_shortstatehash(&body.room_id)
		.await?;

	Ok(joined_members::v3::Response {
		joined: services
			.rooms
			.state_accessor
			.state_keys_with_ids::<ruma::OwnedEventId>(
				shortstatehash,
				&StateEventType::RoomMember,
			)
			.broad_filter_map(|(state_key, event_id)| async move {
				let user_id: OwnedUserId = state_key.as_str().try_into().ok()?;
				let pdu = services.rooms.timeline.get_pdu(&event_id).await.ok()?;
				let content: RoomMemberEventContent = pdu.get_content().ok()?;
				if content.membership != MembershipState::Join {
					return None;
				}

				Some((user_id, RoomMember {
					display_name: content.displayname,
					avatar_url: content.avatar_url,
				}))
			})
			.collect()
			.await,
	})
}

fn membership_filter<Pdu: Event>(
	pdu: Pdu,
	for_membership: Option<&MembershipEventFilter>,
	not_membership: Option<&MembershipEventFilter>,
) -> Option<impl Event> {
	let membership_state_filter = match for_membership {
		| Some(MembershipEventFilter::Ban) => MembershipState::Ban,
		| Some(MembershipEventFilter::Invite) => MembershipState::Invite,
		| Some(MembershipEventFilter::Knock) => MembershipState::Knock,
		| Some(MembershipEventFilter::Leave) => MembershipState::Leave,
		| Some(_) | None => MembershipState::Join,
	};

	let not_membership_state_filter = match not_membership {
		| Some(MembershipEventFilter::Ban) => MembershipState::Ban,
		| Some(MembershipEventFilter::Invite) => MembershipState::Invite,
		| Some(MembershipEventFilter::Join) => MembershipState::Join,
		| Some(MembershipEventFilter::Knock) => MembershipState::Knock,
		| Some(_) | None => MembershipState::Leave,
	};

	let evt_membership = pdu.get_content::<RoomMemberEventContent>().ok()?.membership;

	if for_membership.is_some() && not_membership.is_some() {
		if membership_state_filter != evt_membership
			|| not_membership_state_filter == evt_membership
		{
			None
		} else {
			Some(pdu)
		}
	} else if for_membership.is_some() && not_membership.is_none() {
		if membership_state_filter != evt_membership {
			None
		} else {
			Some(pdu)
		}
	} else if not_membership.is_some() && for_membership.is_none() {
		if not_membership_state_filter == evt_membership {
			None
		} else {
			Some(pdu)
		}
	} else {
		Some(pdu)
	}
}
