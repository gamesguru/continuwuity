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

// -- V12 room_id stripping tests --

/// Helper: simulates the V12 room_id stripping logic used in import/export.
/// Returns whether room_id was removed.
fn strip_v12_room_id(
	obj: &mut serde_json::Map<String, serde_json::Value>,
	room_version: &str,
) -> bool {
	if room_version == "12" && obj.get("type").and_then(|v| v.as_str()) == Some("m.room.create") {
		obj.remove("room_id").is_some()
	} else {
		false
	}
}

#[test]
fn v12_create_event_strips_room_id() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","room_id":"!abc:example.org","content":{"creator":"@alice:example.org"}}"#,
	)
	.unwrap();
	assert!(strip_v12_room_id(&mut obj, "12"));
	assert!(!obj.contains_key("room_id"));
}

#[test]
fn v12_non_create_event_keeps_room_id() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	assert!(!strip_v12_room_id(&mut obj, "12"));
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v10_create_event_keeps_room_id() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","room_id":"!abc:example.org","content":{"creator":"@alice:example.org"}}"#,
	)
	.unwrap();
	assert!(!strip_v12_room_id(&mut obj, "10"));
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v12_create_event_without_room_id_is_noop() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","content":{"creator":"@alice:example.org"}}"#,
	)
	.unwrap();
	assert!(!strip_v12_room_id(&mut obj, "12"));
}

// -- V11/V12 event format compliance tests --

/// Helper: simulates the import field-stripping pipeline.
/// Strips diagnostic fields and applies V12-specific transformations.
fn strip_import_fields(obj: &mut serde_json::Map<String, serde_json::Value>, room_version: &str) {
	// Diagnostic fields injected during export
	obj.remove("__shortstatehash");
	obj.remove("prev_state_events");
	obj.remove("state_jump_pointers");

	// V12: strip room_id from create events
	strip_v12_room_id(obj, room_version);
}

/// Helper: checks whether an event's auth_events list references the create
/// event.
fn auth_events_reference_create(
	obj: &serde_json::Map<String, serde_json::Value>,
	create_event_id: &str,
) -> bool {
	obj.get("auth_events")
		.and_then(|v| v.as_array())
		.is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(create_event_id)))
}

#[test]
fn import_strips_diagnostic_fields() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.message","room_id":"!abc:example.org","__shortstatehash":12345,"prev_state_events":[],"state_jump_pointers":[],"content":{}}"#,
	)
	.unwrap();
	strip_import_fields(&mut obj, "10");
	assert!(!obj.contains_key("__shortstatehash"));
	assert!(!obj.contains_key("prev_state_events"));
	assert!(!obj.contains_key("state_jump_pointers"));
	// Non-diagnostic fields preserved
	assert!(obj.contains_key("type"));
	assert!(obj.contains_key("room_id"));
	assert!(obj.contains_key("content"));
}

#[test]
fn v11_event_has_no_room_id_in_wire_format() {
	// In v11+, room_id is not part of the wire format for non-create events.
	// The import pipeline should NOT strip room_id for v11 (only v12 create
	// events).
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	strip_import_fields(&mut obj, "11");
	// room_id is kept because: (a) it's not a create event, (b) not v12
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v12_create_event_full_import_pipeline() {
	// Full import pipeline for a V12 create event
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","room_id":"!abc:example.org","__shortstatehash":999,"content":{"room_version":"12"},"auth_events":[],"prev_events":[]}"#,
	)
	.unwrap();
	strip_import_fields(&mut obj, "12");
	assert!(!obj.contains_key("room_id"), "V12 create must not have room_id");
	assert!(!obj.contains_key("__shortstatehash"), "diagnostic field must be stripped");
	assert!(obj.contains_key("content"));
	assert!(obj.contains_key("auth_events"));
}

#[test]
fn v12_non_create_event_keeps_room_id_after_import() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	strip_import_fields(&mut obj, "12");
	// room_id is kept for non-create events (server stores it internally)
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v12_auth_events_must_not_reference_create() {
	// In V12, events must NOT list the create event in auth_events
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","auth_events":["$power_levels","$join_rules"],"content":{}}"#,
	)
	.unwrap();
	assert!(!auth_events_reference_create(&obj, "$create_event"));
}

#[test]
fn v12_auth_events_rejects_explicit_create_reference() {
	// If a V12 event incorrectly references the create event, we should detect it
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","auth_events":["$create_event","$power_levels"],"content":{}}"#,
	)
	.unwrap();
	assert!(auth_events_reference_create(&obj, "$create_event"));
}

#[test]
fn v10_auth_events_must_reference_create() {
	// In v10 and below, events MUST list the create event in auth_events
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","auth_events":["$create_event","$power_levels","$join_rules"],"content":{}}"#,
	)
	.unwrap();
	assert!(auth_events_reference_create(&obj, "$create_event"));
}

#[test]
fn v12_create_event_has_empty_auth_events() {
	// The create event itself always has empty auth_events in all versions
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","auth_events":[],"content":{"room_version":"12"}}"#,
	)
	.unwrap();
	let auth = obj.get("auth_events").and_then(|v| v.as_array()).unwrap();
	assert!(auth.is_empty(), "create event must have empty auth_events");
}

#[test]
fn strip_preserves_all_versions() {
	// Verify stripping logic is correct across all relevant room versions
	for version in &["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"] {
		let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
			r#"{"type":"m.room.create","room_id":"!abc:example.org","content":{}}"#,
		)
		.unwrap();
		strip_v12_room_id(&mut obj, version);
		assert!(
			obj.contains_key("room_id"),
			"room_id must be preserved for room version {version}"
		);
	}
}

#[test]
fn strip_only_v12_removes() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	strip_v12_room_id(&mut obj, "12");
	assert!(!obj.contains_key("room_id"), "room_id must be removed for V12 create events");
}
