use std::{borrow::Cow, str::FromStr};

use oxide_auth::{
	code_grant::authorization::Request as AuthorizationRequest, endpoint::QueryParameter,
};
use url::Url;

use super::LoginQuery;
use crate::OidcRequest;

/// The set of parameters required for an OIDC authorization request.
#[derive(serde::Deserialize, Debug, Clone)]
pub struct AuthorizationQuery {
	pub client_id: String,
	pub client_name: Option<String>,
	pub client_secret: Option<String>,
	pub redirect_uri: Url,
	pub scope: String,
	pub state: String,
	pub code_challenge: String,
	pub code_challenge_method: String,
	pub response_type: String,
	pub response_mode: Option<String>,
	pub username: Option<String>,
	pub allow: Option<String>,
	pub deny: Option<String>,
}

impl AuthorizationRequest for AuthorizationQuery {
	fn valid(&self) -> bool { true }

	fn client_id(&self) -> Option<Cow<'_, str>> { Some(self.client_id.as_str().into()) }

	fn scope(&self) -> Option<Cow<'_, str>> { Some(self.scope.as_str().into()) }

	fn state(&self) -> Option<Cow<'_, str>> { Some(self.state.as_str().into()) }

	fn redirect_uri(&self) -> Option<Cow<'_, str>> { Some(self.redirect_uri.as_str().into()) }

	fn response_type(&self) -> Option<Cow<'_, str>> { Some(self.response_type.as_str().into()) }

	/// Placeholder.
	fn extension(&self, _key: &str) -> Option<Cow<'_, str>> { None }
}

#[derive(Debug)]
pub enum AuthError {
	NoQuery,
	MissingField(String),
	InvalidField(String),
}

impl TryFrom<&mut OidcRequest> for AuthorizationQuery {
	type Error = AuthError;

	fn try_from(value: &mut OidcRequest) -> Result<Self, Self::Error> {
		AuthorizationQuery::try_from(value.clone())
	}
}

impl TryFrom<OidcRequest> for AuthorizationQuery {
	type Error = AuthError;

	fn try_from(value: OidcRequest) -> Result<Self, Self::Error> {
		//let query = value.body().ok_or(AuthError::NoBody)?;
		let query = value.query().or(value.body()).ok_or(AuthError::NoQuery)?;

		let getopt = |key| query.unique_value(key).map(|s| s.to_string());
		let get = |key| {
			query
				.unique_value(key)
				.ok_or(AuthError::MissingField(key.into()))
				.map(|s| s.to_string())
		};

		Ok(AuthorizationQuery {
			client_id: get("client_id")?,
			client_name: getopt("client_name"),
			client_secret: getopt("client_secret"),
			redirect_uri: Url::from_str(&get("redirect_uri")?)
				.map_err(|_| AuthError::InvalidField("redirect_uri".into()))?,
			scope: get("scope")?,
			state: get("state")?,
			code_challenge: get("code_challenge")?,
			code_challenge_method: get("code_challenge_method")?,
			response_type: get("response_type")?,
			// response_mode is not strictly needed : it must be the literal "fragment"
			// when over https. It's required by the Matrix spec but Fractal doesn't provide it.
			response_mode: getopt("response_mode").or(Some("fragment".to_string())),
			username: getopt("username"),
			allow: getopt("allow"),
			deny: getopt("deny"),
		})
	}
}

impl From<LoginQuery> for AuthorizationQuery {
	/// Drops the `password` field on the way.
	fn from(value: LoginQuery) -> Self {
		let LoginQuery {
			client_id,
			client_name,
			client_secret,
			redirect_uri,
			scope,
			state,
			code_challenge,
			code_challenge_method,
			response_type,
			response_mode,
			username,
			allow,
			deny,
			..
		} = value;

		Self {
			client_id,
			client_name,
			client_secret,
			redirect_uri,
			scope,
			state,
			code_challenge,
			code_challenge_method,
			response_type,
			response_mode: Some(response_mode),
			username: Some(username),
			allow,
			deny,
		}
	}
}
