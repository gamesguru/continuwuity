#![allow(deprecated)]

use axum::extract::State;
use conduwuit::{Result, err};
use conduwuit_service::Services;
use futures::FutureExt;
use ruma::{
	RoomId, ServerName, api::federation::membership::create_leave_event,
	events::room::member::MembershipState,
};
use serde_json::value::RawValue as RawJsonValue;

use crate::Ruma;

/// # `PUT /_matrix/federation/v1/send_leave/{roomId}/{eventId}`
///
/// Submits a signed leave event.
pub(crate) async fn create_leave_event_v1_route(
	State(services): State<crate::State>,
	body: Ruma<create_leave_event::v1::Request>,
) -> Result<create_leave_event::v1::Response> {
	create_leave_event(&services, body.origin(), &body.room_id, &body.pdu).await?;

	Ok(create_leave_event::v1::Response::new())
}

/// # `PUT /_matrix/federation/v2/send_leave/{roomId}/{eventId}`
///
/// Submits a signed leave event.
pub(crate) async fn create_leave_event_v2_route(
	State(services): State<crate::State>,
	body: Ruma<create_leave_event::v2::Request>,
) -> Result<create_leave_event::v2::Response> {
	create_leave_event(&services, body.origin(), &body.room_id, &body.pdu).await?;

	Ok(create_leave_event::v2::Response::new())
}

async fn create_leave_event(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	pdu: &RawJsonValue,
) -> Result {
	let (event_id, value, _, _, _origin_sender, _state_key) =
		super::utils::verify_send_membership(
			services,
			origin,
			room_id,
			pdu,
			MembershipState::Leave,
		)
		.await?;

	let mutex_lock = services
		.rooms
		.event_handler
		.mutex_federation
		.lock(room_id)
		.await;

	let pdu_id = services
		.rooms
		.event_handler
		.handle_incoming_pdu(origin, room_id, &event_id, value, true, None)
		.boxed()
		.await?
		.ok_or_else(|| err!(Request(InvalidParam("Could not accept as timeline event."))))?;

	drop(mutex_lock);

	services
		.sending
		.send_pdu_room(room_id, &pdu_id)
		.boxed()
		.await
}
