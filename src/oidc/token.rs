use std::borrow::Cow;

use oxide_auth::{
	code_grant::{
		accesstoken::{Authorization, Request as AccessTokenRequest},
		refresh::Request as RefreshTokenRequest,
	},
	endpoint::QueryParameter,
};

use crate::OidcRequest;

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
	/// Placeholder TODO replace.
	fn valid(&self) -> bool { true }

	fn code(&self) -> Option<Cow<'_, str>> { self.code.clone() }

	fn client_id(&self) -> Option<Cow<'_, str>> { self.client_id.clone() }

	fn grant_type(&self) -> Option<Cow<'_, str>> { self.grant_type.clone() }

	fn redirect_uri(&self) -> Option<Cow<'_, str>> { self.redirect_uri.clone() }

	fn authorization(&self) -> Authorization<'_> { Authorization::None }

	/// Placeholder TODO replace.
	fn extension(&self, _key: &str) -> Option<Cow<'_, str>> { None }
}

impl RefreshTokenRequest for AccessTokenForm<'_> {
	/// Placeholder TODO replace.
	fn valid(&self) -> bool { true }

	fn refresh_token(&self) -> Option<Cow<'_, str>> { self.refresh_token.clone() }

	/// Placeholder TODO replace.
	fn authorization(&self) -> Option<(Cow<'_, str>, Cow<'_, [u8]>)> { None }

	fn scope(&self) -> Option<Cow<'_, str>> { self.scope.clone() }

	fn extension(&self, _key: &str) -> Option<Cow<'_, str>> { None }

	/// Wild-guessed.
	fn grant_type(&self) -> Option<Cow<'_, str>> { Some(Cow::Borrowed("refresh_token")) }
}

#[derive(Debug)]
pub enum AuthError {
	NoBody,
	MissingField(String),
	InvalidField(String),
}

impl TryFrom<&mut OidcRequest> for AccessTokenForm<'_> {
	type Error = AuthError;

	fn try_from(value: &mut OidcRequest) -> Result<Self, Self::Error> {
		AccessTokenForm::try_from(value.clone())
	}
}

impl TryFrom<OidcRequest> for AccessTokenForm<'_> {
	type Error = AuthError;
	
	fn try_from(value: OidcRequest) -> Result<Self, Self::Error> {
		let body = value.body().ok_or(AuthError::NoBody)?;
		let getopt = |key| body.unique_value(key).map(|s| Cow::Owned(s.to_string()));

	    Ok(AccessTokenForm {
			code: getopt("code_challenge"),
			code_verifier: getopt("code_challenge_method"),
			client_id: getopt("client_id"),
			grant_type: getopt("grant_type"),
			redirect_uri: getopt("redirect_uri"),
			scope: getopt("scope"),
			refresh_token: getopt("refresh_token"),
		})
	}
}
