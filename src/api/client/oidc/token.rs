use axum::extract::{Form, State};
use conduwuit::{Result, err};
use conduwuit_web::oidc::{AccessTokenForm, OidcResponse};
use oxide_auth::{
	code_grant::{
		accesstoken::{Error as AccessTokenError, Request as AccessTokenRequest},
		refresh::Error as RefreshTokenError,
	},
	endpoint::WebResponse,
};
use oxide_auth_async::code_grant;

/// # `POST /_matrix/client/unstable/org.matrix.msc2964/token`
///
/// Depending on `grant_type`, either deliver a new token to a device and store
/// it in the server's ring, or refresh the token.
pub(crate) async fn token(
	State(services): State<crate::State>,
	Form(oauth): Form<AccessTokenForm<'_>>,
) -> Result<OidcResponse> {
	tracing::trace!("processing OpenID token request {:#?}", oauth);

	let grant_type = AccessTokenRequest::grant_type(&oauth).map(|t| t.to_string());
	let token = match grant_type.as_deref() {
		| Some("authorization_code") => access_token(services, &oauth).await,
		| Some("refresh_token") => refresh_token(services, &oauth).await,
		| any => Err(err!(Request(Unknown("unimplemented grant type: {any:?}")))),
	}?;

	let mut response = OidcResponse::default();
	response
		.body_json(&token)
		.expect("append a json body in response");

	Ok(response)

	/*
	let endpoint = services.oidc.endpoint();
	tracing::debug!("submitting OpenID token request for grant type {grant_type:?}");

	match grant_type.as_deref() {
		| Some("authorization_code") => endpoint
			.access_token_flow()
			.execute(oauth)
			.map_err(|err| err!(Request(Unknown("token grant failed: {err:?}")))),
		| Some("refresh_token") => endpoint
			.refresh_flow()
			.execute(oauth)
			.map_err(|err| err!(Request(Unknown("token refresh failed: {err:?}")))),
		| other => Err(err!(Request(Unknown("unsupported grant type: {other:?}")))),
	}
	*/
}

async fn access_token(services: crate::State, oauth: &AccessTokenForm<'_>) -> Result<String> {
	let mut endpoint = services.oidc.endpoint.lock().await;
	match code_grant::access_token::access_token(&mut *endpoint, oauth).await {
		| Err(e) => match e {
			| AccessTokenError::Invalid(_) => Err(err!(Request(Unknown("invalid token")))),
			| AccessTokenError::Unauthorized(..) => {
				// TODO should probably return Unauthorized http status.
				Err(err!(Request(Unknown("unauthorized"))))
			},
			| AccessTokenError::Primitive(_e) => {
				// TODO: handle this
				//return StatusCode::SERVICE_UNAVAILABLE.into_response();

				Err(err!(Request(Unknown("server error"))))
			},
		},
		| Ok(token) => Ok(token.to_json()),
	}
}

async fn refresh_token(services: crate::State, oauth: &AccessTokenForm<'_>) -> Result<String> {
	let mut endpoint = services.oidc.endpoint.lock().await;
	match code_grant::refresh::refresh(&mut *endpoint, oauth).await {
		| Err(e) => match e {
			| RefreshTokenError::Invalid(_) => Err(err!(Request(Unknown("invalid token")))),
			| RefreshTokenError::Unauthorized(..) => {
				// TODO should probably return Unauthorized http status.
				Err(err!(Request(Unknown("unauthorized"))))
			},
			| RefreshTokenError::Primitive => {
				// TODO: handle this
				Err(err!(Request(Unknown("server error"))))
			},
		},
		| Ok(token) => Ok(token.to_json()),
	}
}
