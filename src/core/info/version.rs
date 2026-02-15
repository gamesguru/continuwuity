//! one true function for returning the conduwuit version with the necessary
//! CONDUWUIT_VERSION_EXTRA env variables used if specified
//!
//! Set the environment variable `CONDUWUIT_VERSION_EXTRA` to any UTF-8 string
//! to include it in parenthesis after the SemVer version. A common value are
//! git commit hashes.

use std::sync::OnceLock;

static BRANDING: &str = "continuwuity";
static SEMANTIC: &str = env!("CARGO_PKG_VERSION");

static VERSION: OnceLock<String> = OnceLock::new();
static USER_AGENT: OnceLock<String> = OnceLock::new();
static GIT_BRANCH: OnceLock<String> = OnceLock::new();
static GIT_COMMIT_HASH: OnceLock<String> = OnceLock::new();
static GIT_REMOTE_URL: OnceLock<String> = OnceLock::new();
static GIT_REMOTE_COMMIT_URL: OnceLock<String> = OnceLock::new();
static GIT_REMOTE_WEB_URL: OnceLock<String> = OnceLock::new();

#[inline]
#[must_use]
pub fn name() -> &'static str { BRANDING }

#[inline]
#[must_use]
pub fn version() -> &'static str { VERSION.get_or_init(init_version) }

#[inline]
#[must_use]
pub fn git_branch() -> &'static str { GIT_BRANCH.get_or_init(|| "unknown".to_owned()) }

#[inline]
#[must_use]
pub fn git_commit_hash() -> &'static str { GIT_COMMIT_HASH.get_or_init(|| "unknown".to_owned()) }

#[inline]
#[must_use]
pub fn git_remote_url() -> &'static str { GIT_REMOTE_URL.get_or_init(|| "unknown".to_owned()) }

#[inline]
#[must_use]
pub fn git_remote_commit_url() -> &'static str {
	GIT_REMOTE_COMMIT_URL.get_or_init(|| "unknown".to_owned())
}

#[inline]
#[must_use]
pub fn git_remote_web_url() -> &'static str {
	GIT_REMOTE_WEB_URL.get_or_init(|| "unknown".to_owned())
}

#[inline]
#[must_use]
pub fn user_agent() -> &'static str { USER_AGENT.get_or_init(init_user_agent) }

fn init_user_agent() -> String { format!("{}/{}", name(), version()) }

/// Initialize the version strings, should be called once at startup.
pub fn set(
	version: impl Into<String>,
	branch: impl Into<String>,
	hash: impl Into<String>,
	remote: impl Into<String>,
	commit_url: impl Into<String>,
	web_url: impl Into<String>,
) {
	let _ = VERSION.set(version.into());
	let _ = GIT_BRANCH.set(branch.into());
	let _ = GIT_COMMIT_HASH.set(hash.into());
	let _ = GIT_REMOTE_URL.set(remote.into());
	let _ = GIT_REMOTE_COMMIT_URL.set(commit_url.into());
	let _ = GIT_REMOTE_WEB_URL.set(web_url.into());
}

fn init_version() -> String { SEMANTIC.to_owned() }
