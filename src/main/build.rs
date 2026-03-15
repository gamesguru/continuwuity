fn main() {
	println!("cargo:rerun-if-changed=Cargo.toml");

	// Embed the dynamic library paths into the final binary so users don't need LD_LIBRARY_PATH
	let mut rpaths = std::collections::HashSet::new();

	if let Ok(lib_dir) = std::env::var("ROCKSDB_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=ROCKSDB_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = std::env::var("SNAPPY_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=SNAPPY_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = std::env::var("ZSTD_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=ZSTD_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = std::env::var("BZIP2_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=BZIP2_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(lib_dir) = std::env::var("LZ4_LIB_DIR") {
		println!("cargo:rerun-if-env-changed=LZ4_LIB_DIR");
		rpaths.insert(lib_dir);
	}
	if let Ok(jemalloc) = std::env::var("JEMALLOC_OVERRIDE") {
		println!("cargo:rerun-if-env-changed=JEMALLOC_OVERRIDE");
		if let Some(parent) = std::path::Path::new(&jemalloc).parent() {
			if let Some(dir) = parent.to_str() {
				rpaths.insert(dir.to_owned());
			}
		}
	}

	for rpath in rpaths {
		println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
	}
}
