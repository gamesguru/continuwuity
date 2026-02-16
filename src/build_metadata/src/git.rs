use std::process::Command;

pub(crate) fn run(args: &[&str]) -> Option<String> {
	Command::new("git")
		.args(args)
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty())
}

pub(crate) fn description() -> Option<String> {
	// --always fallback handles shallow clones (no tags) by returning the short
	// hash
	let s = run(&["describe", "--tags", "--always", "--dirty"])?;
	Some(format(&s))
}

fn format(s: &str) -> String {
	let s = s.trim().trim_start_matches('v').to_owned();
	if let Some((prefix, suffix)) = s.rsplit_once("-g") {
		if let Some((ver, count)) = prefix.rsplit_once('-') {
			if count.chars().all(char::is_numeric) {
				return format!("{ver}+{count}~{suffix}");
			}
		}
	}

	// Trim out unintuitive git suffixes (g being a hexadecimal value)
	match s.rsplit_once("-g") {
		| Some((prefix, suffix)) => format!("{prefix}~{suffix}"),
		| None => s,
	}
}

#[cfg(test)]
mod tests {
	use super::format;

	#[test]
	fn test_format() {
		assert_eq!(format("v0.5.5-26-g23701cf0-dirty"), "0.5.5+26~23701cf0-dirty");
		assert_eq!(format("v0.5.5-26-g23701cf0"), "0.5.5+26~23701cf0");
		assert_eq!(format("0.5.5-26-g23701cf0"), "0.5.5+26~23701cf0");
		// Shallow clone / no tags / just hash (e.g. from git describe --always)
		assert_eq!(format("abc1234"), "abc1234");
		assert_eq!(format("abc1234-dirty"), "abc1234-dirty");
		assert_eq!(format("v0.5.5-beta-g23701cf0"), "0.5.5-beta~23701cf0");
		// Tag names containing "-g" should not be corrupted
		assert_eq!(format("v1.0.0-gamma-g1234abc"), "1.0.0-gamma~1234abc");
	}
}
