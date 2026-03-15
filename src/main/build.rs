use std::{env, fmt::Write, fs, path::Path};

fn main() {
	println!("cargo:rerun-if-changed=Cargo.toml");

	// Embed dynamic lib paths in final binary so users don't need LD_LIBRARY_PATH
	let mut rpaths = std::collections::BTreeSet::new();

	if let Ok(lib_dir) = env::var("ROCKSDB_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=ROCKSDB_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = env::var("SNAPPY_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=SNAPPY_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = env::var("ZSTD_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=ZSTD_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = env::var("BZIP2_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=BZIP2_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = env::var("LZ4_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=LZ4_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(jemalloc) = env::var("JEMALLOC_OVERRIDE") {
		println!("cargo:rerun-if-env-changed=JEMALLOC_OVERRIDE");
		if let Some(parent) = Path::new(&jemalloc).parent() {
			if let Some(dir) = parent.to_str() {
				rpaths.insert(dir.to_owned());
			}
		}
	}

	for rpath in rpaths {
		println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
	}

	let cargo_toml = fs::read_to_string("Cargo.toml").unwrap();
	let mut available_features = Vec::new();
	let mut in_features = false;
	let mut in_dependencies = false;

	for line in cargo_toml.lines() {
		let line = line.trim();
		if line.starts_with("[features]") {
			in_features = true;
			in_dependencies = false;
			continue;
		}
		if line.starts_with("[dependencies]") || line.starts_with("[target.") {
			in_features = false;
			in_dependencies = true;
			continue;
		}
		if line.starts_with('[') {
			in_features = false;
			in_dependencies = false;
			continue;
		}

		if in_features && !line.is_empty() && !line.starts_with('#') {
			if let Some((feat, _)) = line.split_once('=') {
				available_features.push(feat.trim().to_owned());
			}
		} else if in_dependencies && !line.is_empty() && !line.starts_with('#') {
			if line.contains("optional = true") {
				// e.g. `console-subscriber.optional = true` or `console-subscriber = { optional
				// = true }`
				if let Some((dep, _)) = line.split_once('.') {
					available_features.push(dep.trim().to_owned());
				} else if let Some((dep, _)) = line.split_once('=') {
					available_features.push(dep.trim().to_owned());
				}
			}
		}
	}
	available_features.sort();
	available_features.dedup();

	let mut enabled_features = Vec::new();
	for (key, _) in env::vars() {
		if let Some(f) = key.strip_prefix("CARGO_FEATURE_") {
			println!("cargo:rerun-if-env-changed={key}");
			let normalized = f.to_lowercase();

			// Find the original feature name that matches this normalized representation
			let mut matched = false;
			for orig in &available_features {
				if orig.to_lowercase().replace('-', "_") == normalized {
					if orig != "default" {
						enabled_features.push(orig.clone());
					}
					matched = true;
					break;
				}
			}

			// Fallback if not found in Cargo.toml (should rarely happen)
			if !matched && normalized != "default" {
				enabled_features.push(normalized.replace('_', "-"));
			}
		}
	}
	enabled_features.sort();

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
