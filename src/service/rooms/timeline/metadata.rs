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
	/// Timeline position counter. 0 = outlier / not in timeline.
	/// Normal events start at 1, backfilled events wrap to high u64.
	#[serde(default)]
	pub pdu_count: u64,
}
