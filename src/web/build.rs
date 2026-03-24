fn main() {
	// SAFETY: This is safe because this is a short-lived build script and there are
	// no multiple threads accessing the environment variables.
	unsafe { std::env::set_var("MEMORY_SERVE_QUIET", "1") };
	memory_serve::load_directory("./pages/resources");
}
