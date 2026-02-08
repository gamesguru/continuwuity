use axum::extract::State;
use conduwuit::Result;
use ruma::api::client::push::get_notifications;

use crate::Ruma;

/// # `GET /_matrix/client/v3/notifications`
///
/// Get notifications for the user.
///
/// Currently just returns an empty response.
pub(crate) async fn get_notifications_route(
	State(_services): State<crate::State>,
	_body: Ruma<get_notifications::v3::Request>,
) -> Result<get_notifications::v3::Response> {
	Ok(get_notifications::v3::Response {
		next_token: None,
		notifications: vec![],
	})
}
