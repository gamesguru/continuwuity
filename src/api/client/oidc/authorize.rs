use axum::extract::{Query, State};
use axum_extra::extract::SignedCookieJar;
use conduwuit::{Result, err};
use conduwuit_oidc::{AuthorizationQuery, OidcRequest, OidcResponse, oidc_login_form};
use oxide_auth_async::endpoint::authorization::AuthorizationFlow;
use percent_encoding::percent_decode_str;
use ruma::UserId;
use service::oidc::{SCOPE_PREFIX_API, SCOPE_PREFIX_DEVICE};

/// # `GET /_matrix/client/unstable/org.matrix.msc2964/authorize`
///
/// Authenticate a user and device, and solicit the user's consent.
///
/// Redirects to the login page if no token or token not belonging to any user.
/// [super::login::oidc_login] takes it up at the same point, so it's either
/// the client has a token, or the user does user password. Then the user gets
/// access to stage two, [authorize_consent].
pub(crate) async fn authorize(
	State(services): State<crate::State>,
	jar: SignedCookieJar,
	Query(query): Query<AuthorizationQuery>,
	mut request: OidcRequest,
) -> Result<OidcResponse> {
	// TODO add solicitor page.
	// Enforce MSC2964's restrictions on OAuth2 flow.
	let Ok(scope) = percent_decode_str(&query.scope).decode_utf8() else {
		return Err(err!(Request(Unknown("the scope could not be percent-decoded"))));
	};
	if !scope.contains(&format!("{SCOPE_PREFIX_API}*")) {
		return Err(err!(Request(Unknown("the scope does not include the client API"))));
	}
	if !scope.contains(SCOPE_PREFIX_DEVICE) {
		return Err(err!(Request(Unknown("the scope does not include a device ID"))));
	}
	if query.code_challenge_method != "S256" {
		return Err(err!(Request(Unknown("unsupported code challenge method"))));
	}

	let Some(user_id) = jar.get("user_id").map(|cookie| cookie.value().to_owned()) else {
		let hostname = services.config.server_name.host();
		return Ok(oidc_login_form(hostname, &query));
	};
	let user_id = UserId::parse(&user_id)?;
	tracing::debug!("submitting OIDC authorisation for user_id {user_id}");

	// Add the username field to the request so it's used as beneficiary in the
	// consent form.
	request
		.add_username_to_query(user_id.localpart())
		.map_err(|e| err!(Request(Unknown("cannot add username to query: {e:?}"))))?;

	tracing::trace!("submitting authorization flow with {request:#?}");
	let mut endpoint = services.oidc.endpoint.lock().await;
	let mut flow = AuthorizationFlow::prepare(&mut *endpoint)
		.map_err(|e| err!(Request(Unknown("flow preparation: {:?}", e))))?;

	flow.execute(request)
		.await
		.map_err(|e| err!(Request(Unknown("flow execution: {:?}", e))))
}

/// # `POST /_matrix/client/unstable/org.matrix.msc2964/authorize?allow=[Option<String>]`
///
/// Authorize the device based on the owner's consent. If the owner allows
/// it to access their data, the client may request a token at the
/// [super::token::token] endpoint.
///
/// On the owner's consent, if their specific device is unregistered it will be
/// registered in their device list (not to be confused with the OIDC client
/// registration).
pub(crate) async fn authorize_consent(
	State(services): State<crate::State>,
	jar: SignedCookieJar,
	request: OidcRequest,
) -> Result<OidcResponse> {
	// The request's query, either GET or POST fields.
	let query: AuthorizationQuery = request
		.clone()
		.try_into()
		.map_err(|_| err!(Request(Unknown("cannot parse request"))))?;

	tracing::debug!("processing owner's consent");
	tracing::trace!("owner's consent query: {:#?}", query);

	// Pass the consent form fields as GET query fields.
	authorize(State(services), jar, Query(query), request).await
}
