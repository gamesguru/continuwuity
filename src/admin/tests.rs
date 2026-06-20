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
fn yolo_list_outliers_basic() { parse_yolo(&["yolo", "list-outliers"]).unwrap(); }

#[test]
fn yolo_list_outliers_with_room() {
	parse_yolo(&["yolo", "list-outliers", "!foo:example.org"]).unwrap();
}

#[test]
fn yolo_list_outliers_rejected_flag() {
	parse_yolo(&["yolo", "list-outliers", "!foo:example.org", "--rejected"]).unwrap();
}

#[test]
fn yolo_list_outliers_clear_requires_rejected() {
	// --clear without --rejected should fail
	parse_yolo(&["yolo", "list-outliers", "!foo:example.org", "--clear"]).unwrap_err();
}

#[test]
fn yolo_list_outliers_rejected_and_clear() {
	parse_yolo(&["yolo", "list-outliers", "!foo:example.org", "--rejected", "--clear"]).unwrap();
}

#[test]
fn yolo_list_outliers_with_limit() {
	parse_yolo(&["yolo", "list-outliers", "--limit", "50"]).unwrap();
}

#[test]
fn yolo_list_outliers_with_sender() {
	parse_yolo(&["yolo", "list-outliers", "--sender", "@user:example.org"]).unwrap();
}

#[test]
fn yolo_get_room_dag_negative_end() {
	// Tests allow_hyphen_values = true
	parse_yolo(&["yolo", "get-room-dag", "!foo:example.org", "0", "-1"]).unwrap();
}

#[test]
fn yolo_view_extremities_requires_room_or_all() {
	// Neither room nor --all should fail
	parse_yolo(&["yolo", "view-extremities"]).unwrap_err();
}

#[test]
fn yolo_view_extremities_all() { parse_yolo(&["yolo", "view-extremities", "--all"]).unwrap(); }

#[test]
fn yolo_view_extremities_with_room() {
	parse_yolo(&["yolo", "view-extremities", "!foo:example.org"]).unwrap();
}

// -- V11+/V12+ room_id stripping tests --

/// Helper: simulates the V11+/V12+ room_id stripping logic used in
/// import/export. V11: strips room_id from all non-create events (MSC3820)
/// V12+: strips room_id from ALL events including create (MSC4291)
fn strip_room_id_if_needed(
	obj: &mut serde_json::Map<String, serde_json::Value>,
	room_version: &str,
) -> bool {
	let is_create = obj.get("type").and_then(|v| v.as_str()) == Some("m.room.create");

	let room_version_id =
		ruma::RoomVersionId::try_from(room_version).unwrap_or(ruma::RoomVersionId::V1);
	let room_features = conduwuit_core::RoomVersion::new(&room_version_id)
		.unwrap_or(conduwuit_core::RoomVersion::V1);

	if room_features.strips_room_id(is_create) {
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
	assert!(strip_room_id_if_needed(&mut obj, "12"));
	assert!(!obj.contains_key("room_id"));
}

#[test]
fn v12_non_create_event_keeps_room_id() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	assert!(!strip_room_id_if_needed(&mut obj, "12"));
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v11_non_create_event_keeps_room_id() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	assert!(!strip_room_id_if_needed(&mut obj, "11"));
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v11_create_event_keeps_room_id() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","room_id":"!abc:example.org","content":{"creator":"@alice:example.org"}}"#,
	)
	.unwrap();
	assert!(!strip_room_id_if_needed(&mut obj, "11"));
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v10_create_event_keeps_room_id() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","room_id":"!abc:example.org","content":{"creator":"@alice:example.org"}}"#,
	)
	.unwrap();
	assert!(!strip_room_id_if_needed(&mut obj, "10"));
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v12_create_event_without_room_id_is_noop() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","content":{"creator":"@alice:example.org"}}"#,
	)
	.unwrap();
	assert!(!strip_room_id_if_needed(&mut obj, "12"));
}

// -- V11/V12 event format compliance tests --

/// Helper: simulates the import field-stripping pipeline.
/// Strips diagnostic fields and applies room_id transformations.
fn strip_import_fields(obj: &mut serde_json::Map<String, serde_json::Value>, room_version: &str) {
	// Diagnostic fields injected during export
	obj.remove("__shortstatehash");
	obj.remove("prev_state_events");
	obj.remove("state_jump_pointers");

	strip_room_id_if_needed(obj, room_version);
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
fn v11_event_keeps_room_id_in_wire_format() {
	// In v11, room_id IS part of the wire format.
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	strip_import_fields(&mut obj, "11");
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v12_create_event_full_import_pipeline() {
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
	assert!(obj.contains_key("room_id"));
}

#[test]
fn v12_auth_events_must_not_reference_create() {
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","auth_events":["$power_levels","$join_rules"],"content":{}}"#,
	)
	.unwrap();
	assert!(!auth_events_reference_create(&obj, "$create_event"));
}

#[test]
fn v12_auth_events_rejects_explicit_create_reference() {
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","auth_events":["$create_event","$power_levels"],"content":{}}"#,
	)
	.unwrap();
	assert!(auth_events_reference_create(&obj, "$create_event"));
}

#[test]
fn v10_auth_events_must_reference_create() {
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.member","auth_events":["$create_event","$power_levels","$join_rules"],"content":{}}"#,
	)
	.unwrap();
	assert!(auth_events_reference_create(&obj, "$create_event"));
}

#[test]
fn v12_create_event_has_empty_auth_events() {
	let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","auth_events":[],"content":{"room_version":"12"}}"#,
	)
	.unwrap();
	let auth = obj.get("auth_events").and_then(|v| v.as_array()).unwrap();
	assert!(auth.is_empty(), "create event must have empty auth_events");
}

#[test]
fn strip_preserves_older_versions() {
	for version in &["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"] {
		let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
			r#"{"type":"m.room.member","room_id":"!abc:example.org","content":{}}"#,
		)
		.unwrap();
		strip_room_id_if_needed(&mut obj, version);
		assert!(
			obj.contains_key("room_id"),
			"room_id must be preserved for room version {version}"
		);
	}
}

#[test]
fn strip_v12_create_removes() {
	let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
		r#"{"type":"m.room.create","room_id":"!abc:example.org","content":{}}"#,
	)
	.unwrap();
	strip_room_id_if_needed(&mut obj, "12");
	assert!(!obj.contains_key("room_id"), "room_id must be removed for V12 create events");
}

#[tokio::test]
async fn test_yolo_audit_membership_drift() {
	let _ = rustls::crypto::ring::default_provider().install_default();

	use std::{path::PathBuf, sync::Arc};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
		pdu::PduBuilder,
	};
	use figment::{Figment, providers::Format};
	use ruma::{
		RoomId, RoomVersionId,
		events::room::{
			create::RoomCreateEventContent,
			member::{MembershipState, RoomMemberEventContent},
		},
	};

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_yolo_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}

	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}

	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build services");
	let services = services.start().await.expect("failed to start services");

	// Boot admin module context references
	crate::init(&services.admin).await;

	let room_id = RoomId::new(services.globals.server_name());
	let _short_id = services
		.rooms
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.rooms.state.mutex.lock(&room_id).await;

	// Create bot user
	let server_user = services.globals.server_user.as_ref();
	services
		.users
		.create(server_user, None, None)
		.await
		.unwrap();

	// 1. Create event
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCreateEventContent {
				federate: true,
				predecessor: None,
				room_version: RoomVersionId::V11,
				..RoomCreateEventContent::new_v11()
			}),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	// 2. Bot user joins
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::from(server_user),
				&RoomMemberEventContent::new(MembershipState::Join),
			),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	// Power levels event
	use ruma::events::room::power_levels::RoomPowerLevelsEventContent;
	let mut power_levels = RoomPowerLevelsEventContent::new();
	power_levels
		.users
		.insert(server_user.to_owned(), ruma::int!(100));
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &power_levels),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	// Join rules event
	use ruma::events::room::join_rules::{JoinRule, RoomJoinRulesEventContent};
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(JoinRule::Public)),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	drop(state_lock);

	// Assert cache is currently consistent
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "audit-membership failed: {res:?}");
	let output = match res {
		| Ok(Some(out)) => out.body().to_owned(),
		| _ => panic!("Expected output"),
	};
	assert!(
		output.contains("No actionable divergences."),
		"expected no divergences: {output}"
	);
	assert!(
		output.contains("OK: Membership cache is consistent"),
		"expected consistent cache: {output}"
	);

	// 1. Simulate user mismatch drift (user joined in state, but marked as left in
	//    cache)
	let user_id = ruma::user_id!("@user:test.conduwuit.local");
	services.users.create(user_id, None, None).await.unwrap();

	let state_lock = services.rooms.state.mutex.lock(&room_id).await;
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::from(user_id.as_str()),
				&RoomMemberEventContent::new(MembershipState::Join),
			),
			user_id,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();
	drop(state_lock);

	// Manually mark as left in cache (corrupt cache)
	services
		.rooms
		.state_cache
		.mark_as_left_silent(user_id, &room_id)
		.await;
	services
		.rooms
		.state_cache
		.update_joined_count(&room_id)
		.await;

	// Run audit-membership and check it reports inconsistency and heals it
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	let output = match res {
		| Ok(Some(out)) => out.body().to_owned(),
		| _ => panic!("Expected output"),
	};
	assert!(
		output.contains("✗ CACHE INCONSISTENCY"),
		"expected cache inconsistency: {output}"
	);
	assert!(output.contains("Cache repaired."), "expected cache to be repaired: {output}");

	// Assert cache is now consistent again
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	let output = match res {
		| Ok(Some(out)) => out.body().to_owned(),
		| _ => panic!("Expected output"),
	};
	assert!(
		output.contains("OK: Membership cache is consistent"),
		"expected consistent cache: {output}"
	);

	// 2. Simulate aggregate count mismatch drift (count drift)
	services.db["roomid_joinedcount"].raw_put(&room_id, 999_u64);

	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	let output = match res {
		| Ok(Some(out)) => out.body().to_owned(),
		| _ => panic!("Expected output"),
	};
	assert!(
		output.contains("✗ CACHE INCONSISTENCY"),
		"expected count inconsistency: {output}"
	);
	assert!(output.contains("cache=999"), "expected cached count in output: {output}");
	assert!(output.contains("Cache repaired."), "expected cache to be repaired: {output}");

	// Assert cache is consistent again
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	let output = match res {
		| Ok(Some(out)) => out.body().to_owned(),
		| _ => panic!("Expected output"),
	};
	assert!(
		output.contains("OK: Membership cache is consistent"),
		"expected consistent cache: {output}"
	);
}

#[tokio::test]
async fn test_yolo_reorder_timeline() {
	let _ = rustls::crypto::ring::default_provider().install_default();

	use std::{path::PathBuf, sync::Arc};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
		pdu::PduBuilder,
	};
	use figment::{Figment, providers::Format};
	use ruma::{
		RoomId, RoomVersionId,
		events::room::{
			create::RoomCreateEventContent,
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
		},
	};

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_reorder_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}

	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}

	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build services");
	let services = services.start().await.expect("failed to start services");

	// Boot admin module context references
	crate::init(&services.admin).await;

	let room_id = RoomId::new(services.globals.server_name());
	let _short_id = services
		.rooms
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.rooms.state.mutex.lock(&room_id).await;

	// Create bot user
	let server_user = services.globals.server_user.as_ref();
	services
		.users
		.create(server_user, None, None)
		.await
		.unwrap();

	// 1. Create event
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCreateEventContent {
				federate: true,
				predecessor: None,
				room_version: RoomVersionId::V11,
				..RoomCreateEventContent::new_v11()
			}),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	// 2. Bot user joins
	let join_event = services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::from(server_user),
				&RoomMemberEventContent::new(MembershipState::Join),
			),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	// 3. Append Event A
	let event_a = services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::timeline(&RoomMessageEventContent::text_plain("Event A")),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	// Reset extremities to join_event so that Event B is concurrent (fork)
	services
		.rooms
		.state
		.set_forward_extremities(&room_id, vec![join_event.clone()].into_iter(), &state_lock)
		.await;

	// 4. Append Event B
	let event_b = services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::timeline(&RoomMessageEventContent::text_plain("Event B")),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	drop(state_lock);

	// Mutate origin_server_ts: Event A = 2000, Event B = 1000
	let mut json_a = services
		.rooms
		.timeline
		.get_pdu_json(&event_a)
		.await
		.unwrap();
	json_a.insert("origin_server_ts".to_owned(), ruma::CanonicalJsonValue::Integer(2000.into()));
	let pdu_id_a = services.rooms.timeline.get_pdu_id(&event_a).await.unwrap();
	services
		.rooms
		.timeline
		.replace_pdu(&pdu_id_a, &json_a, &event_a)
		.await
		.unwrap();

	let mut json_b = services
		.rooms
		.timeline
		.get_pdu_json(&event_b)
		.await
		.unwrap();
	json_b.insert("origin_server_ts".to_owned(), ruma::CanonicalJsonValue::Integer(1000.into()));
	let pdu_id_b = services.rooms.timeline.get_pdu_id(&event_b).await.unwrap();
	services
		.rooms
		.timeline
		.replace_pdu(&pdu_id_b, &json_b, &event_b)
		.await
		.unwrap();

	// Check original order (Event A count < Event B count)
	let count_a_before = conduwuit::matrix::pdu::Id::from(
		services.rooms.timeline.get_pdu_id(&event_a).await.unwrap(),
	);
	let count_b_before = conduwuit::matrix::pdu::Id::from(
		services.rooms.timeline.get_pdu_id(&event_b).await.unwrap(),
	);
	assert!(
		count_a_before.shorteventid.into_signed() < count_b_before.shorteventid.into_signed(),
		"Event A should be before Event B initially"
	);

	// Run reorder-timeline
	let res = services
		.admin
		.command_in_place(
			format!("yolo reorder-timeline {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "reorder-timeline failed: {res:?}");

	// Run rebuild-state
	let res = services
		.admin
		.command_in_place(
			format!("yolo rebuild-state {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "rebuild-state failed: {res:?}");

	// Check new order (Event B should now be before Event A)
	let count_a_after = conduwuit::matrix::pdu::Id::from(
		services.rooms.timeline.get_pdu_id(&event_a).await.unwrap(),
	);
	let count_b_after = conduwuit::matrix::pdu::Id::from(
		services.rooms.timeline.get_pdu_id(&event_b).await.unwrap(),
	);
	println!(
		"count_b_after: {}, count_a_after: {}",
		count_b_after.shorteventid, count_a_after.shorteventid
	);
	assert!(
		count_b_after.shorteventid.into_signed() < count_a_after.shorteventid.into_signed(),
		"Event B should be before Event A after reordering. B: {}, A: {}",
		count_b_after.shorteventid,
		count_a_after.shorteventid
	);
}

#[tokio::test]
async fn test_busted_dag_resolution() {
	let _ = rustls::crypto::ring::default_provider().install_default();

	use std::{
		path::{Path, PathBuf},
		sync::Arc,
	};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
		matrix::Event,
	};
	use figment::{Figment, providers::Format};
	use futures::StreamExt;
	use ruma::RoomId;

	let dag_path = Path::new(
		"/run/media/shane/shane4tb-ent/dags/\
		 local-dag-L58ME6ufiP49v97UIOBIpvWKEgj4912JmECPuDzlvCI-v12-wombatx.me-d1-68018.jsonl",
	);
	if !dag_path.exists() {
		println!("Skipping test_busted_dag_resolution: test DAG file not found");
		return;
	}

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_busted_dag_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}

	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}

	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build services");
	let services = services.start().await.expect("failed to start services");

	// Boot admin module context references
	crate::init(&services.admin).await;

	let room_id = RoomId::parse("!L58ME6ufiP49v97UIOBIpvWKEgj4912JmECPuDzlvCI").unwrap();

	// 1. Import the DAG
	let start_import = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!(
				"yolo import-pdus {room_id} {} --skip-auth --skip-sig-verify --room-version 12",
				dag_path.to_string_lossy()
			),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "import-pdus failed: {res:?}");
	println!("yolo import-pdus took {:?}", start_import.elapsed());

	// Run reorder-timeline
	println!("Starting yolo reorder-timeline...");
	let start_reorder = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!("yolo reorder-timeline {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "reorder-timeline failed: {res:?}");
	println!("yolo reorder-timeline took {:?}", start_reorder.elapsed());

	// Bootstrap room state hash from the latest PDU
	let latest_pdu = services
		.rooms
		.timeline
		.latest_pdu_in_room(room_id)
		.await
		.unwrap();
	let latest_event_id = latest_pdu.event_id();
	let ssh = services
		.rooms
		.state_accessor
		.pdu_shortstatehash(latest_event_id)
		.await
		.unwrap();
	let state_lock = services.rooms.state.mutex.lock(room_id).await;
	services
		.rooms
		.state
		.set_room_state(room_id, ssh, &state_lock);
	drop(state_lock);

	// Run force-set-state (to trigger re-resolution on local DAG)
	println!("Starting force-set-state...");
	let start_force = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!("debug force-set-state {room_id} --event-id {latest_event_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "force-set-state failed: {res:?}");
	println!("force-set-state took {:?}", start_force.elapsed());

	// Run check-rooms (to check sanity)
	println!("Starting check-rooms...");
	let start_check = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			"yolo check-rooms".to_owned(),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "check-rooms failed: {res:?}");
	println!("check-rooms took {:?}", start_check.elapsed());

	// Run audit-membership
	println!("Starting audit-membership...");
	let start_audit = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "audit-membership failed: {res:?}");
	println!("audit-membership took {:?}", start_audit.elapsed());

	// Verify forward extremities count is small and not bloated (e.g. 2000 heads)
	let exts_count = services
		.rooms
		.state
		.get_forward_extremities(room_id)
		.count()
		.await;
	println!("Busted DAG resolved. Final forward extremities count: {exts_count}");
	assert!(exts_count < 10, "expected very few forward extremities, got: {exts_count}");
}

#[tokio::test]
async fn test_unredacted_room_dag_resolution() {
	let _ = rustls::crypto::ring::default_provider().install_default();

	use std::{
		path::{Path, PathBuf},
		sync::Arc,
	};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
		matrix::Event,
	};
	use figment::{Figment, providers::Format};
	use futures::StreamExt;
	use ruma::RoomId;

	let dag_path = Path::new(
		"/run/media/shane/shane4tb-ent/dags/remote-dag-BDSybzDpGyDxMHZzpN_unredacted.\
		 org-v10-unredacted.org-d1-23142.jsonl",
	);
	if !dag_path.exists() {
		println!("Skipping test_unredacted_room_dag_resolution: test DAG file not found");
		return;
	}

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_unredacted_room_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}

	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}

	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build services");
	let services = services.start().await.expect("failed to start services");

	// Boot admin module context references
	crate::init(&services.admin).await;

	let room_id = RoomId::parse("!BDSybzDpGyDxMHZzpN:unredacted.org").unwrap();

	// 1. Import the DAG
	let res = services
		.admin
		.command_in_place(
			format!(
				"yolo import-pdus {room_id} {} --skip-auth --skip-sig-verify --room-version 10",
				dag_path.to_string_lossy()
			),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "import-pdus failed: {res:?}");

	// Run reorder-timeline
	let res = services
		.admin
		.command_in_place(
			format!("yolo reorder-timeline {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "reorder-timeline failed: {res:?}");

	// Run rebuild-state
	let res = services
		.admin
		.command_in_place(
			format!("yolo rebuild-state {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "rebuild-state failed: {res:?}");

	// Bootstrap room state hash from the latest PDU
	let latest_pdu = services
		.rooms
		.timeline
		.latest_pdu_in_room(room_id)
		.await
		.unwrap();
	let latest_event_id = latest_pdu.event_id();
	let ssh = services
		.rooms
		.state_accessor
		.pdu_shortstatehash(latest_event_id)
		.await
		.unwrap();
	let state_lock = services.rooms.state.mutex.lock(room_id).await;
	services
		.rooms
		.state
		.set_room_state(room_id, ssh, &state_lock);
	drop(state_lock);

	// Run force-set-state (to trigger re-resolution on local DAG)
	let res = services
		.admin
		.command_in_place(
			format!("debug force-set-state {room_id} --event-id {latest_event_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "force-set-state failed: {res:?}");

	// Run check-rooms (to check sanity)
	let res = services
		.admin
		.command_in_place(
			"yolo check-rooms".to_owned(),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "check-rooms failed: {res:?}");

	// Run audit-membership
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "audit-membership failed: {res:?}");

	// Verify forward extremities count is small and not bloated (e.g. 2000 heads)
	let exts_count = services
		.rooms
		.state
		.get_forward_extremities(room_id)
		.count()
		.await;
	println!("Unredacted Room DAG resolved. Final forward extremities count: {exts_count}");
	assert!(exts_count < 10, "expected very few forward extremities, got: {exts_count}");
}

#[tokio::test]
async fn test_unredacted_lounge_dag_resolution() {
	let _ = rustls::crypto::ring::default_provider().install_default();

	use std::{
		path::{Path, PathBuf},
		sync::Arc,
	};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
		matrix::Event,
	};
	use figment::{Figment, providers::Format};
	use futures::StreamExt;
	use ruma::RoomId;

	let dag_path = Path::new(
		"/run/media/shane/shane4tb-ent/dags/\
		 merged-sM2LwqNHGQOgLf35gqxPMy9D7oYde2q9ADg8HPBM3kE-unredacted-lounge-v12-d1-84135.jsonl",
	);
	if !dag_path.exists() {
		println!("Skipping test_unredacted_lounge_dag_resolution: test DAG file not found");
		return;
	}

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path =
		std::env::temp_dir().join(format!("conduwuit_test_db_unredacted_lounge_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}

	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}

	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build services");
	let services = services.start().await.expect("failed to start services");

	// Boot admin module context references
	crate::init(&services.admin).await;

	let room_id = RoomId::parse("!sM2LwqNHGQOgLf35gqxPMy9D7oYde2q9ADg8HPBM3kE").unwrap();

	// 1. Import the DAG
	println!("Starting import-pdus...");
	let start_import = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!(
				"yolo import-pdus {room_id} {} --skip-auth --skip-sig-verify --room-version 12",
				dag_path.to_string_lossy()
			),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "import-pdus failed: {res:?}");
	println!("import-pdus took {:?}", start_import.elapsed());

	// Run reorder-timeline
	println!("Starting reorder-timeline...");
	let start_reorder = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!("yolo reorder-timeline {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "reorder-timeline failed: {res:?}");
	println!("reorder-timeline took {:?}", start_reorder.elapsed());

	// Run rebuild-state
	println!("Starting rebuild-state...");
	let start_rebuild = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!("yolo rebuild-state {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "rebuild-state failed: {res:?}");
	println!("rebuild-state took {:?}", start_rebuild.elapsed());

	// Bootstrap room state hash from the latest PDU
	let latest_pdu = services
		.rooms
		.timeline
		.latest_pdu_in_room(room_id)
		.await
		.unwrap();
	let latest_event_id = latest_pdu.event_id();
	let ssh = services
		.rooms
		.state_accessor
		.pdu_shortstatehash(latest_event_id)
		.await
		.unwrap();
	let state_lock = services.rooms.state.mutex.lock(room_id).await;
	services
		.rooms
		.state
		.set_room_state(room_id, ssh, &state_lock);
	drop(state_lock);

	// Run force-set-state (to trigger re-resolution on local DAG)
	println!("Starting force-set-state...");
	let start_force = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!("debug force-set-state {room_id} --event-id {latest_event_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "force-set-state failed: {res:?}");
	println!("force-set-state took {:?}", start_force.elapsed());

	// Run check-rooms (to check sanity)
	println!("Starting check-rooms...");
	let start_check = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			"yolo check-rooms".to_owned(),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "check-rooms failed: {res:?}");
	println!("check-rooms took {:?}", start_check.elapsed());

	// Run audit-membership
	println!("Starting audit-membership...");
	let start_audit = std::time::Instant::now();
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "audit-membership failed: {res:?}");
	println!("audit-membership took {:?}", start_audit.elapsed());

	// Verify forward extremities count is small and not bloated (e.g. 2000 heads)
	let exts_count = services
		.rooms
		.state
		.get_forward_extremities(room_id)
		.count()
		.await;
	println!("Unredacted Lounge DAG resolved. Final forward extremities count: {exts_count}");
	assert!(exts_count < 10, "expected very few forward extremities, got: {exts_count}");

	// Validate the expected state (compare with unredacted.org disputed winners)
	let resolved_state_ids: std::collections::HashSet<ruma::OwnedEventId> = services
		.rooms
		.state_accessor
		.state_full_pdus(ssh)
		.map(|pdu| pdu.event_id().to_owned())
		.collect()
		.await;

	let expected_present = [
		"$TN3aSG4dg-NueYfa8FNgOg154yVJlB_g102cf5eQiFY",
		"$x49Eu0L3xnLbMJ1sAJIk8wtj0moDiZyjya_rNh3U2UQ",
		"$xqrfEc0vwvpDFN4laAkpvtniqlv1oV7kb-RfdT7mXCI",
		"$0-Rwh5ycT6Hwr9jkoiSsOSKW7HK_xiSrNyCvzh2Whcs",
		"$4sXgVhE2a85_i94Ul_TvfwKVfpjIHUQKWcuzdw0W8as",
		"$CITU5ramZfoRbG5NuEBd_kMm6f9a1UJB5TKRhMpVT6E",
		"$Hk-xXbs52DhNQI_Ca1E2DkyNMazBITKkepo8IuqC7EI",
		"$DT2PAjF5OtuocQGMV_ekKgN68M6XaYYsO2TGQPGEZ_c",
	];

	let expected_absent = [
		"$AJsK9SExNlblHbfse7eDhSNISk9E871gJzbkqoTA9Ds",
		"$TtQ6QYSjCphiJuzNiwfINI-ylQQTkBSkWaMydae_nCc",
		"$YlZG-G6Ak3fdjf4TIHEA8oD7C_FHX8EwmwFYL6jXNtg",
		"$heDtrL6Z-AVUZkzEsqtIKLxIQpzhMwcEU4JZ1bRyXSE",
		"$kUBfA5z53UYwkouV54Wq_UgK_8vnszbTp8gflvF3qns",
		"$mK__qhCzbLBUyb4IjkIxXKQpmdBwr8vxWwd40sXn1U4",
		"$rmb6V2Nb_UScP9htYUTPOy9LhbWgxb5wxgMEIfj8aFM",
		"$EhAnh9S3GYGd3tHSsoVhZAGbQt9fPgV_ketRNIQDc0s",
	];

	for id in &expected_present {
		let eid = <&ruma::EventId>::try_from(*id).unwrap();
		assert!(resolved_state_ids.contains(eid), "resolved state should contain event: {id}");
	}

	for id in &expected_absent {
		let eid = <&ruma::EventId>::try_from(*id).unwrap();
		assert!(
			!resolved_state_ids.contains(eid),
			"resolved state should NOT contain event: {id}"
		);
	}
}

#[tokio::test]
async fn test_nheko_dag_resolution() {
	let _ = rustls::crypto::ring::default_provider().install_default();

	use std::{
		path::{Path, PathBuf},
		sync::Arc,
	};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
		matrix::Event,
	};
	use figment::{Figment, providers::Format};
	use futures::StreamExt;
	use ruma::RoomId;

	let dag_path_str = std::env::var("CONDUWUIT_TEST_DAG_FILE").unwrap_or_else(|_| {
		"/run/media/shane/shane4tb-ent/dags/local-dag-UbCmIlGTHNIgIRZcpt_nheko.im-v5-wombatx.\
		 me-d1-383595.jsonl"
			.to_owned()
	});
	let dag_path = Path::new(&dag_path_str);
	if !dag_path.exists() {
		println!("Skipping test_nheko_dag_resolution: test DAG file not found");
		return;
	}

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_nheko_room_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}

	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}

	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build services");
	let services = services.start().await.expect("failed to start services");

	// Boot admin module context references
	crate::init(&services.admin).await;

	let room_id = RoomId::parse("!UbCmIlGTHNIgIRZcpt:nheko.im").unwrap();

	// 1. Import the DAG
	let res = services
		.admin
		.command_in_place(
			format!(
				"yolo import-pdus {room_id} {} --skip-auth --skip-sig-verify --room-version 5",
				dag_path.to_string_lossy()
			),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "import-pdus failed: {res:?}");

	// Run reorder-timeline
	let res = services
		.admin
		.command_in_place(
			format!("yolo reorder-timeline {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "reorder-timeline failed: {res:?}");

	// Run rebuild-state
	let res = services
		.admin
		.command_in_place(
			format!("yolo rebuild-state {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "rebuild-state failed: {res:?}");

	// Bootstrap room state hash from the latest PDU
	let latest_pdu = services
		.rooms
		.timeline
		.latest_pdu_in_room(room_id)
		.await
		.unwrap();
	let latest_event_id = latest_pdu.event_id();
	let ssh = services
		.rooms
		.state_accessor
		.pdu_shortstatehash(latest_event_id)
		.await
		.unwrap();
	let state_lock = services.rooms.state.mutex.lock(room_id).await;
	services
		.rooms
		.state
		.set_room_state(room_id, ssh, &state_lock);
	drop(state_lock);

	// Run force-set-state (to trigger re-resolution on local DAG)
	let res = services
		.admin
		.command_in_place(
			format!("debug force-set-state {room_id} --event-id {latest_event_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "force-set-state failed: {res:?}");

	// Run check-rooms (to check sanity)
	let res = services
		.admin
		.command_in_place(
			"yolo check-rooms".to_owned(),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "check-rooms failed: {res:?}");

	// Run audit-membership
	let res = services
		.admin
		.command_in_place(
			format!("yolo audit-membership {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "audit-membership failed: {res:?}");

	// Verify forward extremities count is not bloated (originally 6344 heads)
	let exts_count = services
		.rooms
		.state
		.get_forward_extremities(room_id)
		.count()
		.await;
	println!("Nheko Room DAG resolved. Final forward extremities count: {exts_count}");
	assert!(exts_count < 10, "expected very few forward extremities, got: {exts_count}");
}

#[tokio::test]
async fn test_yolo_heal_receipts() {
	let _ = rustls::crypto::ring::default_provider().install_default();
	use std::{path::PathBuf, sync::Arc};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
	};
	use conduwuit_database::Json;
	use figment::{Figment, providers::Format};
	use futures::StreamExt;
	use ruma::{
		RoomId, UserId,
		events::receipt::{Receipt, ReceiptEvent, ReceiptEventContent, ReceiptType},
	};

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_heal_receipts_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}
	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}
	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build");
	let services = services.start().await.expect("failed to start");
	crate::init(&services.admin).await;

	let room_id = RoomId::new(services.globals.server_name());
	let user_id = UserId::parse("@user:test.conduwuit.local").unwrap();

	// 1. Manually insert duplicate receipts into the database
	let mut content1 = ReceiptEventContent(std::collections::BTreeMap::new());
	let mut users1 = std::collections::BTreeMap::new();
	users1
		.insert(user_id.into(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(1000_u32.into())));
	let mut types1 = std::collections::BTreeMap::new();
	types1.insert(ReceiptType::Read, users1);
	content1
		.0
		.insert(ruma::event_id!("$event1").to_owned(), types1);

	let event1 = ReceiptEvent {
		content: content1,
		room_id: room_id.clone(),
	};

	let mut content2 = ReceiptEventContent(std::collections::BTreeMap::new());
	let mut users2 = std::collections::BTreeMap::new();
	users2
		.insert(user_id.into(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(2000_u32.into())));
	let mut types2 = std::collections::BTreeMap::new();
	types2.insert(ReceiptType::Read, users2);
	content2
		.0
		.insert(ruma::event_id!("$event2").to_owned(), types2);

	let event2 = ReceiptEvent {
		content: content2,
		room_id: room_id.clone(),
	};

	// Insert in order
	let mut prefix = room_id.as_bytes().to_vec();
	prefix.push(conduwuit_database::SEP);

	let mut key1 = prefix.clone();
	key1.extend_from_slice(&1_u64.to_be_bytes());
	key1.push(conduwuit_database::SEP);
	key1.extend_from_slice(user_id.as_bytes());
	services.db["readreceiptid_readreceipt"].raw_put(&key1, Json(event1));

	let mut key2 = prefix.clone();
	key2.extend_from_slice(&2_u64.to_be_bytes());
	key2.push(conduwuit_database::SEP);
	key2.extend_from_slice(user_id.as_bytes());
	services.db["readreceiptid_readreceipt"].raw_put(&key2, Json(event2));

	assert_eq!(
		services.db["readreceiptid_readreceipt"]
			.raw_stream()
			.count()
			.await,
		2
	);

	let res = services
		.admin
		.command_in_place(
			"yolo heal-receipts".to_owned(),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "heal-receipts failed: {res:?}");

	let count = services.db["readreceiptid_readreceipt"]
		.raw_stream()
		.count()
		.await;
	assert_eq!(count, 1, "Expected exactly 1 receipt remaining, got {count}");
}

#[tokio::test]
async fn test_yolo_rescue_room() {
	let _ = rustls::crypto::ring::default_provider().install_default();

	use std::{path::PathBuf, sync::Arc};

	use conduwuit::{
		Server,
		config::Config,
		log::{Log, LogLevelReloadHandles, capture},
		pdu::PduBuilder,
	};
	use figment::{Figment, providers::Format};
	use ruma::{
		RoomId,
		events::room::{
			create::RoomCreateEventContent,
			member::{MembershipState, RoomMemberEventContent},
		},
	};

	static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_rescue_room_{count}"));
	let _ = std::fs::remove_dir_all(&db_path);

	struct TempDbGuard {
		path: PathBuf,
	}
	impl Drop for TempDbGuard {
		fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
	}
	let _guard = TempDbGuard { path: db_path.clone() };

	let figment = Figment::new().merge(figment::providers::Toml::string(&format!(
		r#"
			server_name = "test.conduwuit.local"
			database_path = "{}"
			"#,
		db_path.to_string_lossy().replace('\\', "/")
	)));

	let config = Config::new(&figment).expect("failed to parse config");
	let runtime_handle = tokio::runtime::Handle::current();
	let server = Arc::new(Server::new(config, Some(&runtime_handle), Log {
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(capture::State::default()),
	}));

	let services = service::Services::build(server)
		.await
		.expect("failed to build");
	let services = services.start().await.expect("failed to start");
	crate::init(&services.admin).await;

	let room_id = RoomId::new(services.globals.server_name());
	let server_user = services.globals.server_user.as_ref();
	services
		.users
		.create(server_user, None, None)
		.await
		.unwrap();

	let _admin_room = services.admin.get_admin_room().await.unwrap();

	let state_lock = services.rooms.state.mutex.lock(&room_id).await;

	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCreateEventContent::new_v11()),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();

	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::from(server_user),
				&RoomMemberEventContent::new(MembershipState::Join),
			),
			server_user,
			Some(&room_id),
			&state_lock,
		)
		.await
		.unwrap();
	drop(state_lock);

	services.db["roomid_shortstatehash"].remove(&room_id);

	let res = services
		.admin
		.command_in_place(
			format!("yolo rescue-room {room_id}"),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	assert!(res.is_ok(), "rescue-room failed: {res:?}");

	let res = services
		.admin
		.command_in_place(
			"yolo check-rooms".to_owned(),
			None,
			service::admin::InvocationSource::Console,
		)
		.await;
	let output = res.unwrap().unwrap().body().to_owned();
	assert!(!output.contains("✗"), "Expected clean state after rescue");
}
