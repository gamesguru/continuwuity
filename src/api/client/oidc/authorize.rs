use std::borrow::Cow;

use axum::extract::{Query, State};
use conduwuit::{Result, debug, err};
use conduwuit_web::oidc::{
	AuthorizationQuery,
	OidcRequest,
	OidcResponse,
	oidc_consent_form,
	oidc_login_form,
};
use oxide_auth::{code_grant::authorization::Error as AuthorizationError, endpoint::WebResponse};
use oxide_auth_async::code_grant;
use percent_encoding::percent_decode_str;
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
	Query(query): Query<AuthorizationQuery>,
	oauth: OidcRequest,
) -> Result<OidcResponse> {
	// TODO add solicitor page.
	tracing::trace!("processing OAuth request: {query:#?}");
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

	// Redirect to the login page if no token or token not known.
	let hostname = services.config.server_name.host();
	let Some(token) = oauth.authorization_header() else {
		return Ok(oidc_login_form(hostname, &query));
	};

	tracing::debug!("submitting OIDC authorisation for token : {token:#?}");

	// Add the user_id to the query.
	let (user_id, device_id) = services.users.find_from_token(token).await?;
	let mut query_with_user_id = query.clone();
	query_with_user_id.username = Some(user_id.localpart().to_string());

	/*
	services
		.oidc
		.endpoint()
		.with_solicitor(oidc_consent_form(hostname, &query_with_user_id))
		.authorization_flow()
		.execute(oauth)
		.map_err(|err| err!("authorization failed: {err:?}"))
	*/

	let mut endpoint = services.oidc.endpoint.lock().await;
	let pending =
		match code_grant::authorization::authorization_code(&mut *endpoint, &query_with_user_id)
			.await
		{
			| Err(e) => match e {
				| AuthorizationError::Ignore => {
					debug!(?user_id, ?device_id, "authorization request ignored");
					return Err(err!(Request(Unknown("authorization request ignored"))));
				},
				| AuthorizationError::Redirect(url) => {
					debug!(?user_id, ?device_id, "authorization request was redirected");
					let mut response = OidcResponse::default();
					response
						.redirect(url.into())
						.map_err(|e| err!(Request(Unknown("{}", e))))?;
					return Ok(response);
				},
				| AuthorizationError::PrimitiveError => {
					debug!(?user_id, ?device_id, "there was a primitive error while authorizing");
					return Err(err!(Request(Unknown("primitive error"))));
				},
			},
			| Ok(pending) => pending,
		};

	match query.owner_allowance.as_deref() {
		| Some("false") => match pending.deny() {
			| Err(AuthorizationError::Redirect(url)) => {
				debug!(?user_id, ?device_id, "authorization request was redirected");
				let mut response = OidcResponse::default();
				response
					.redirect(url.into())
					.map_err(|e| err!(Request(Unknown("{}", e))))?;

				Ok(response)
			},
			| _ => {
				debug!(?user_id, ?device_id, "there was a primitive error while denying auth");

				Err(err!(Request(Unknown("primitive error"))))
			},
		},
		| _ => {
			let user_id = Cow::from(user_id.to_string());
			match pending.authorize(&mut *endpoint, user_id.clone()).await {
				| Err(_) => {
					debug!(
						?user_id,
						?device_id,
						"there was a primitive error while allowing auth"
					);

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
		},
	}
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
	Query(query): Query<AuthorizationQuery>,
	oauth: OidcRequest,
) -> Result<OidcResponse> {
	tracing::debug!("processing owner's consent");
	tracing::trace!("owner's consent request: {:#?}", query);

	authorize(State(services), Query(query), oauth).await

	/*
	services
		.oidc
		.endpoint()
		.with_solicitor(FnSolicitor(move |_: &mut _, _: Solicitation<'_>| match allow.clone() {
			| None => OwnerConsent::Denied,
			| Some(user_id) => OwnerConsent::Authorized(user_id),
		}))
		.authorization_flow()
		.execute(oauth)
		.map_err(|err| err!(Request(Unknown("consent request failed: {err:?}"))))
	*/
}
