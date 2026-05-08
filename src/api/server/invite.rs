use axum::extract::State;
use axum_client_ip::ClientIp;
use base64::{Engine as _, engine::general_purpose};
use conduwuit::{
	Err, PduEvent, Result, err, error,
	matrix::{Event, event::gen_event_id},
	utils::hash::sha256,
	warn,
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedUserId, UserId,
	api::federation::membership::create_invite, events::invite_permission_config::FilterLevel,
	serde::JsonObject,
};

use crate::Ruma;

/// # `POST /_matrix/federation/v2/invite/{roomId}/{eventId}`
///
/// The recipient's server SHOULD return a 200 OK response to the sender's
/// server, but MUST NOT notify the recipient of the invite.
pub(crate) async fn create_invite_route(
	State(services): State<crate::State>,
	ClientIp(_client_ip): ClientIp,
	body: Ruma<create_invite::v2::Request>,
) -> Result<create_invite::v2::Response> {
	let mut signed_event: CanonicalJsonObject = serde_json::from_str(body.event.get())
		.map_err(|e| err!(Request(BadJson("Invalid invite event PDU: {e}"))))?;

	// Ensure the sender's server matches the origin server
	let sender_server = signed_event
		.get("sender")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|sender| UserId::parse(sender).ok())
		.map(|user_id| user_id.server_name().to_owned())
		.ok_or_else(|| err!(Request(InvalidParam("Invalid sender property"))))?;

	if sender_server != body.origin() {
		return Err!(Request(Forbidden("Sender's server does not match the origin server.",)));
	}

	// Ensure the target user belongs to this server
	let recipient_user: OwnedUserId = signed_event
		.get("state_key")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|state_key| UserId::parse(state_key).ok())
		.map(UserId::to_owned)
		.ok_or_else(|| err!(Request(InvalidParam("Invalid state_key property"))))?;

	if !services
		.globals
		.server_is_ours(recipient_user.server_name())
	{
		return Err!(Request(InvalidParam("User does not belong to this homeserver.")));
	}

	// Make sure we're not ACL'ed from their room.
	services
		.rooms
		.event_handler
		.acl_check(recipient_user.server_name(), &body.room_id)
		.await?;

	services
		.server_keys
		.hash_and_sign_event(&mut signed_event, &body.room_version)
		.map_err(|e| err!(Request(InvalidParam("Failed to sign event: {e}"))))?;

	// Generate event id
	let event_id = gen_event_id(&signed_event, &body.room_version)?;

	// Add event_id back
	signed_event.insert("event_id".to_owned(), CanonicalJsonValue::String(event_id.to_string()));

	let sender_user: &UserId = signed_event
		.get("sender")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|sender| UserId::parse(sender).ok())
		.ok_or_else(|| err!(Request(InvalidParam("Invalid sender property"))))?;

	if services.rooms.metadata.is_banned(&body.room_id).await
		&& !services.users.is_admin(&recipient_user).await
	{
		return Err!(Request(Forbidden("This room is banned on this homeserver.")));
	}

	if services.config.block_non_admin_invites && !services.users.is_admin(&recipient_user).await
	{
		return Err!(Request(Forbidden("This server does not allow room invites.")));
	}

	let recipient_filter_level = services
		.users
		.invite_filter_level(sender_user, &recipient_user)
		.await;

	match recipient_filter_level {
		| FilterLevel::Block => {
			return Err!(Request(InviteBlocked(
				"{recipient_user} has blocked invites from you."
			)));
		},
		| FilterLevel::Ignore => {
			return Ok(create_invite::v2::Response {
				event: services
					.sending
					.convert_to_outgoing_federation_event(signed_event)
					.await,
			});
		},
		| FilterLevel::Allow => {},
	}

	if let Err(e) = services
		.antispam
		.user_may_invite(sender_user.to_owned(), recipient_user.clone(), body.room_id.clone())
		.await
	{
		warn!("Antispam rejected invite: {e:?}");
		return Err!(Request(Forbidden("Invite rejected by antispam service.")));
	}

	let mut invite_state = body.invite_room_state.clone();

	let mut event: JsonObject = serde_json::from_str(body.event.get())
		.map_err(|e| err!(Request(BadJson("Invalid invite event PDU: {e}"))))?;

	event.insert("event_id".to_owned(), "$placeholder".into());

	let pdu: PduEvent = serde_json::from_value(event.into())
		.map_err(|e| err!(Request(BadJson("Invalid invite event PDU: {e}"))))?;

	invite_state.push(pdu.to_format());

	// If we are active in the room, the remote server will notify us about the
	// join/invite through /send. If we are not in the room, we need to manually
	// record the invited state for client /sync through update_membership(), and
	// send the invite PDU to the relevant appservices.
	if !services
		.rooms
		.state_cache
		.server_in_room(services.globals.server_name(), &body.room_id)
		.await
	{
		services
			.rooms
			.state_cache
			.mark_as_invited(
				&recipient_user,
				&body.room_id,
				sender_user,
				Some(invite_state),
				body.via.clone(),
			)
			.await?;

		services
			.rooms
			.state_cache
			.update_joined_count(&body.room_id)
			.await;

		for appservice in services.appservice.read().await.values() {
			if appservice.is_user_match(&recipient_user) {
				let request = ruma::api::appservice::event::push_events::v1::Request {
					events: vec![pdu.to_format()],
					txn_id: general_purpose::URL_SAFE_NO_PAD
						.encode(sha256::hash(pdu.event_id.as_bytes()))
						.into(),
					ephemeral: Vec::new(),
					to_device: Vec::new(),
				};
				services
					.sending
					.send_appservice_request(appservice.registration.clone(), request)
					.await
					.map_err(|e| {
						error!(
							"failed to notify appservice {} about incoming invite: {e}",
							appservice.registration.id
						);
						err!(BadServerResponse(
							"Failed to notify appservice about incoming invite."
						))
					})?;
			}
		}
	}

	Ok(create_invite::v2::Response {
		event: services
			.sending
			.convert_to_outgoing_federation_event(signed_event)
			.await,
	})
}
