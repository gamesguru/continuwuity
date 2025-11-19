use std::borrow::Cow;

use oxide_auth::code_grant::{
	accesstoken::{Authorization, Request as AccessTokenRequest},
	refresh::Request as RefreshTokenRequest,
};

#[derive(serde::Deserialize, Debug, Clone)]
pub struct AccessTokenForm<'a> {
	pub code: Option<Cow<'a, str>>,
	code_verifier: Option<Cow<'a, str>>,
	client_id: Option<Cow<'a, str>>,
	grant_type: Option<Cow<'a, str>>,
	redirect_uri: Option<Cow<'a, str>>,
	// Only needed for token refresh requests.
	refresh_token: Option<Cow<'a, str>>,
	scope: Option<Cow<'a, str>>,
}

impl AccessTokenRequest for AccessTokenForm<'_> {
	fn valid(&self) -> bool { true }

	fn code(&self) -> Option<Cow<'_, str>> { self.code.clone() }

	fn client_id(&self) -> Option<Cow<'_, str>> { self.client_id.clone() }

	fn grant_type(&self) -> Option<Cow<'_, str>> { self.grant_type.clone() }

	fn redirect_uri(&self) -> Option<Cow<'_, str>> { self.redirect_uri.clone() }

	fn authorization(&self) -> Authorization<'_> { Authorization::None }

	/// Placeholder.
	fn extension(&self, _key: &str) -> Option<Cow<'_, str>> { None }
}

impl RefreshTokenRequest for AccessTokenForm<'_> {
	fn valid(&self) -> bool { true }

	fn refresh_token(&self) -> Option<Cow<'_, str>> { self.refresh_token.clone() }

	/// Placeholder.
	fn authorization(&self) -> Option<(Cow<'_, str>, Cow<'_, [u8]>)> { None }

	fn scope(&self) -> Option<Cow<'_, str>> { self.scope.clone() }

	fn extension(&self, _key: &str) -> Option<Cow<'_, str>> { None }

	/// Wild-guessed.
	fn grant_type(&self) -> Option<Cow<'_, str>> { Some(Cow::Borrowed("refresh_token")) }
}
