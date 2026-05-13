use std::str::FromStr;

use ruma::{
	UInt,
	api::federation::space::{SpaceHierarchyParentSummary, SpaceHierarchyParentSummaryInit},
	owned_room_id, owned_server_name,
	space::SpaceRoomJoinRule,
};

use crate::rooms::spaces::{PaginationToken, get_parent_children_via};

#[test]
fn get_summary_children() {
	let summary: SpaceHierarchyParentSummary = SpaceHierarchyParentSummaryInit {
		num_joined_members: UInt::from(1_u32),
		room_id: owned_room_id!("!root:example.org"),
		world_readable: true,
		guest_can_join: true,
		join_rule: SpaceRoomJoinRule::Public,
		children_state: vec![
			serde_json::from_str(
				r#"{
                      "content": {
                        "via": [
                          "example.org"
                        ],
                        "suggested": false
                      },
                      "origin_server_ts": 1629413349153,
                      "sender": "@alice:example.org",
                      "state_key": "!foo:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
			serde_json::from_str(
				r#"{
                      "content": {
                        "via": [
                          "example.org"
                        ],
                        "suggested": true
                      },
                      "origin_server_ts": 1629413349157,
                      "sender": "@alice:example.org",
                      "state_key": "!bar:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
			serde_json::from_str(
				r#"{
                      "content": {
                        "via": [
                          "example.org"
                        ]
                      },
                      "origin_server_ts": 1629413349160,
                      "sender": "@alice:example.org",
                      "state_key": "!baz:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
		],
		allowed_room_ids: vec![],
	}
	.into();

	assert_eq!(get_parent_children_via(&summary, false), vec![
		(owned_room_id!("!bar:example.org"), vec![owned_server_name!("example.org")]),
		(owned_room_id!("!baz:example.org"), vec![owned_server_name!("example.org")]),
		(owned_room_id!("!foo:example.org"), vec![owned_server_name!("example.org")])
	]);
	assert_eq!(get_parent_children_via(&summary, true), vec![(
		owned_room_id!("!bar:example.org"),
		vec![owned_server_name!("example.org")]
	)]);
}

#[test]
fn get_summary_children_sorted_by_order() {
	let summary: SpaceHierarchyParentSummary = SpaceHierarchyParentSummaryInit {
		num_joined_members: UInt::from(1_u32),
		room_id: owned_room_id!("!root:example.org"),
		world_readable: true,
		guest_can_join: true,
		join_rule: SpaceRoomJoinRule::Public,
		children_state: vec![
			// No order field — should sort last (by room_id)
			serde_json::from_str(
				r#"{
                      "content": { "via": ["example.org"] },
                      "origin_server_ts": 1629413349153,
                      "sender": "@alice:example.org",
                      "state_key": "!zzz:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
			// order "b" — should be second
			serde_json::from_str(
				r#"{
                      "content": { "via": ["example.org"], "order": "b" },
                      "origin_server_ts": 1629413349157,
                      "sender": "@alice:example.org",
                      "state_key": "!second:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
			// order "a" — should be first
			serde_json::from_str(
				r#"{
                      "content": { "via": ["example.org"], "order": "a" },
                      "origin_server_ts": 1629413349160,
                      "sender": "@alice:example.org",
                      "state_key": "!first:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
			// No order field — should sort last (by room_id, before !zzz)
			serde_json::from_str(
				r#"{
                      "content": { "via": ["example.org"] },
                      "origin_server_ts": 1629413349165,
                      "sender": "@alice:example.org",
                      "state_key": "!aaa:example.org",
                      "type": "m.space.child"
                    }"#,
			)
			.unwrap(),
		],
		allowed_room_ids: vec![],
	}
	.into();

	// Expected: order "a" first, order "b" second, then no-order by room_id
	assert_eq!(get_parent_children_via(&summary, false), vec![
		(owned_room_id!("!first:example.org"), vec![owned_server_name!("example.org")]),
		(owned_room_id!("!second:example.org"), vec![owned_server_name!("example.org")]),
		(owned_room_id!("!aaa:example.org"), vec![owned_server_name!("example.org")]),
		(owned_room_id!("!zzz:example.org"), vec![owned_server_name!("example.org")]),
	]);
}

#[test]
fn invalid_pagination_tokens() {
	fn token_is_err(token: &str) { PaginationToken::from_str(token).unwrap_err(); }

	token_is_err("231_2_noabool");
	token_is_err("");
	token_is_err("111_3_");
	token_is_err("foo_not_int");
	token_is_err("11_4_true_");
	token_is_err("___");
	token_is_err("__false");
}

#[test]
fn valid_pagination_tokens() {
	assert_eq!(
		PaginationToken {
			short_room_ids: vec![5383, 42934, 283, 423],
			limit: UInt::from(20_u32),
			max_depth: UInt::from(1_u32),
			suggested_only: true
		},
		PaginationToken::from_str("5383,42934,283,423_20_1_true").unwrap()
	);

	assert_eq!(
		PaginationToken {
			short_room_ids: vec![740],
			limit: UInt::from(97_u32),
			max_depth: UInt::from(10539_u32),
			suggested_only: false
		},
		PaginationToken::from_str("740_97_10539_false").unwrap()
	);
}

#[test]
fn pagination_token_to_string() {
	assert_eq!(
		PaginationToken {
			short_room_ids: vec![740],
			limit: UInt::from(97_u32),
			max_depth: UInt::from(10539_u32),
			suggested_only: false
		}
		.to_string(),
		"740_97_10539_false"
	);

	assert_eq!(
		PaginationToken {
			short_room_ids: vec![9, 34],
			limit: UInt::from(3_u32),
			max_depth: UInt::from(1_u32),
			suggested_only: true
		}
		.to_string(),
		"9,34_3_1_true"
	);
}
