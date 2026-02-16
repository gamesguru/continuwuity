use std::{
	env, fs,
	path::Path,
	process::{Command, Stdio},
};

fn main() {
	let local_bin = env::var("LOCAL_BIN").expect("LOCAL_BIN env var not set");
	let remote_bin = env::var("REMOTE_BIN").expect("REMOTE_BIN env var not set");
	let service_name = env::var("CONTINUWUITY").unwrap_or_else(|_| "conduwuit".to_owned());

	let local_path = Path::new(&local_bin);
	let remote_path = Path::new(&remote_bin);

	println!("Deploying {local_bin} to {remote_bin}");

	if !remote_path.exists() || !files_are_identical(local_path, remote_path) {
		println!("Installing binary...");
		let status = Command::new("sudo")
			.args(["install", "-b", "-p", "-m", "755", &local_bin, &remote_bin])
			.status()
			.expect("Failed to execute sudo install");

		if !status.success() {
			eprintln!("Install failed with status: {status}");
			std::process::exit(1);
		}
	} else {
		println!("Binary {remote_bin} is identical to {local_bin}. Skipping install.");
	}

	println!("Restarting {service_name} service...");
	// Try without sudo first (e.g. root user), fallback to sudo
	let status = Command::new("systemctl")
		.args(["restart", &service_name])
		.status();

	if status.is_err() || !status.as_ref().unwrap().success() {
		println!("Trying with sudo...");
		let sudo_status = Command::new("sudo")
			.args(["systemctl", "restart", &service_name])
			.status()
			.expect("Failed to execute sudo systemctl");

		if !sudo_status.success() {
			eprintln!("Restart failed with status: {sudo_status}");
			std::process::exit(1);
		}
	}

	println!("Deployment complete.");
}

fn files_are_identical(p1: &Path, p2: &Path) -> bool {
	// Simple size check first as optimization
	if let (Ok(m1), Ok(m2)) = (fs::metadata(p1), fs::metadata(p2)) {
		if m1.len() != m2.len() {
			return false;
		}
	}
	// Use cmp command for byte-by-byte comparison (std::fs reading is tedious for
	// large binaries) and consistent with previous Makefile logic.
	let status = Command::new("cmp")
		.arg("-s")
		.arg(p1)
		.arg(p2)
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status();

	match status {
		| Ok(s) => s.success(),
		| Err(_) => false, // Assume different if cmp fails or is missing
	}
}
