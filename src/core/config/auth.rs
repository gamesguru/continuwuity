use serde::Deserialize;

/// Auth backend for the `authenticated_flow` config option.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthBackend {
	Token,
	Turnstile,
	Recaptcha,
}
