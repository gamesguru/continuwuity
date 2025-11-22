use axum::extract::State;
use conduwuit::{Result, err};
use conduwuit_oidc::{OidcRequest, OidcResponse, flows::AccessTokenForm};
use oxide_auth::code_grant::accesstoken::Request as AccessTokenRequest;
use oxide_auth_async::endpoint::{access_token::AccessTokenFlow, refresh::RefreshFlow};

/// # `POST /_matrix/client/unstable/org.matrix.msc2964/token`
///
/// Depending on `grant_type`, either deliver a new token to a device and store
/// it in the server's ring, or refresh the token.
pub(crate) async fn token(
	State(services): State<crate::State>,
	request: OidcRequest,
) -> Result<OidcResponse> {
	let oauth: AccessTokenForm<'_> = request
		.clone()
		.try_into()
		.map_err(|_| err!(Request(Unknown("form parsing error"))))?;
	let grant_type = AccessTokenRequest::grant_type(&oauth).map(|t| t.to_string());

	match grant_type.as_deref() {
		| Some("authorization_code") => access_token(services, request).await,
		| Some("refresh_token") => refresh_token(services, request).await,
		| any => Err(err!(Request(Unknown("unimplemented grant type: {any:?}")))),
	}
}

async fn access_token(services: crate::State, request: OidcRequest) -> Result<OidcResponse> {
	tracing::trace!("submitting token flow with {request:#?}");
	let mut endpoint = services.oidc.endpoint.lock().await;
	let mut flow = AccessTokenFlow::prepare(&mut *endpoint)
		.map_err(|e| err!(Request(Unknown("flow preparation: {:?}", e))))?;

	flow.execute(request)
		.await
		.map_err(|e| err!(Request(Unknown("flow execution: {:?}", e))))
}

async fn refresh_token(services: crate::State, request: OidcRequest) -> Result<OidcResponse> {
	tracing::trace!("submitting refresh flow with {request:#?}");
	let mut endpoint = services.oidc.endpoint.lock().await;
	let mut flow = RefreshFlow::prepare(&mut *endpoint)
		.map_err(|e| err!(Request(Unknown("flow preparation: {:?}", e))))?;

	flow.execute(request)
		.await
		.map_err(|e| err!(Request(Unknown("flow execution: {:?}", e))))
}
