use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum_client_ip::ClientIp;
use conduwuit::{Err, Result, err, utils};
use ruma::api::client::delayed_events;
use service::rooms::delayed_events::ScheduledDelayedEvent;

use crate::Ruma;

pub(crate) async fn send_delayed_event_route(
	State(services): State<crate::State>,
	ClientIp(client_ip): ClientIp,
	body: Ruma<delayed_events::send_delayed_event::unstable::Request>,
) -> Result<delayed_events::send_delayed_event::unstable::Response> {
	let Ruma {
		body:
			delayed_events::send_delayed_event::unstable::Request {
				txn_id,
				room_id,
				event_type,
				content,
				delay,
				state_key,
				..
			},
		identity,
		..
	} = body;

	let sender_user = identity
		.sender_user()
		.expect("There should be a user for this endpoint.");
	let sender_device = identity.sender_device();

	if services.users.is_suspended(sender_user).await? {
		return Err!(Request(UserSuspended("You cannot perform this action while suspended.")));
	}

	services
		.users
		.update_device_last_seen(sender_user, sender_device, client_ip)
		.await;

	// We use the room state lock to get the required synchronization guarantees
	// on transaction IDs
	let state_lock = services.rooms.state.mutex.lock(room_id.as_str()).await;

	// Check if this is a new transaction id
	if let Ok(response) = services
		.transactions
		.get_client_txn(sender_user, sender_device, &txn_id)
		.await
	{
		// The client might have sent a txnid of the /sendToDevice endpoint
		// This txnid has no response associated with it
		if response.is_empty() {
			return Err!(Request(InvalidParam(
				"Tried to use txn id already used for an incompatible endpoint."
			)));
		}

		let delay_id = utils::string_from_bytes(&response)
			.map(TryInto::try_into)
			.map_err(|e| err!(Database("Invalid event_id in txnid data: {e:?}")))??;

		return Ok(delayed_events::send_delayed_event::unstable::Response::new(delay_id));
	}

	if delay > Duration::from_hours(24 * 365 * 1000) {
		return Err!(Request(Forbidden("Requested delay duration is too long.")));
	}

	let running_since = SystemTime::now();

	let scheduled_event = ScheduledDelayedEvent {
		event_type,
		state_key,
		content,
		user_id: sender_user.to_owned(),
		room_id,
		running_since,
		delay,
	};

	let delay_id = services
		.rooms
		.delayed_events
		.queue_delayed_event(scheduled_event)
		.await?;

	services.transactions.add_client_txnid(
		sender_user,
		sender_device,
		&txn_id,
		delay_id.as_bytes(),
	);

	drop(state_lock);

	Ok(delayed_events::send_delayed_event::unstable::Response::new(delay_id))
}

pub(crate) async fn update_delayed_event_event_route(
	State(services): State<crate::State>,
	body: Ruma<delayed_events::update_delayed_event::unstable_v2::Request>,
) -> Result<delayed_events::update_delayed_event::unstable_v2::Response> {
	let Ruma {
		body: delayed_events::update_delayed_event::unstable_v2::Request { delay_id, action, .. },
		..
	} = body;

	services
		.rooms
		.delayed_events
		.update_delayed_event(delay_id, action)
		.await?;

	Ok(delayed_events::update_delayed_event::unstable_v2::Response::new())
}

pub(crate) async fn get_delayed_event_route(
	State(services): State<crate::State>,
	body: Ruma<delayed_events::get_delayed_event::unstable::Request>,
) -> Result<delayed_events::get_delayed_event::unstable::Response> {
	let Ruma {
		body: delayed_events::get_delayed_event::unstable::Request { delay_id, .. },
		..
	} = body;

	let data = services
		.rooms
		.delayed_events
		.get_delayed_event(delay_id)
		.await?;

	Ok(data.into())
}

pub(crate) async fn get_all_delayed_events_route(
	State(services): State<crate::State>,
	body: Ruma<delayed_events::get_all_delayed_events::unstable::Request>,
) -> Result<delayed_events::get_all_delayed_events::unstable::Response> {
	let sender_user = body
		.identity
		.sender_user()
		.expect("This endpoint requires a user.");

	let mut data = services
		.rooms
		.delayed_events
		.get_user_scheduled_delayed_events(sender_user, None)
		.await;

	data.sort_by_key(|event| event.running_since.checked_add(event.delay));

	Ok(delayed_events::get_all_delayed_events::unstable::Response::new(data))
}
