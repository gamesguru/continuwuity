use askama::Template;
use axum::{async_trait, http::StatusCode};
use oxide_auth::{
	endpoint::{OwnerConsent, Solicitation, WebRequest},
	frontends::simple::request::Body,
};
use oxide_auth_async::endpoint::OwnerSolicitor;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::{AuthorizationQuery, OidcRequest, OidcResponse, consent::ConsentPageTemplate};

pub struct AsyncSolicitor {
	pub hostname: String,
}

#[async_trait]
impl OwnerSolicitor<OidcRequest> for AsyncSolicitor {
	async fn check_consent(
		&mut self,
		request: &mut OidcRequest,
		_solicitation: Solicitation<'_>,
	) -> OwnerConsent<<OidcRequest as WebRequest>::Response> {
		let query: AuthorizationQuery = request
			.try_into()
			.expect("OidcRequest should be a valid AuthorizationQuery");
		if query.deny.is_some() {
			return OwnerConsent::Denied;
		}

		match query.allow {
			| Some(username) => OwnerConsent::Authorized(username),
			| None => OwnerConsent::InProgress(oidc_consent_form(&self.hostname, &query)),
		}
	}
}

/// A web consent solicitor form for the OIDC authentication flow.
///
/// Asks the resource owner for their consent to let a client access their data
/// on this server.
#[must_use]
pub(crate) fn oidc_consent_form(hostname: &str, query: &AuthorizationQuery) -> OidcResponse {
	// The target request route.
	let route = "/_matrix/client/unstable/org.matrix.msc2964/authorize";
	let nonce = &rand::random::<u64>().to_string();
	let beneficiary = &encode(
		query
			.username
			.as_ref()
			.expect("the username as a beneficiary to present to the owner"),
	);
	let template = ConsentPageTemplate {
		nonce,
		hostname,
		route,
		beneficiary,
		client_id: &encode(query.client_id.as_str()),
		client_name: query.client_name.as_deref(),
		client_secret: query.client_secret.as_deref(),
		redirect_uri: query.redirect_uri.as_str(),
		scope: query.scope.as_str(),
		state: query.state.as_str(),
		code_challenge: query.code_challenge.as_str(),
		code_challenge_method: query.code_challenge_method.as_str(),
		response_type: query.response_type.as_str(),
		response_mode: query.response_mode.as_deref().unwrap_or("fragment"),
	};
	let body = Some(Body::Text(template.render().expect("consent page render")));

	OidcResponse {
		status: StatusCode::OK,
		location: None,
		www_authenticate: None,
		body,
		nonce: Some(nonce.clone()),
	}
}

pub(crate) fn encode(text: &str) -> String {
	utf8_percent_encode(text, NON_ALPHANUMERIC).to_string()
}
