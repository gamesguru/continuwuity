use std::process::Command;

#[path = "src/git.rs"]
mod git;

fn run_git_command(args: &[&str]) -> Option<String> {
	Command::new("git")
		.args(args)
		.output()
		.ok()
		.filter(|output| output.status.success())
		.and_then(|output| String::from_utf8(output.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty())
}
fn get_env(env_var: &str) -> Option<String> {
	match std::env::var(env_var) {
		| Ok(val) if !val.is_empty() => Some(val),
		| _ => None,
	}
}
fn main() {
	// built gets the default crate from the workspace. Not sure if this is intended
	// behavior, but it's what we want.
	built::write_built_file().expect("Failed to acquire build-time information");

	// --- Git Information ---
	// Get short commit hash
	let short_hash = run_git_command(&["rev-parse", "--short", "HEAD"])
		.unwrap_or_else(|| "unknown".to_owned());

	// Get full commit hash
	let full_hash =
		run_git_command(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());

	println!("cargo:rustc-env=GIT_COMMIT_HASH_SHORT={short_hash}");
	println!("cargo:rustc-env=GIT_COMMIT_HASH={full_hash}");

	// only rebuild if the HEAD commit changes
	// println!("cargo:rerun-if-changed=.git/HEAD");

	for (var, url) in [
		("GIT_REMOTE_URL", "remote.origin.url"),
		("GIT_REMOTE_WEB_URL", "remote.origin.web_url"),
		("GIT_REMOTE_COMMIT_URL", "remote.origin.commit_url"),
	] {
		if get_env(var).is_none() {
			if let Some(url) = run_git_command(&["config", "--get", url]) {
				println!("cargo:rustc-env={var}={url}");
			}
		}
	}

	// This is the version string that Conduwuit uses.
	// It is generated here and exposed as an environment variable.
	// We want this to be robust, so we handle pre-release tags and
	// the version string while keeping the hash dynamic
	if get_env("CONTINUWUITY_VERSION_EXTRA").is_none() {
		let desc = std::env::var("GIT_DESCRIBE").ok().or_else(git::description);

		let mut extra = vec![desc.unwrap_or_else(|| short_hash.clone())];

		if let Ok(ver) = std::env::var("CARGO_PKG_VERSION") {
			// Safely strip the base version and any leading + or -
			if let Some(stripped) = extra[0].strip_prefix(&ver) {
				extra[0] = stripped
					.trim_start_matches(|c| c == '+' || c == '-')
					.to_owned();
			}
		}

		if let Some(b) = get_env("CONTINUWUITY_BRANCH")
			.or_else(|| run_git_command(&["rev-parse", "--abbrev-ref", "HEAD"]))
		{
			println!("cargo:rustc-env=GIT_BRANCH={b}");
			if b != "main" && b != "master" {
				extra.push(format!("b={b}"));
			}
		}

		// Remove empty strings so we don't join with a leading comma
		extra.retain(|s| !s.is_empty());

		let extra_s = extra.join(",");
		println!("cargo:rustc-env=CONTINUWUITY_VERSION_EXTRA={extra_s}");
		println!(
			"cargo:warning=Continuwuity Version: {} ({})",
			std::env::var("CARGO_PKG_VERSION").unwrap_or_default(),
			extra_s
		);
	}

	// Get remote URL and convert to web URL
	let mut remote_url_web = None;
	if let Some(remote_url_raw) = get_env("GIT_REMOTE_URL")
		.or_else(|| run_git_command(&["config", "--get", "remote.origin.url"]))
	{
		println!("cargo:rustc-env=GIT_REMOTE_URL={remote_url_raw}");
		let web_url = if remote_url_raw.starts_with("https://") {
			remote_url_raw.trim_end_matches(".git").to_owned()
		} else if remote_url_raw.starts_with("git@") {
			remote_url_raw
				.trim_end_matches(".git")
				.replacen(':', "/", 1)
				.replacen("git@", "https://", 1)
		} else if remote_url_raw.starts_with("ssh://") {
			remote_url_raw
				.trim_end_matches(".git")
				.replacen("git@", "", 1)
				.replacen("ssh:", "https:", 1)
		} else {
			// Assume it's already a web URL or unknown format
			remote_url_raw
		};
		println!("cargo:rustc-env=GIT_REMOTE_WEB_URL={web_url}");
		remote_url_web = Some(web_url);
	}

	// Construct remote commit URL
	if let Some(remote_commit_url) = get_env("GIT_REMOTE_COMMIT_URL") {
		println!("cargo:rustc-env=GIT_REMOTE_COMMIT_URL={remote_commit_url}");
	} else if let Some(base_url) = remote_url_web.as_ref() {
		let hash = if full_hash != "unknown" {
			&full_hash
		} else {
			&short_hash
		};
		let commit_page = format!("{base_url}/commit/{hash}");
		println!("cargo:rustc-env=GIT_REMOTE_COMMIT_URL={commit_page}");
	}

	// Rerun if the git HEAD changes
	if let Some(head_path) = run_git_command(&["rev-parse", "--git-path", "HEAD"]) {
		println!("cargo:rerun-if-changed={head_path}");
	}

	// Rerun if the current branch ref changes (e.g. switching back/forth)
	if let Some(ref_path) = run_git_command(&["symbolic-ref", "--quiet", "HEAD"]) {
		if let Some(ref_path) = run_git_command(&["rev-parse", "--git-path", &ref_path]) {
			println!("cargo:rerun-if-changed={ref_path}");
		}
	}

	// Rerun if packed-refs changes (in case the branch is packed)
	if let Some(packed_refs_path) = run_git_command(&["rev-parse", "--git-path", "packed-refs"]) {
		println!("cargo:rerun-if-changed={packed_refs_path}");
	}

	println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH");
	println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH_SHORT");
	println!("cargo:rerun-if-env-changed=GIT_REMOTE_URL");
	println!("cargo:rerun-if-env-changed=GIT_DESCRIBE");
	println!("cargo:rerun-if-env-changed=CONTINUWUITY_VERSION_EXTRA");
	println!("cargo:rerun-if-env-changed=GIT_REMOTE_COMMIT_URL");
}
