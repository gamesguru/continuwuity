use serde::Deserialize;

/// Auth backend for the `authenticated_flow` config option.
#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthBackend {
	Turnstile,
	Recaptcha,
}

/// Default captcha backend order when `authenticated_flow` is empty.
pub const DEFAULT_AUTH_BACKENDS: [AuthBackend; 2] =
	[AuthBackend::Recaptcha, AuthBackend::Turnstile];
