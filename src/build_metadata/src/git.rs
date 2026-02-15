use std::process::Command;

pub(crate) fn description() -> Option<String> {
	let output = Command::new("git")
		.args(["describe", "--tags", "--always", "--dirty"])
		.output()
		.ok()?;

	if !output.status.success() {
		return None;
	}

	let s = String::from_utf8(output.stdout).ok()?;
	let mut s = s.trim().trim_start_matches('v').to_owned();

	// Rewrite, i.e., 0.5.5-26-g23701cf0-dirty to 0.5.5+26~23701cf0-dirty
	if let Some((prefix, suffix)) = s.rsplit_once("-g") {
		if let Some((ver, count)) = prefix.rsplit_once('-') {
			if count.chars().all(char::is_numeric) {
				s = format!("{ver}+{count}~{suffix}");
				return Some(s);
			}
		}
	}

	Some(s.replace("-g", "~"))
}
