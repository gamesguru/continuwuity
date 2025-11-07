#[derive(serde::Deserialize, Debug, Clone)]
pub enum TokenTypeHint {
	AccessToken,
	RefreshToken,
}

/// The set of query parameters a client needs to get authorization.
#[derive(serde::Deserialize, Debug, Clone)]
pub struct RevokeQuery {
	pub token: String,
	pub token_type_hint: Option<TokenTypeHint>,
	pub client_id: Option<String>,
}
