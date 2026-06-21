use axum::extract::State;
use axum_client_ip::ClientIp;
use conduwuit::{Err, Result, err, utils};
use ruma::api::client::message::send_message_event;

use crate::Ruma;

/// # `PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}`
///
/// Send a message event into the room.
///
/// - Is a NOOP if the txn id was already used before and returns the same event
///   id again
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is
///   allowed
pub(crate) async fn send_message_event_route(
	State(services): State<crate::State>,
	ClientIp(client_ip): ClientIp,
	body: Ruma<send_message_event::v3::Request>,
) -> Result<send_message_event::v3::Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device.as_deref();
	let appservice_info = body.appservice_info.as_ref();
	if services.users.is_suspended(sender_user).await? {
		return Err!(Request(UserSuspended("You cannot perform this action while suspended.")));
	}

	services
		.users
		.update_device_last_seen(sender_user, body.sender_device.as_deref(), client_ip)
		.await;

	let state_lock = services.rooms.state.mutex.lock(&body.room_id).await;

	// Check if this is a new transaction id
	if let Ok(response) = services
		.transactions
		.get_client_txn(sender_user, sender_device, &body.txn_id)
		.await
	{
		// The client might have sent a txnid of the /sendToDevice endpoint
		// This txnid has no response associated with it
		if response.is_empty() {
			return Err!(Request(InvalidParam(
				"Tried to use txn id already used for an incompatible endpoint."
			)));
		}

		return Ok(send_message_event::v3::Response {
			event_id: utils::string_from_bytes(&response)
				.map(TryInto::try_into)
				.map_err(|e| err!(Database("Invalid event_id in txnid data: {e:?}")))??,
		});
	}

	let event_id = Box::pin(services.rooms.timeline.send_message_event_helper(
		sender_user,
		&body.room_id,
		&state_lock,
		&body.event_type,
		&body.body.body,
		Some(&body.txn_id),
		if appservice_info.is_some() {
			body.timestamp
		} else {
			None
		},
		None,
	))
	.await?;

	services.transactions.add_client_txnid(
		sender_user,
		sender_device,
		&body.txn_id,
		event_id.as_bytes(),
	);

	drop(state_lock);

	Ok(send_message_event::v3::Response { event_id })
}
