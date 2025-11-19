use std::borrow::Cow;

use oxide_auth::code_grant::authorization::Request as AuthorizationRequest;
use url::Url;

use super::LoginQuery;

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
	pub owner_allowance: Option<String>,
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

impl From<LoginQuery> for AuthorizationQuery {
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
			owner_allowance,
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
			owner_allowance,
		}
	}
}
