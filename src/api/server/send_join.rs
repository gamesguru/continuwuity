#![allow(deprecated)]

use std::{
	borrow::Borrow,
	time::{Duration, Instant},
	vec,
};

use axum::extract::State;
use conduwuit::{
	Err, Event, Result, at, debug, err, info, trace,
	utils::stream::{BroadbandExt, IterStream, TryBroadbandExt},
	warn,
};
use conduwuit_service::Services;
use futures::{FutureExt, StreamExt, TryStreamExt};
use ruma::{
	CanonicalJsonValue, OwnedEventId, RoomId, ServerName, UserId,
	api::federation::membership::create_join_event,
	events::room::{join_rules::JoinRule, member::MembershipState},
};
use serde_json::value::{RawValue as RawJsonValue, to_raw_value};

use crate::Ruma;

/// helper method for /send_join v1 and v2
#[tracing::instrument(skip(services, pdu, omit_members), fields(room_id = room_id.as_str(), origin = origin.as_str()), level = "info")]
async fn create_join_event(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	pdu: &RawJsonValue,
	omit_members: bool,
) -> Result<create_join_event::v2::RoomState> {
	let (event_id, mut value, content, room_version_id, _sender, state_key) =
		super::utils::verify_send_membership(
			services,
			origin,
			room_id,
			pdu,
			MembershipState::Join,
		)
		.await?;

	// We need to return the state prior to joining, let's keep a reference to that
	// here
	let shortstatehash = services
		.rooms
		.state
		.get_room_shortstatehash(room_id)
		.await
		.map_err(|e| err!(Request(NotFound(error!("Room has no state: {e}")))))?;

	if let Some(authorising_user) = content.join_authorized_via_users_server {
		use ruma::RoomVersionId::*;

		if matches!(room_version_id, V1 | V2 | V3 | V4 | V5 | V6 | V7) {
			return Err!(Request(InvalidParam(
				"Room version {room_version_id} does not support restricted rooms but \
				 join_authorised_via_users_server ({authorising_user}) was found in the event."
			)));
		}

		if !services.globals.user_is_local(&authorising_user) {
			return Err!(Request(InvalidParam(
				"Cannot authorise membership event through {authorising_user} as they do not \
				 belong to this homeserver"
			)));
		}

		if !services
			.rooms
			.state_cache
			.is_joined(&authorising_user, room_id)
			.await
		{
			return Err!(Request(InvalidParam(
				"Authorising user {authorising_user} is not in the room you are trying to join, \
				 they cannot authorise your join."
			)));
		}

		if super::user_can_perform_restricted_join(
			services,
			&state_key,
			room_id,
			&room_version_id,
		)
		.await?
		.is_none()
		{
			return Err!(Request(UnableToAuthorizeJoin(
				"Joining user did not pass restricted room's rules."
			)));
		}

		services
			.server_keys
			.hash_and_sign_event(&mut value, &room_version_id)
			.map_err(|e| {
				err!(Request(InvalidParam(warn!("Failed to sign send_join event: {e}"))))
			})?;
	} else {
		// Guard for restricted/knock_restricted rooms: when the join event
		// lacks join_authorized_via_users_server the user must be invited or
		// already joined.  Without this, handle_and_send_incoming_pdu would soft-fail
		// the event but send_join would still return success.
		guard_restricted_join_without_auth(services, &state_key, room_id).await?;
	}

	let pdu_id = super::utils::handle_and_send_incoming_pdu(
		services,
		origin,
		room_id,
		&event_id,
		value.clone(),
		None,
	)
	.await?;

	trace!("Fetching current state IDs");
	let state_ids: Vec<OwnedEventId> = services
		.rooms
		.state_accessor
		.state_full_ids(shortstatehash)
		.map(at!(1))
		.collect()
		.await;

	trace!(%omit_members, "Constructing current state");
	let state = state_ids
		.iter()
		.try_stream()
		.broad_filter_map(|event_id| async move {
			if omit_members {
				if let Ok(e) = event_id.as_ref() {
					let pdu = services
						.rooms
						.timeline
						.get_pdu_in_room(Some(room_id), e)
						.await;
					if pdu.is_ok_and(|p| p.kind().to_cow_str() == "m.room.member") {
						trace!("omitting member event {e:?} from returned state");
						// skip members
						return None;
					}
				}
			}
			Some(event_id)
		})
		.broad_and_then(|event_id| services.rooms.timeline.get_pdu_json(event_id))
		.broad_and_then(|pdu| {
			services
				.sending
				.convert_to_outgoing_federation_event(pdu)
				.map(Ok)
		})
		.try_collect()
		.boxed()
		.await?;

	let starting_events = state_ids.iter().map(Borrow::borrow);
	trace!("Constructing auth chain");
	let auth_chain = services
		.rooms
		.auth_chain
		.event_ids_iter(room_id, starting_events)
		.broad_and_then(|event_id| async move {
			services.rooms.timeline.get_pdu_json(&event_id).await
		})
		.broad_and_then(|pdu| {
			services
				.sending
				.convert_to_outgoing_federation_event(pdu)
				.map(Ok)
		})
		.try_collect()
		.boxed()
		.await?;
	info!(fast_join = %omit_members, "Sending join event to other servers");
	services.sending.send_pdu_room(room_id, &pdu_id).await?;
	if !services.globals.server_is_ours(origin) {
		services
			.sending
			.wait_for_pdu_servers(vec![origin.to_owned()], &pdu_id, Duration::from_secs(15))
			.await?;
	}
	debug!("Finished sending join event");
	let servers_in_room: Option<Vec<_>> = if !omit_members {
		None
	} else {
		trace!("Fetching list of servers in room");
		let servers: Vec<String> = services
			.rooms
			.state_cache
			.room_servers(room_id)
			.map(|sn| sn.as_str().to_owned())
			.collect()
			.await;
		// If there's no servers, just add us
		let servers = if servers.is_empty() {
			warn!("Failed to find any servers, adding our own server name as a last resort");
			vec![services.globals.server_name().to_string()]
		} else {
			trace!("Found {} servers in room", servers.len());
			servers
		};
		Some(servers)
	};
	debug!("Returning send_join data");
	Ok(create_join_event::v2::RoomState {
		auth_chain,
		state,
		event: to_raw_value(&CanonicalJsonValue::Object(value)).ok(),
		members_omitted: omit_members,
		servers_in_room,
	})
}

/// # `PUT /_matrix/federation/v1/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub(crate) async fn create_join_event_v1_route(
	State(services): State<crate::State>,
	body: Ruma<create_join_event::v1::Request>,
) -> Result<create_join_event::v1::Response> {
	let now = Instant::now();
	let room_state = create_join_event(&services, body.origin(), &body.room_id, &body.pdu, false)
		.boxed()
		.await?;
	let transformed = create_join_event::v1::RoomState {
		auth_chain: room_state.auth_chain,
		state: room_state.state,
		event: room_state.event,
	};
	info!(
		"Finished sending a join for {} in {} in {:?}",
		body.origin(),
		&body.room_id,
		now.elapsed()
	);

	Ok(create_join_event::v1::Response { room_state: transformed })
}

/// # `PUT /_matrix/federation/v2/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub(crate) async fn create_join_event_v2_route(
	State(services): State<crate::State>,
	body: Ruma<create_join_event::v2::Request>,
) -> Result<create_join_event::v2::Response> {
	let now = Instant::now();
	let room_state =
		create_join_event(&services, body.origin(), &body.room_id, &body.pdu, body.omit_members)
			.boxed()
			.await?;
	info!(
		"Finished sending a join for {} in {} in {:?}",
		body.origin(),
		&body.room_id,
		now.elapsed()
	);

	Ok(create_join_event::v2::Response { room_state })
}

/// Reject a join to a restricted/knock_restricted room when the event lacks
/// `join_authorized_via_users_server` and the user is neither invited nor
/// already joined.
async fn guard_restricted_join_without_auth(
	services: &Services,
	joining_user: &UserId,
	room_id: &RoomId,
) -> Result<()> {
	let join_rules = services.rooms.state_accessor.get_join_rules(room_id).await;

	if !matches!(join_rules, JoinRule::Restricted(_) | JoinRule::KnockRestricted(_)) {
		return Ok(());
	}

	let is_invited = services
		.rooms
		.state_cache
		.is_invited(joining_user, room_id)
		.await;

	let is_joined = services
		.rooms
		.state_cache
		.is_joined(joining_user, room_id)
		.await;

	if !is_invited && !is_joined {
		return Err!(Request(Forbidden(
			"Restricted room requires join_authorized_via_users_server, an invite, or existing \
			 membership."
		)));
	}

	Ok(())
}
