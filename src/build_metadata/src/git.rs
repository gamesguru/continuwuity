use std::process::Command;

fn format_description(s: String) -> String {
	let s = s.trim().trim_start_matches('v').to_owned();

	// Rewrite, i.e., 0.5.5-26-g23701cf0-dirty to 0.5.5+26~23701cf0-dirty
	if let Some((prefix, suffix)) = s.rsplit_once("-g") {
		if let Some((ver, count)) = prefix.rsplit_once('-') {
			if count.chars().all(char::is_numeric) {
				return format!("{ver}+{count}~{suffix}");
			}
		}
	}

	// Fallback: just replace -g with ~ (e.g. if count is missing or not numeric)
	s.replace("-g", "~")
}

pub(crate) fn description() -> Option<String> {
	let output = Command::new("git")
		.args(["describe", "--tags", "--always", "--dirty"])
		.output()
		.ok()?;

	if !output.status.success() {
		return None;
	}

	let s = String::from_utf8(output.stdout).ok()?;
	Some(format_description(s))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_format_description() {
		assert_eq!(
			format_description("v0.5.5-26-g23701cf0-dirty".to_owned()),
			"0.5.5+26~23701cf0-dirty"
		);
		assert_eq!(format_description("v0.5.5-26-g23701cf0".to_owned()), "0.5.5+26~23701cf0");
		assert_eq!(format_description("0.5.5-26-g23701cf0".to_owned()), "0.5.5+26~23701cf0");
		// Shallow clone / no tags / just hash
		assert_eq!(format_description("abc1234".to_owned()), "abc1234");
		assert_eq!(format_description("abc1234-dirty".to_owned()), "abc1234-dirty");
		// Weird case where count is missing or not numeric (fallback)
		assert_eq!(format_description("v0.5.5-beta-g23701cf0".to_owned()), "0.5.5-beta~23701cf0");
	}
}
