use axum::{extract::State, response::IntoResponse};
use axum_extra::extract::{SignedCookieJar, cookie::Cookie};
use conduwuit::{Result, err, utils::hash::verify_password};
use conduwuit_oidc::{OidcRequest, flows::LoginQuery};
use oxide_auth_async::endpoint::authorization::AuthorizationFlow;
use ruma::user_id::UserId;

//#[axum::debug_handler]
/// # `POST /_matrix/client/unstable/org.matrix.msc2964/login`
///
/// Display a login UI to the user and return an authorization code on success.
/// We presume that the OAuth2 query parameters are provided in the form.
/// With the code, the client may then access stage two,
/// [super::authorize::authorize_consent].
pub(crate) async fn oidc_login(
	State(services): State<crate::State>,
	jar: SignedCookieJar,
	request: OidcRequest,
) -> Result<impl IntoResponse> {
	let query: LoginQuery = request
		.clone()
		.try_into()
		.map_err(|err| err!(Request(InvalidParam("Cannot process login form. {err:?}"))))?;
	// Only accept local usernames. Mostly to simplify things at first.
	let user_id =
		UserId::parse_with_server_name(query.username.clone(), &services.config.server_name)
			.map_err(|e| err!(Request(InvalidUsername("Username is invalid: {e}"))))?;

	if !services.users.exists(&user_id).await {
		return Err(err!(Request(Unknown("unknown username"))));
	}
	let valid_hash = services.users.password_hash(&user_id).await?;

	if valid_hash.is_empty() {
		return Err(err!(Request(UserDeactivated("the user's hash was not found"))));
	}
	if verify_password(&query.password, &valid_hash).is_err() {
		return Err(err!(Request(InvalidParam("password does not match"))));
	}

	// TODO check if user disabled, etc. See /src/api/client/session.rs

	let jar = jar.add(default_cookie("user_id", user_id.to_string()));
	tracing::info!("logging in {user_id:?}");

	tracing::trace!("submitting login flow with {request:#?}");
	let mut endpoint = services.oidc.endpoint.lock().await;
	let mut flow = AuthorizationFlow::prepare(&mut *endpoint)
		.map_err(|e| err!(Request(Unknown("flow preparation: {:?}", e))))?;

	let oidc_response = flow
		.execute(request)
		.await
		.map_err(|e| err!(Request(Unknown("flow execution: {:?}", e))))?;

	// Build up a response with the cookie jar embedded, so it's committed by the
	// client.
	Ok((jar, oidc_response.into_response()).into_response())
}

fn default_cookie<'a>(key: &str, user_id: String) -> Cookie<'a> {
	Cookie::build((key.to_string(), user_id))
		.path("/")
		.http_only(true)
		// TODO make this a global setting ?
		// TODO import the cookie crate and cookie::time::Duration.
		//.max_age(Duration::new(24 * 60 * 60, 0).into())
		.secure(true)
		.build()
}
