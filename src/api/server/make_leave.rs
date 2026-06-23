use axum::extract::State;
use conduwuit::{Result, matrix::pdu::PduBuilder, utils};
use ruma::{
	api::federation::membership::prepare_leave_event,
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use serde_json::value::to_raw_value;

use crate::Ruma;

/// # `GET /_matrix/federation/v1/make_leave/{roomId}/{eventId}`
///
/// Creates a leave template.
pub(crate) async fn create_leave_event_template_route(
	State(services): State<crate::State>,
	body: Ruma<prepare_leave_event::v1::Request>,
) -> Result<prepare_leave_event::v1::Response> {
	super::utils::verify_make_membership(&services, body.origin(), &body.room_id, &body.user_id)
		.await?;

	let room_version_id = services.rooms.state.get_room_version(&body.room_id).await?;
	let state_lock = services.rooms.state.mutex.lock(&body.room_id).await;

	let (pdu, _) = services
		.rooms
		.timeline
		.create_event(
			PduBuilder::state(
				body.user_id.to_string(),
				&RoomMemberEventContent::new(MembershipState::Leave),
			),
			&body.user_id,
			Some(&body.room_id),
			&state_lock,
		)
		.await?;

	drop(state_lock);
	let mut pdu_json = utils::to_canonical_object(&pdu)
		.expect("Barebones PDU should be convertible to canonical JSON");
	pdu_json.remove("event_id");

	Ok(prepare_leave_event::v1::Response {
		room_version: Some(room_version_id),
		event: to_raw_value(&pdu_json).expect("CanonicalJson can be serialized to JSON"),
	})
}
