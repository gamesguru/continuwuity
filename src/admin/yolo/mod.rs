mod commands;
pub(crate) mod outlier_utils;

use clap::Subcommand;
use conduwuit::Result;
use ruma::{OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId};

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub enum YoloCommand {
	/// Audit the auth chain for a room, reporting events that are missing
	/// from both the timeline and outlier store (true DAG gaps).
	///
	/// Scans the auth chains of all current state events and buckets each
	/// referenced event as: timeline, outlier-only, or missing. With
	/// --fetch, fans out GET /event to all known room servers for each gap
	/// and stores successes as outliers.
	AuditAuthChain {
		/// The room ID to audit.
		room_id: OwnedRoomId,

		/// Fan out GET /event to all known room servers for missing events
		/// and store successful responses as outliers.
		#[arg(long)]
		fetch: bool,

		/// Show each missing/outlier event ID (default: summary only).
		#[arg(short, long)]
		verbose: bool,
	},

	/// Full membership audit: timeline vs state vs cache vs remote server.
	///
	/// Compares timeline membership against state snapshot, checks the
	/// membership cache for consistency, and optionally cross-references
	/// with a remote server.
	AuditMembership {
		/// The room ID to audit.
		room_id: OwnedRoomId,

		/// Optional remote server to cross-reference.
		#[arg(long)]
		server: Option<OwnedServerName>,

		/// Event ID to query remote state at. Defaults to latest PDU
		/// in the target room (not the admin room).
		#[arg(long)]
		at_event: Option<OwnedEventId>,

		/// If set, automatically demotes divergent timeline events to outliers
		#[arg(long)]
		clean: bool,
	},

	/// Attempts to "rescue" an outlier PDU by upgrading it to a timeline event.
	///
	/// This will perform all necessary auth checks and state resolution.
	/// Falls back to current room state when no server can provide historical
	/// /state_ids for the event.
	RescuePdu {
		/// An event ID (a $ followed by the base64 reference hash)
		event_id: OwnedEventId,

		/// Bypass all state resolution and auth checks entirely. Use when the
		/// network returns 404 for /state_ids (servers have pruned historical
		/// state) or the origin server no longer exists. After force-rescuing
		/// several events, run reorder-timeline --tail N to fix ordering.
		#[arg(long)]
		force: bool,
	},

	/// List all outlier PDUs in our database.
	ListOutliers {
		/// Filter outliers to a specific room
		room_id: Option<OwnedRoomOrAliasId>,

		/// Filter outliers to a specific sender
		#[arg(short, long)]
		sender: Option<OwnedUserId>,

		/// Limit the number of outliers listed.
		#[arg(short, long)]
		limit: Option<usize>,

		/// Only show rejected/soft-failed outliers (identifies
		/// cascade-poisoned events).
		#[arg(short, long, alias = "softfailed")]
		rejected: bool,

		/// Clear rejected/soft-fail markers for matched outliers (use with
		/// --rejected).
		#[arg(short, long, requires = "rejected")]
		clear: bool,

		/// Show newest events first (default: oldest first).
		#[arg(short = 'R', long)]
		reverse: bool,
	},

	/// View the current forward extremities (timeline tips) of a room,
	/// or scan all rooms for DAG fractures with --all.
	ViewExtremities {
		/// The room ID or alias.
		#[arg(required_unless_present = "all")]
		room: Option<OwnedRoomOrAliasId>,

		/// Show extremities for all rooms.
		#[arg(long, conflicts_with = "room")]
		all: bool,

		/// Show full details (type, sender, timestamp) for each extremity.
		#[arg(short, long)]
		verbose: bool,
	},

	/// Recalculate and fix forward extremities using true topological DAG
	/// resolution.
	RecalculateExtremities {
		/// The room ID or alias.
		room: OwnedRoomOrAliasId,

		/// The number of recent events to analyze (default: 50, or -1 for all
		/// events)
		#[arg(allow_hyphen_values = true, long, default_value_t = 50)]
		tail: i64,
	},

	/// Prune dangling forward extremities and reset them to the current room
	/// state.
	CleanExtremities {
		/// The room ID.
		room_id: OwnedRoomId,
	},

	/// Purge outlier PDUs that already exist in our timeline.
	///
	/// This is a safe cleanup command that resolves "stuck" state where an
	/// event exists in both the timeline and outlier tables. It will NOT
	/// delete outliers that haven't been rescued yet.
	///
	/// To purge a single event by ID, use `--event-id $id`.
	#[command(alias("purge-stuck"), alias("purge-outlier"))]
	PurgeOutliers {
		/// Purge a specific outlier event by event ID.
		#[arg(long)]
		event_id: Option<OwnedEventId>,

		/// Filter outliers to a specific room
		#[arg(short, long)]
		room_id: Option<OwnedRoomOrAliasId>,

		/// Filter outliers to a specific sender
		#[arg(short, long)]
		sender: Option<OwnedUserId>,

		/// Purge ALL outliers in the database.
		#[arg(long)]
		all: bool,

		/// Force-remove even un-rescued outliers (use with caution).
		#[arg(long)]
		force: bool,
	},

	/// Attempts to "rescue" all outlier PDUs in a room.
	RescueRoom {
		/// The room ID.
		room_id: OwnedRoomId,

		/// If set, bypasses strict auth checks.
		#[arg(short, long)]
		force: bool,

		/// If set, forcefully re-processes existing timeline PDUs.
		#[arg(long)]
		nuclear: bool,

		/// If set, rescues outliers in ALL rooms.
		#[arg(long)]
		all: bool,

		/// If set, includes the last N timeline PDUs for re-processing.
		#[arg(long)]
		timeline_limit: Option<usize>,

		/// If set, automatically runs reorder-timeline after rescue.
		/// Fixes anachronisms from rescued outliers being appended at the end.
		#[arg(long)]
		reorder: bool,

		/// After rescue, force-set room state from these server(s) using
		/// absolute override (--overwrite). Chains rescue-room →
		/// force-set-state into a single command. Implies --force.
		#[arg(long = "heal-from")]
		heal_from: Vec<OwnedServerName>,
	},

	/// Reorder the timeline for a room by origin_server_ts.
	///
	/// Fixes anachronisms caused by rescued outliers being appended at the
	/// end of the timeline instead of in chronological order.
	ReorderTimeline {
		/// The room ID.
		room_id: OwnedRoomId,

		/// If set, reorders timeline in ALL rooms.
		#[arg(long)]
		all: bool,

		/// Only reorder the last N events (fast path). Useful when only recent
		/// events are out of order (e.g. after force-set-state ingestion).
		/// When omitted, the full timeline is reordered.
		#[arg(long)]
		tail: Option<usize>,
	},

	/// Promote all outlier events in a room to backfill timeline PDUs.
	///
	/// This skips auth checks and directly inserts outliers into the timeline.
	/// Useful for rooms where the join flow stored events as outliers
	/// instead of timeline PDUs.
	PromoteOutliers {
		/// The room ID.
		room_id: OwnedRoomId,
	},

	/// Purge a PDU from the timeline (removes from both timeline and outlier
	/// tables).
	///
	/// Use this to remove rescued PDUs that are causing timeline issues.
	/// After purging, you should force-set room state and reorder.
	PurgeTimelinePdu {
		/// The event ID to purge from the timeline.
		event_id: OwnedEventId,
	},

	/// Get the room DAG as a list of PDUs in a range.
	GetRoomDag {
		/// Room ID
		room_id: OwnedRoomOrAliasId,

		/// Start index (0-based, or negative for offset from the end)
		#[arg(allow_hyphen_values = true)]
		start: i64,

		/// End index (0-based, inclusive, or -1 for all)
		#[arg(allow_hyphen_values = true)]
		end: i64,

		/// Print PDUs to the admin room (in addition to writing to file)
		#[arg(long)]
		print: bool,
	},

	/// Fetch a room's DAG from a remote server via federation backfill API
	/// and write it to a JSONL file.
	GetRemoteDag {
		/// Room ID
		room_id: OwnedRoomId,

		/// Remote server to fetch from
		server: OwnedServerName,

		/// Maximum number of events to fetch (-1 for unlimited, default: 100)
		#[arg(long, default_value = "100", allow_hyphen_values = true)]
		limit: i64,

		/// Event ID to start backfill from (default: latest local event)
		#[arg(long)]
		from: Option<OwnedEventId>,

		/// Print PDUs to the admin room (in addition to writing to file)
		#[arg(long)]
		print: bool,

		/// Show verbose output including federation request/response details
		#[arg(long)]
		verbose: bool,
	},

	/// Fetches a PDU from a remote server and attempts to verify/persist it.
	///
	/// This will fetch the PDU and all its missing ancestors from the
	/// specified server, stitching the DAG back together.
	///
	/// With --skip-auth, bypasses all auth checks and inserts the event
	/// directly into the timeline (like promote-outlier but for events
	/// that don't exist locally). Useful for recovering events that were
	/// rejected due to auth chain issues.
	FetchPdu {
		/// The room ID
		room_id: OwnedRoomId,
		/// The event ID to fetch
		event_id: OwnedEventId,
		/// The server to fetch from
		server: OwnedServerName,
		/// Skip auth checks and insert directly into timeline
		#[arg(long)]
		skip_auth: bool,
	},

	/// Re-broadcast stored read receipts for a room to all participating
	/// servers (or a specific server). Useful for recovering lost receipts
	/// after federation downtime.
	ResendReceipts {
		/// The room ID to resend receipts for.
		room_id: OwnedRoomId,

		/// Optional: only send to this specific server.
		#[arg(short, long)]
		server: Option<OwnedServerName>,
	},

	/// Repair the `unsigned` field (prev_content, prev_sender, replaces_state)
	/// for state events in a room's timeline.
	///
	/// This fixes persistent corruption where prev_content contained the
	/// event's own content instead of the actual previous state.
	RepairUnsigned {
		/// The room ID to repair.
		room_id: OwnedRoomId,
	},

	/// Compares room state. With one server, compares local state against
	/// it. With multiple servers, also compares the first server against
	/// each additional server.
	CompareRoomState {
		/// The room ID.
		room_id: OwnedRoomId,
		/// One or more servers to compare against. First server is
		/// compared against local state; additional servers are compared
		/// against the first.
		servers: Vec<OwnedServerName>,
		/// The event ID to query state at. If not provided, uses the
		/// latest local event.
		#[arg(long)]
		at_event: Option<OwnedEventId>,
		/// Drill into a specific user's membership across all servers.
		/// Shows event ID, timestamp, membership, displayname and avatar
		/// for each server.
		#[arg(long)]
		conflict: Option<OwnedUserId>,
		/// Only show counts and stats, omit the full event ID lists.
		#[arg(long)]
		summary: bool,
		/// Skip signature verification (prevents DB write contention)
		#[arg(long)]
		skip_sig_verify: bool,
	},

	/// Heals a room by rescuing local outliers, fetching genuinely missing
	/// events from federation, and optionally resyncing state.
	///
	/// By default this is a dry-run that only reports what would be done.
	/// Pass --execute to actually make changes.
	HealRoom {
		/// The room ID.
		room_id: OwnedRoomId,
		/// The server to fetch from for missing history.
		server: OwnedServerName,
		/// If set, forcefully re-processes existing timeline PDUs.
		#[arg(long)]
		nuclear: bool,
		/// Actually execute changes. Without this flag, only a dry-run
		/// summary is printed.
		#[arg(long)]
		execute: bool,
		/// If set, also resyncs room state from the remote server
		/// (Phase 5). This is expensive and usually unnecessary.
		#[arg(long)]
		resync_state: bool,
		/// If set, purges stuck outliers after healing.
		#[arg(long)]
		purge_after: bool,
	},

	/// Emergency command to re-import outliers from a JSONL file.
	ImportOutliers {
		/// The raw JSONL content.
		jsonl: String,
	},

	/// Import PDUs from a JSONL file on disk into the timeline.
	///
	/// By default, each PDU goes through the full federation pipeline:
	/// signature verification, auth checks, and state resolution.
	///
	/// Use `get-remote-dag` to create the JSONL file, then this command
	/// to import it. Run `reorder-timeline` afterwards to fix ordering.
	ImportPdus {
		/// The room ID to import into.
		room_id: OwnedRoomId,
		/// Path to the JSONL file on disk.
		path: String,
		/// Skip auth checks and state resolution (force-insert directly
		/// into the timeline, bypassing handle_incoming_pdu).
		#[arg(long)]
		skip_auth: bool,
		/// Skip signature verification on incoming PDUs.
		#[arg(long)]
		skip_sig_verify: bool,
	},

	/// Make a raw federation API request to a remote server and print/save
	/// the response. Useful for debugging and capturing test fixtures.
	///
	/// Example: debug federation-request matrix.org
	///   /_matrix/federation/v1/state/!room:server?event_id=$event
	FederationRequest {
		/// Target server name
		server_name: OwnedServerName,

		/// Federation API path (e.g. /_matrix/federation/v1/state/!room:server)
		url_path: String,

		/// Save response body to file
		#[arg(short, long)]
		output: Option<String>,
	},

	/// Find the merge-base (common ancestor) between two DAG tips and
	/// render an ASCII graph of the divergence.
	///
	/// By default, compares the local latest PDU against the remote
	/// server's latest PDU for the room. Use --event-a / --event-b to
	/// override with specific event IDs.
	DagMergeBase {
		/// The room ID.
		room_id: OwnedRoomId,
		/// Remote server to compare against. Required unless both --event-a
		/// and --event-b are provided (local-only comparison).
		#[arg(long)]
		server: Option<OwnedServerName>,
		/// Override the local tip event ID.
		#[arg(long)]
		event_a: Option<OwnedEventId>,
		/// Override the remote tip event ID (or second local tip).
		#[arg(long)]
		event_b: Option<OwnedEventId>,
		/// Maximum depth to walk before giving up (default: 500).
		#[arg(long, default_value = "500")]
		max_depth: usize,
		/// Fetch missing events from the remote server during the walk.
		/// Without this, the walk dead-ends when prev_events are missing
		/// locally. Logs each federation fetch at INFO level.
		#[arg(long)]
		federate: bool,
	},

	/// Forcefully re-resolve and set room state.
	///
	/// When called without servers, rebuilds from the local DAG.
	/// Multiple servers are merged before resolution.
	/// Delegates to the `debug` implementation.
	#[clap(alias = "force-set-room-state-from-server")]
	ForceSetState {
		/// The impacted room ID
		room_id: OwnedRoomId,
		/// Servers to query room state from. If omitted, rebuilds from
		/// the local DAG without federation.
		server_names: Vec<OwnedServerName>,
		/// The event ID of the latest known PDU in the room. Will be found
		/// automatically if not provided.
		#[arg(long = "at-event")]
		event_id: Option<OwnedEventId>,
		/// Skip signature verification AND use absolute override (shorthand
		/// for `--skip-sig-verify --absolute`)
		#[arg(short, long)]
		overwrite: bool,
		/// Skip signature verification on incoming PDUs
		#[arg(long)]
		skip_sig_verify: bool,
		/// Use remote state exclusively without merging with local state
		#[arg(long)]
		absolute: bool,
		/// Dump the raw federation state response to a JSON file
		#[arg(long)]
		output: Option<String>,
		/// Load state from a previously dumped JSON file instead of federation
		#[arg(long)]
		input: Option<String>,
		/// Show what would change without modifying state
		#[arg(long)]
		dry_run: bool,
		/// Skip per-member membership cache rebuild (fast path for bulk
		/// healing)
		#[arg(long, hide = true, default_value_t = false)]
		skip_membership_rebuild: bool,
	},

	/// Fast local-only health check across all rooms.
	///
	/// Scans every room in the database and reports:
	/// - Corrupt room IDs (non-ASCII, parse failures)
	/// - Soft-failed or missing create events
	/// - Orphaned rooms (no local users)
	/// - Extremity anomalies (0 or >10 forward extremities)
	/// - Membership cache drift (state vs cache mismatch)
	CheckRooms {
		/// Only show rooms with problems (hide healthy rooms)
		#[arg(long, short)]
		problems_only: bool,

		/// Auto-repair membership cache drift when detected
		#[arg(long)]
		fix: bool,
	},

	/// Mark or unmark event IDs as rejected in the database.
	///
	/// Rejected events are permanently excluded from state resolution.
	/// Use `compare-room-state` to identify divergent event IDs first.
	/// By default, marks events as rejected. Use --unreject to reverse.
	/// Add --soft-fail to also handle the soft-failed marker.
	#[command(alias("mark-rejected"), alias("unmark-rejected"))]
	ManageRejected {
		/// One or more event IDs to mark or unmark.
		event_ids: Vec<OwnedEventId>,

		/// Remove the rejected marker instead of adding it.
		/// Events will participate in state resolution again.
		#[arg(long)]
		unreject: bool,

		/// Also handle the soft-failed marker (in addition to rejected).
		#[arg(long)]
		soft_fail: bool,
	},

	/// Bulk-unreject all rejected events in a room.
	///
	/// Scans the timeline and outlier tree, unmarks any events flagged
	/// as rejected so they participate in state resolution again.
	/// Use --soft-fail to also clear soft-fail markers.
	#[command(name = "unreject-room")]
	UnrejectRoom {
		/// The room to scan
		room_id: OwnedRoomId,

		/// Only report the count without unrejecting
		#[arg(long)]
		dry_run: bool,

		/// Also clear soft-fail markers
		#[arg(long)]
		soft_fail: bool,
	},

	/// Batch-heal all rooms by comparing state against a backbone server.
	///
	/// For each room: compares local state with the remote server,
	/// marks any extra local events as rejected, then force-sets state
	/// from the remote server. Reports a summary when done.
	HealAllRooms {
		/// Backbone server to compare against (e.g. matrix.org)
		server: OwnedServerName,

		/// Only report divergence without making changes
		#[arg(long)]
		dry_run: bool,

		/// Maximum number of rooms to heal (0 = unlimited)
		#[arg(long, default_value = "0")]
		limit: usize,
	},

	/// Scan the database for corrupt/invalid room IDs and purge them.
	///
	/// This removes entries from serverroomids that contain non-ASCII bytes,
	/// missing colons, or other malformed data that causes SEGV on downstream
	/// parsing. Run once to clean up, then the scattered validation guards
	/// in monitor/state_cache become unnecessary.
	CleanCorruptRooms {
		/// Actually delete corrupt entries. Without this, only reports them.
		#[arg(long)]
		execute: bool,
	},
	/// Rebuild the membership cache from the current state snapshot.
	///
	/// Useful after force-set-state or when /sync is missing rooms.
	RebuildMembershipCache {
		/// The room ID to rebuild.
		room_id: OwnedRoomId,
	},

	/// Surgically override a single state event in a room's state snapshot.
	///
	/// Sets the state for a specific (type, state_key) tuple to the given
	/// event_id. The event must exist locally (timeline or outlier).
	/// Rebuilds membership cache if the event is m.room.member.
	///
	/// Example: yolo set-state-event !room:server m.room.member @user:server
	/// $eventid
	#[command(name = "set-state-event")]
	SetStateEvent {
		/// The room ID.
		room_id: OwnedRoomId,
		/// The state event type (e.g. m.room.member, m.room.power_levels).
		event_type: String,
		/// The state key (e.g. @user:server for members, empty for PLs).
		state_key: String,
		/// The event ID to set as the current state for this (type, key).
		event_id: OwnedEventId,
	},

	/// Fan out POST /get_missing_events to all room servers to recover DAG
	/// holes.
	///
	/// Uses the room's current forward extremities as the earliest boundary
	/// and fans out to all EMA-ranked room servers in parallel. Each server
	/// returns events it knows about in the gap; received events are stored
	/// as outliers and processed into the timeline. Optionally accepts a
	/// list of specific event IDs to target; otherwise targets all current
	/// room extremities.
	///
	/// Example: yolo fetch-missing-events !room:server
	#[command(name = "fetch-missing-events")]
	FetchMissingEvents {
		/// The room ID to fill gaps for.
		room_id: OwnedRoomId,

		/// Specific event IDs to request as gap targets (latest_events).
		/// Defaults to the room's current forward extremities.
		#[arg(long = "event-id")]
		event_ids: Vec<OwnedEventId>,

		/// Number of iterative rounds to run (default: 3).
		#[arg(long, default_value = "3")]
		rounds: usize,
	},

	/// Clears the global bad_event ratelimiter cache.
	///
	/// Useful after massive DAG healing operations where 404s have bloated the
	/// heap.
	ClearRatelimiter,
}
