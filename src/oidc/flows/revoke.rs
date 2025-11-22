#[derive(serde::Deserialize, Debug, Clone)]
pub enum TokenTypeHint {
	AccessToken,
	RefreshToken,
}

/// GET parameters an [OidcDevice] needs to get authorization.
#[derive(serde::Deserialize, Debug, Clone)]
pub struct RevokeQuery {
	pub token: String,
	pub token_type_hint: Option<TokenTypeHint>,
	pub client_id: Option<String>,
}
