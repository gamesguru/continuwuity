use axum::{
	extract::{Path, State},
	Json,
};
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use conduwuit::{Err, Error, Result, err};
use ruma::{OwnedUserId, api::client::error::ErrorKind};
use serde_json::json;

pub(crate) async fn get_user_admin_route(
	State(services): State<crate::State>,
	Path(user_id): Path<OwnedUserId>,
	bearer: Option<TypedHeader<Authorization<Bearer>>>,
) -> Result<Json<serde_json::Value>> {
	// Authentication
	let token = bearer
		.map(|TypedHeader(Authorization(bearer))| bearer.token().to_owned())
		.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

	let (sender_user, _device_id) = services.users.find_from_token(&token).await.map_err(|_| {
		Error::BadRequest(ErrorKind::UnknownToken { soft_logout: false }, "Unknown access token.")
	})?;

    // Check if the sender is allowed to see this.
    // Usually, only the user themselves or an admin should be able to see this?
    // The Rageshake/EleWeb client usually calls this for the logged-in user.
    if sender_user != user_id && !services.users.is_admin(&sender_user).await {
        return Err!(Request(Forbidden("You are not allowed to view this user's admin status.")));
    }

	let is_admin = services.users.is_admin(&user_id).await;

	Ok(Json(json!({
		"admin": is_admin,
        "x_continuwuity_compat": "This endpoint is provided for Synapse compatibility."
	})))
}
