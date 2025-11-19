use std::borrow::Cow;

use axum::extract::State;
use conduwuit::{Result, debug, err, utils::hash::verify_password};
use conduwuit_web::oidc::{
	// TODO add consent form.
	AuthorizationQuery,
	LoginError,
	LoginQuery,
	OidcRequest,
	OidcResponse,
	oidc_consent_form,
};
use oxide_auth::{code_grant::authorization::Error as AuthorizationError, endpoint::WebResponse};
use oxide_auth_async::code_grant;
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
	request: OidcRequest,
) -> Result<OidcResponse> {
	let query: LoginQuery = request.clone().try_into().map_err(|LoginError(err)| {
		err!(Request(InvalidParam("Cannot process login form. {err}")))
	})?;
	tracing::trace!("processing login query {:#?}", query.clone());
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
	tracing::info!("logging in {user_id:?}");

	/*
	let issuer = services.oidc.endpoint.get_mut().issuer;

	issuer
		.with_solicitor(oidc_consent_form(hostname, &query.into()))
		.authorization_flow()
		.execute(request)
		.map_err(|err| err!(Request(Unknown("authorisation failed: {err:?}"))))
	*/

	let query: AuthorizationQuery = query.into();

	let mut endpoint = services.oidc.endpoint.lock().await;
	let pending =
		match code_grant::authorization::authorization_code(&mut *endpoint, &query).await {
			| Err(e) => match e {
				| AuthorizationError::Ignore => {
					debug!(?user_id, "authorization request ignored");
					return Err(err!(Request(Unknown("authorization request ignored"))));
				},
				| AuthorizationError::Redirect(url) => {
					debug!(?user_id, "authorization request was redirected");
					let mut response = OidcResponse::default();
					response
						.redirect(url.into())
						.map_err(|e| err!(Request(Unknown("{}", e))))?;
					return Ok(response);
				},
				| AuthorizationError::PrimitiveError => {
					debug!(?user_id, "there was a primitive error while authorizing");
					return Err(err!(Request(Unknown("primitive error"))));
				},
			},
			| Ok(pending) => pending,
		};

	let user_id = Cow::from(user_id.to_string());
	match pending.authorize(&mut *endpoint, user_id.clone()).await {
		| Err(_) => {
			debug!(?user_id, "there was a primitive error while allowing auth");
			Err(err!(Request(Unknown("primitive error"))))
		},
		| Ok(url) => {
			let mut web_response = OidcResponse::default();
			web_response
				.redirect(url)
				.map_err(|e| err!(Request(Unknown("{}", e))))?;
			Ok(web_response)
		},
	}
}
