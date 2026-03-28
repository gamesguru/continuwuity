use std::str::FromStr;

use askama::Template;
use axum::http::StatusCode;
use conduwuit_build_metadata::{GIT_REMOTE_COMMIT_URL, GIT_REMOTE_WEB_URL, version_tag};
use oxide_auth::{endpoint::QueryParameter, frontends::simple::request::Body};
use url::Url;

use super::AuthorizationQuery;
use crate::{OidcRequest, OidcResponse};

/// The parameters for the OIDC login page template.
#[derive(Template)]
#[template(path = "login.html.j2")]
pub(crate) struct LoginPageTemplate<'a> {
	nonce: &'a str,
	hostname: &'a str,
	route: &'a str,
	client_id: &'a str,
	client_name: Option<&'a str>,
	client_secret: Option<&'a str>,
	redirect_uri: &'a str,
	scope: &'a str,
	state: &'a str,
	code_challenge: &'a str,
	code_challenge_method: &'a str,
	response_type: &'a str,
	response_mode: &'a str,
}

/// POST parameters an [OidcDevice] needs to login (to eventually get
/// authorization).
#[derive(serde::Deserialize, Debug, Clone)]
pub struct LoginQuery {
	pub username: String,
	pub password: String,
	pub client_id: String,
	pub client_name: Option<String>,
	pub client_secret: Option<String>,
	pub redirect_uri: Url,
	pub scope: String,
	pub state: String,
	pub code_challenge: String,
	pub code_challenge_method: String,
	pub response_type: String,
	pub response_mode: String,
	pub allow: Option<String>,
	pub deny: Option<String>,
}

#[derive(Debug)]
pub enum LoginError {
	NoQuery,
	MissingField(String),
	InvalidField(String),
}

impl TryFrom<OidcRequest> for LoginQuery {
	type Error = LoginError;

	fn try_from(value: OidcRequest) -> Result<Self, LoginError> {
		let body = value.body().ok_or(LoginError::NoQuery)?;

		let getopt = |key| body.unique_value(key).map(|s| s.to_string());
		let get = |key| {
			body.unique_value(key)
				.ok_or_else(|| LoginError::MissingField(key.into()))
				.map(|s| s.to_string())
		};

		Ok(Self {
			username: get("username")?,
			password: get("password")?,
			client_id: get("client_id")?,
			client_name: getopt("client_name"),
			client_secret: getopt("client_secret"),
			redirect_uri: Url::from_str(&get("redirect_uri")?)
				.map_err(|_| LoginError::InvalidField("redirect_uri".into()))?,
			scope: get("scope")?,
			state: get("state")?,
			code_challenge: get("code_challenge")?,
			code_challenge_method: get("code_challenge_method")?,
			response_type: get("response_type")?,
			// response_mode is not strictly needed : it must be the literal "fragment"
			// when over https. It's required by the Matrix spec but Fractal doesn't provide it.
			response_mode: getopt("response_mode").unwrap_or_else(|| "fragment".to_owned()),
			allow: getopt("allow"),
			deny: getopt("deny"),
		})
	}
}

/// A web login form for the OIDC authentication flow.
///
/// The returned `OidcResponse` handles CSP headers to allow that
/// form.
#[must_use]
pub fn oidc_login_form(hostname: &str, query: &AuthorizationQuery) -> OidcResponse {
	// The target request route.
	let route = "/_matrix/client/unstable/org.matrix.msc2964/login";
	let nonce = &rand::random::<u64>().to_string();
	let response_mode = query.response_mode.as_deref().unwrap_or("fragment");
	let template = LoginPageTemplate {
		nonce,
		hostname,
		route,
		client_id: query.client_id.as_str(),
		client_name: query.client_name.as_deref(),
		client_secret: query.client_secret.as_deref(),
		redirect_uri: query.redirect_uri.as_str(),
		scope: query.scope.as_str(),
		state: query.state.as_str(),
		code_challenge: query.code_challenge.as_str(),
		code_challenge_method: query.code_challenge_method.as_str(),
		response_type: query.response_type.as_str(),
		response_mode,
	};
	let body = Some(Body::Text(template.render().expect("login template render")));

	OidcResponse {
		status: StatusCode::OK,
		location: None,
		www_authenticate: None,
		body,
		nonce: Some(nonce.clone()),
	}
}
