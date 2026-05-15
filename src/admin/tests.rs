#![cfg(test)]

#[test]
fn get_help_short() { get_help_inner("-h"); }

#[test]
fn get_help_long() { get_help_inner("--help"); }

#[test]
fn get_help_subcommand() { get_help_inner("help"); }

fn get_help_inner(input: &str) {
	use clap::Parser;

	use crate::admin::AdminCommand;

	let Err(error) = AdminCommand::try_parse_from(["argv[0] doesn't matter", input]) else {
		panic!("no error!");
	};

	let error = error.to_string();
	// Search for a handful of keywords that suggest the help printed properly
	assert!(error.contains("Usage:"));
	assert!(error.contains("Commands:"));
	assert!(error.contains("Options:"));
}

// -- Yolo subcommand parsing tests --

fn parse_yolo(args: &[&str]) -> Result<crate::admin::AdminCommand, clap::Error> {
	use clap::Parser;

	let mut full_args = vec!["admin"];
	full_args.extend_from_slice(args);
	crate::admin::AdminCommand::try_parse_from(full_args)
}

#[test]
fn yolo_list_outliers_basic() {
	assert!(parse_yolo(&["yolo", "list-outliers"]).is_ok());
}

#[test]
fn yolo_list_outliers_with_room() {
	assert!(parse_yolo(&["yolo", "list-outliers", "!foo:example.org"]).is_ok());
}

#[test]
fn yolo_list_outliers_rejected_flag() {
	assert!(parse_yolo(&["yolo", "list-outliers", "!foo:example.org", "--rejected"]).is_ok());
}

#[test]
fn yolo_list_outliers_clear_requires_rejected() {
	// --clear without --rejected should fail
	assert!(parse_yolo(&["yolo", "list-outliers", "!foo:example.org", "--clear"]).is_err());
}

#[test]
fn yolo_list_outliers_rejected_and_clear() {
	assert!(
		parse_yolo(&["yolo", "list-outliers", "!foo:example.org", "--rejected", "--clear"])
			.is_ok()
	);
}

#[test]
fn yolo_list_outliers_with_limit() {
	assert!(parse_yolo(&["yolo", "list-outliers", "--limit", "50"]).is_ok());
}

#[test]
fn yolo_list_outliers_with_sender() {
	assert!(parse_yolo(&["yolo", "list-outliers", "--sender", "@user:example.org"]).is_ok());
}

#[test]
fn yolo_get_room_dag_negative_end() {
	// Tests allow_hyphen_values = true
	assert!(parse_yolo(&["yolo", "get-room-dag", "!foo:example.org", "0", "-1"]).is_ok());
}

#[test]
fn yolo_view_extremities_requires_room_or_all() {
	// Neither room nor --all should fail
	assert!(parse_yolo(&["yolo", "view-extremities"]).is_err());
}

#[test]
fn yolo_view_extremities_all() {
	assert!(parse_yolo(&["yolo", "view-extremities", "--all"]).is_ok());
}

#[test]
fn yolo_view_extremities_with_room() {
	assert!(parse_yolo(&["yolo", "view-extremities", "!foo:example.org"]).is_ok());
}
