use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EventMetadata {
	pub short_room_id: u64,
	pub is_outlier: bool,
	pub origin_server_ts: ruma::UInt,
	pub depth: ruma::UInt,
	pub soft_failed: bool,
	pub rejected: bool,
	pub redacted_by: Option<ruma::OwnedEventId>,
	pub short_state_hash: Option<u64>,
	#[serde(default)]
	pub local_topological_depth: u64,
	/// Timeline position counter. `None` = legacy record (not yet migrated).
	/// `Some(0)` = outlier / not in timeline. Normal events start at 1.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pdu_count: Option<u64>,
}
