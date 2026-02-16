#[path = "src/git.rs"]
mod git;
use std::process::Command;

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
	// built::write_built_file().expect("Failed to acquire build-time information");

	// --- Git Information ---
	let mut commit_hash = None;
	let mut commit_hash_short = None;
	let mut remote_url_web = None;

	// Get full commit hash
	if let Some(hash) =
		get_env("GIT_COMMIT_HASH").or_else(|| run_git_command(&["rev-parse", "HEAD"]))
	{
		println!("cargo:rustc-env=GIT_COMMIT_HASH={hash}");
		commit_hash = Some(hash);
	}

	// Get short commit hash
	if let Some(short_hash) = get_env("GIT_COMMIT_HASH_SHORT")
		.or_else(|| run_git_command(&["rev-parse", "--short", "HEAD"]))
	{
		println!("cargo:rustc-env=GIT_COMMIT_HASH_SHORT={short_hash}");
		commit_hash_short = Some(short_hash);
	}

	if get_env("CONTINUWUITY_VERSION_EXTRA").is_none() {
		let desc = std::env::var("GIT_DESCRIBE").ok().or_else(git::description);
		let mut extra = vec![desc.unwrap_or_else(|| {
			commit_hash_short
				.clone()
				.unwrap_or_else(|| "unknown".into())
		})];
		if let Ok(ver) = std::env::var("CARGO_PKG_VERSION") {
			if let Some(stripped) = extra[0].strip_prefix(&ver) {
				#[allow(clippy::assigning_clones)]
				{
					extra[0] = stripped.trim_start_matches(['+', '-']).to_owned();
				}
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
		extra.retain(|s| !s.is_empty());
		println!("cargo:rustc-env=CONTINUWUITY_VERSION_EXTRA={}", extra.join(","));
	}

	// Get remote URL and convert to web URL
	if let Some(remote_url_raw) = get_env("GIT_REMOTE_URL")
		.or_else(|| run_git_command(&["config", "--get", "remote.origin.url"]))
	{
		println!("cargo:rustc-env=GIT_REMOTE_URL={remote_url_raw}");
		let web_url = if remote_url_raw.starts_with("http") {
			remote_url_raw.trim_end_matches(".git").to_owned()
		} else {
			format!(
				"https://{}",
				remote_url_raw
					.trim_start_matches("ssh://")
					.trim_start_matches("git@")
					.replacen(':', "/", 1)
					.trim_end_matches(".git")
			)
		};
		println!("cargo:rustc-env=GIT_REMOTE_WEB_URL={web_url}");
		remote_url_web = Some(web_url);
	}

	// Construct remote commit URL
	if let Some(remote_commit_url) = get_env("GIT_REMOTE_COMMIT_URL") {
		println!("cargo:rustc-env=GIT_REMOTE_COMMIT_URL={remote_commit_url}");
	} else if let (Some(base_url), Some(hash)) =
		(&remote_url_web, commit_hash.as_ref().or(commit_hash_short.as_ref()))
	{
		let commit_page = format!("{base_url}/commit/{hash}");
		println!("cargo:rustc-env=GIT_REMOTE_COMMIT_URL={commit_page}");
	}

	// --- Rerun Triggers ---
	for arg in ["HEAD", "packed-refs"] {
		if let Some(p) = run_git_command(&["rev-parse", "--git-path", arg]) {
			println!("cargo:rerun-if-changed={p}");
		}
	}
	if let Some(ref_path) = run_git_command(&["symbolic-ref", "--quiet", "HEAD"]) {
		if let Some(p) = run_git_command(&["rev-parse", "--git-path", &ref_path]) {
			println!("cargo:rerun-if-changed={p}");
		}
	}

	println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH");
	println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH_SHORT");
	println!("cargo:rerun-if-env-changed=GIT_REMOTE_URL");
	println!("cargo:rerun-if-env-changed=GIT_REMOTE_COMMIT_URL");
	println!("cargo:rerun-if-env-changed=GIT_DESCRIBE");
	println!("cargo:rerun-if-env-changed=CONTINUWUITY_VERSION_EXTRA");
}
