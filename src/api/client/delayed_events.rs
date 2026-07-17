use axum::extract::State;
use conduwuit::{Err, Result};

use crate::Ruma;

pub(crate) async fn update_delayed_event_event_route(
	State(services): State<crate::State>,
	axum::extract::Path(delay_id): axum::extract::Path<String>,
	uri: http::Uri,
	body: Ruma<ruma::api::client::session::logout::v3::Request>,
) -> Result<axum::Json<serde_json::Value>> {
	let sender_user = body.sender_user();

	let action = if uri.path().ends_with("/restart") {
		service::rooms::delayed_events::UpdateAction::Restart
	} else if uri.path().ends_with("/send") {
		service::rooms::delayed_events::UpdateAction::Send
	} else if uri.path().ends_with("/cancel") {
		service::rooms::delayed_events::UpdateAction::Cancel
	} else {
		return Err!(Request(InvalidParam("Invalid action.")));
	};

	services
		.rooms
		.delayed_events
		.update_delayed_event(sender_user, delay_id, action)
		.await?;

	Ok(axum::Json(serde_json::json!({})))
}

pub(crate) async fn get_delayed_event_route(
	State(services): State<crate::State>,
	axum::extract::Path(delay_id): axum::extract::Path<String>,
	body: Ruma<ruma::api::client::device::get_devices::v3::Request>,
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
	body: Ruma<ruma::api::client::device::get_devices::v3::Request>,
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
