#[path = "../git.rs"]
mod git;

fn main() {
	if let Some(version) = git::description() {
		println!("{version}");
	} else {
		// Fallback or error handling if needed, though 'unknown' is used elsewhere
		println!("unknown");
	}
}
