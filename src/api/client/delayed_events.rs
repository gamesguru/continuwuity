use axum::extract::State;
use conduwuit::{Err, Result};
use ruma::api::{
	AuthScheme, IncomingRequest, Metadata, client::device::get_devices,
	error::FromHttpRequestError,
};
use serde::Deserialize;

use crate::Ruma;

pub(crate) struct GetDelayedEventRequest;

impl IncomingRequest for GetDelayedEventRequest {
	type EndpointError = <get_devices::v3::Request as IncomingRequest>::EndpointError;
	type OutgoingResponse = <get_devices::v3::Request as IncomingRequest>::OutgoingResponse;

	const METADATA: Metadata = Metadata {
		method: http::Method::GET,
		rate_limited: true,
		authentication: AuthScheme::AccessToken,
		history: ruma::api::VersionHistory::new(
			&["/_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delay_id}"],
			&[],
			None,
			None,
		),
	};

	fn try_from_http_request<B, S>(
		_req: http::Request<B>,
		path_args: &[S],
	) -> std::result::Result<Self, FromHttpRequestError>
	where
		B: AsRef<[u8]>,
		S: AsRef<str>,
	{
		let (_delay_id,): (String,) =
			Deserialize::deserialize(serde::de::value::SeqDeserializer::<
				_,
				serde::de::value::Error,
			>::new(path_args.iter().map(AsRef::as_ref)))?;

		Ok(Self)
	}
}

pub(crate) struct GetAllDelayedEventsRequest;

impl IncomingRequest for GetAllDelayedEventsRequest {
	type EndpointError = <get_devices::v3::Request as IncomingRequest>::EndpointError;
	type OutgoingResponse = <get_devices::v3::Request as IncomingRequest>::OutgoingResponse;

	const METADATA: Metadata = Metadata {
		method: http::Method::GET,
		rate_limited: true,
		authentication: AuthScheme::AccessToken,
		history: ruma::api::VersionHistory::new(
			&["/_matrix/client/unstable/org.matrix.msc4140/delayed_events"],
			&[],
			None,
			None,
		),
	};

	fn try_from_http_request<B, S>(
		_req: http::Request<B>,
		path_args: &[S],
	) -> std::result::Result<Self, FromHttpRequestError>
	where
		B: AsRef<[u8]>,
		S: AsRef<str>,
	{
		let (): () = Deserialize::deserialize(serde::de::value::SeqDeserializer::<
			_,
			serde::de::value::Error,
		>::new(path_args.iter().map(AsRef::as_ref)))?;

		Ok(Self)
	}
}

// MSC4140: the delay_id itself is the bearer capability for these actions;
// per the MSC and its Complement coverage, restart/send/cancel are called
// without a user access token, so this route is intentionally unauthenticated.
pub(crate) async fn update_delayed_event_route(
	State(services): State<crate::State>,
	axum::extract::Path((delay_id, action)): axum::extract::Path<(String, String)>,
) -> Result<axum::Json<serde_json::Value>> {
	let action = match action.as_str() {
		| "restart" => service::rooms::delayed_events::UpdateAction::Restart,
		| "send" => service::rooms::delayed_events::UpdateAction::Send,
		| "cancel" => service::rooms::delayed_events::UpdateAction::Cancel,
		| _ => return Err!(Request(NotFound("Invalid action."))),
	};

	services
		.rooms
		.delayed_events
		.update_delayed_event(delay_id, action)
		.await?;

	Ok(axum::Json(serde_json::json!({})))
}

pub(crate) async fn update_delayed_event_without_action_route(
	axum::extract::Path(_delay_id): axum::extract::Path<String>,
) -> Result<axum::Json<serde_json::Value>> {
	Err!(Request(NotFound("Invalid action.")))
}

pub(crate) async fn get_delayed_event_route(
	State(services): State<crate::State>,
	axum::extract::Path(delay_id): axum::extract::Path<String>,
	body: Ruma<GetDelayedEventRequest>,
) -> Result<axum::Json<serde_json::Value>> {
	let sender_user = body.sender_user();

	let data = services
		.rooms
		.delayed_events
		.get_delayed_event(sender_user, delay_id)
		.await?;

	Ok(axum::Json(serde_json::json!({
		"delayed_event": data,
	})))
}

pub(crate) async fn get_all_delayed_events_route(
	State(services): State<crate::State>,
	body: Ruma<GetAllDelayedEventsRequest>,
) -> Result<axum::Json<serde_json::Value>> {
	let sender_user = body.sender_user();

	let mut data = services
		.rooms
		.delayed_events
		.get_user_scheduled_delayed_events(sender_user, None)
		.await;

	data.sort_by_key(|event| {
		event
			.running_since
			.to_system_time()
			.and_then(|ts| ts.checked_add(event.delay))
	});

	Ok(axum::Json(serde_json::json!({
		"delayed_events": data,
	})))
}
