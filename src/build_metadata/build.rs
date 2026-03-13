#[path = "src/git.rs"]
mod git;

static GIT_BRANCH_MAIN: &str = "main";

fn get_env(env_var: &str) -> Option<String> {
	match std::env::var(env_var) {
		| Ok(val) if !val.is_empty() => Some(val),
		| _ => None,
	}
}

fn main() {
	// --- Git Information ---
	let mut commit_hash = None;
	let mut commit_hash_short = None;
	let mut remote_url_web = None;

	// Get full commit hash
	if let Some(hash) = get_env("GIT_COMMIT_HASH").or_else(|| git::run(&["rev-parse", "HEAD"])) {
		println!("cargo:rustc-env=GIT_COMMIT_HASH={hash}");
		commit_hash = Some(hash);
	}

	// Get short commit hash
	if let Some(short_hash) =
		get_env("GIT_COMMIT_HASH_SHORT").or_else(|| git::run(&["rev-parse", "--short", "HEAD"]))
	{
		println!("cargo:rustc-env=GIT_COMMIT_HASH_SHORT={short_hash}");
		commit_hash_short = Some(short_hash);
	}

	if get_env("CONTINUWUITY_VERSION_EXTRA").is_none() {
		let desc = git::description();
		let mut extra = vec![desc.unwrap_or_else(|| {
			commit_hash_short
				.clone()
				.unwrap_or_else(|| "unknown".into())
		})];
		if let Some(b) = get_env("CONTINUWUITY_BRANCH")
			.or_else(|| get_env("GITHUB_REF_NAME"))
			.or_else(|| git::run(&["rev-parse", "--abbrev-ref", "HEAD"]))
		{
			println!("cargo:rustc-env=GIT_BRANCH={b}");
			extra.extend(git::branch_tag(&b, GIT_BRANCH_MAIN));
		}
		extra.retain(|s| !s.is_empty());
		println!("cargo:rustc-env=CONTINUWUITY_VERSION_EXTRA={}", extra.join(","));
	}

	// Get remote URL and convert to web URL
	if let Some(remote_url_raw) =
		get_env("GIT_REMOTE_URL").or_else(|| git::run(&["config", "--get", "remote.origin.url"]))
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
		if let Some(p) = git::run(&["rev-parse", "--git-path", arg]) {
			println!("cargo:rerun-if-changed={p}");
		}
	}
	if let Some(ref_path) = git::run(&["symbolic-ref", "--quiet", "HEAD"]) {
		if let Some(p) = git::run(&["rev-parse", "--git-path", &ref_path]) {
			println!("cargo:rerun-if-changed={p}");
		}
	}

	println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH");
	println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH_SHORT");
	println!("cargo:rerun-if-env-changed=GIT_REMOTE_URL");
	println!("cargo:rerun-if-env-changed=GIT_REMOTE_COMMIT_URL");
	println!("cargo:rerun-if-env-changed=CONTINUWUITY_VERSION_EXTRA");
	println!("cargo:rerun-if-env-changed=CONTINUWUITY_BRANCH");

	// Host info
	println!("cargo:rustc-env=HOST_OS={}", std::env::consts::OS);
	println!("cargo:rustc-env=HOST_ARCH={}", std::env::consts::ARCH);

	// Build profile and environment variables passed by Cargo to the build script
	if let Ok(profile) = std::env::var("PROFILE") {
		println!("cargo:rustc-env=PROFILE={profile}");
	}
	if let Ok(opt_level) = std::env::var("OPT_LEVEL") {
		println!("cargo:rustc-env=OPT_LEVEL={opt_level}");
	}
	if let Ok(debug) = std::env::var("DEBUG") {
		println!("cargo:rustc-env=DEBUG={debug}");
	}
	if let Ok(target) = std::env::var("TARGET") {
		println!("cargo:rustc-env=TARGET={target}");
	}
	if let Ok(host) = std::env::var("HOST") {
		println!("cargo:rustc-env=HOST={host}");
	}

	// Target Configuration Variables
	if let Ok(endian) = std::env::var("CARGO_CFG_TARGET_ENDIAN") {
		println!("cargo:rustc-env=CFG_ENDIAN={endian}");
	}
	if let Ok(ptr_width) = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH") {
		println!("cargo:rustc-env=CFG_POINTER_WIDTH={ptr_width}");
	}
	if let Ok(env) = std::env::var("CARGO_CFG_TARGET_ENV") {
		println!("cargo:rustc-env=CFG_ENV={env}");
	}

	// Rustc Version
	if let Ok(rustc) = std::process::Command::new("rustc")
		.arg("--version")
		.output()
	{
		println!(
			"cargo:rustc-env=RUSTC_VERSION={}",
			String::from_utf8_lossy(&rustc.stdout).trim()
		);
	}
}
