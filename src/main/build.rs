use std::{env, fmt::Write, fs, path::Path};

fn main() {
	println!("cargo:rerun-if-changed=Cargo.toml");

	let mut enabled_features = Vec::new();
	for (key, _) in env::vars() {
		if let Some(f) = key.strip_prefix("CARGO_FEATURE_") {
			println!("cargo:rerun-if-env-changed={key}");
			let feature = f.to_lowercase().replace('_', "-");
			if feature != "default" {
				enabled_features.push(feature);
			}
		}
	}
	enabled_features.sort();

	let cargo_toml = fs::read_to_string("Cargo.toml").unwrap();
	let mut available_features = Vec::new();
	let mut in_features = false;
	for line in cargo_toml.lines() {
		let line = line.trim();
		if line.starts_with("[features]") {
			in_features = true;
			continue;
		}
		if in_features && line.starts_with('[') {
			break;
		}
		if in_features && !line.is_empty() && !line.starts_with('#') {
			if let Some((feat, _)) = line.split_once('=') {
				let feat = feat.trim();
				available_features.push(feat.to_owned());
			}
		}
	}
	available_features.sort();

	let out_dir = env::var_os("OUT_DIR").unwrap();
	let dest_path = Path::new(&out_dir).join("features.rs");

	let mut out = String::new();
	out.push_str(
		"pub const ENABLED_FEATURES: &[&str] = &[
",
	);
	for f in &enabled_features {
		writeln!(out, "    \"{f}\",").unwrap();
	}
	out.push_str("];\n\n");

	out.push_str(
		"pub const AVAILABLE_FEATURES: &[&str] = &[
",
	);
	for f in &available_features {
		writeln!(out, "    \"{f}\",").unwrap();
	}
	out.push_str("];\n");

	fs::write(&dest_path, out).unwrap();
}
