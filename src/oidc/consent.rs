use askama::Template;
use conduwuit_build_metadata::{GIT_REMOTE_COMMIT_URL, GIT_REMOTE_WEB_URL, version_tag};

/// The parameters for the OIDC consent page template.
#[derive(Template)]
#[template(path = "consent.html.j2")]
pub(crate) struct ConsentPageTemplate<'a> {
	pub(crate) nonce: &'a str,
	pub(crate) hostname: &'a str,
	pub(crate) route: &'a str,
	pub(crate) beneficiary: &'a str,
	pub(crate) client_id: &'a str,
	pub(crate) client_name: Option<&'a str>,
	pub(crate) client_secret: Option<&'a str>,
	pub(crate) redirect_uri: &'a str,
	pub(crate) scope: &'a str,
	pub(crate) state: &'a str,
	pub(crate) code_challenge: &'a str,
	pub(crate) code_challenge_method: &'a str,
	pub(crate) response_type: &'a str,
	pub(crate) response_mode: &'a str,
}
