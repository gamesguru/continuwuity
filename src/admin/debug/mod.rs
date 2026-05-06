mod commands;
pub(crate) mod tester;

use clap::Subcommand;
use conduwuit::Result;
use ruma::{OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId};
use service::rooms::short::{ShortEventId, ShortRoomId};

use self::tester::TesterCommand;
use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub enum DebugCommand {
	/// Echo input of admin command
	Echo {
		message: Vec<String>,
	},

	/// Get the auth_chain of a PDU
	GetAuthChain {
		/// An event ID (the $ character followed by the base64 reference hash)
		event_id: OwnedEventId,
	},

	/// Parse and print a PDU from a JSON
	///
	/// The PDU event is only checked for validity and is not added to the
	/// database.
	///
	/// This command needs a JSON blob provided in a Markdown code block below
	/// the command.
	ParsePdu,

	/// Retrieve and print a PDU by EventID from the Continuwuity database
	GetPdu {
		/// An event ID (a $ followed by the base64 reference hash)
		event_id: OwnedEventId,
	},

	/// Attempts to "rescue" an outlier PDU by upgrading it to a timeline event.
	///
	/// This will perform all necessary auth checks and state resolution.
	RescuePdu {
		/// An event ID (a $ followed by the base64 reference hash)
		event_id: OwnedEventId,

		/// If set, bypasses strict auth checks.
		#[arg(short, long)]
		force: bool,

		/// If set, skips the soft-fail check against current room state.
		/// Use for late-arriving events that are valid at their DAG position
		/// but conflict with current state.
		#[arg(long)]
		skip_soft_fail: bool,
	},

	/// List all outlier PDUs in our database.
	ListOutliers {
		/// Filter outliers to a specific room
		#[arg(short, long)]
		room_id: Option<OwnedRoomOrAliasId>,

		/// Filter outliers to a specific sender
		#[arg(short, long)]
		sender: Option<OwnedUserId>,

		/// Limit the number of outliers listed.
		#[arg(short, long)]
		limit: Option<usize>,
	},

	/// View the current forward extremities (timeline tips) of a room.
	ViewExtremities {
		/// The room ID or alias.
		room: OwnedRoomOrAliasId,
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
	},

	/// Purge outlier PDUs that already exist in our timeline.
	///
	/// This is a safe cleanup command that resolves "stuck" state where an
	/// event exists in both the timeline and outlier tables. It will NOT
	/// delete outliers that haven't been rescued yet.
	#[command(alias("purge-stuck"))]
	PurgeOutliers {
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

	/// Emergency command to re-import outliers from a JSONL file.
	ImportOutliers {
		/// The raw JSONL content.
		jsonl: String,
	},

	/// Compares local room state with a remote server.
	CompareRoomState {
		/// The room ID.
		room_id: OwnedRoomId,
		/// The server to compare with.
		server: OwnedServerName,
		/// The event ID to query state at. If not provided, uses the latest
		/// local event.
		#[arg(long)]
		at_event: Option<OwnedEventId>,
	},

	/// Compares room state between two different remote servers.
	CompareRemoteState {
		/// The room ID.
		room_id: OwnedRoomId,
		/// The first server to compare.
		server1: OwnedServerName,
		/// The second server to compare.
		server2: OwnedServerName,
		/// The event ID to use as a reference point. Will be found
		/// automatically if not provided.
		event_id: Option<OwnedEventId>,
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

	/// Purge a specific outlier event by event ID.
	PurgeOutlier {
		/// The event ID to purge.
		event_id: OwnedEventId,
	},

	/// Get the room DAG as a list of PDUs in a range.
	GetRoomDag {
		/// Room ID
		room_id: OwnedRoomOrAliasId,

		/// Start index (0-based)
		start: u64,

		/// End index (0-based, inclusive, or -1 for all)
		end: i64,
	},

	/// Fetch a room's DAG from a remote server via federation backfill API
	/// and write it to a JSONL file.
	GetRemoteDag {
		/// Room ID
		room_id: OwnedRoomId,

		/// Remote server to fetch from
		server: OwnedServerName,

		/// Maximum number of events to fetch (default: 100)
		#[arg(long, default_value = "100")]
		limit: usize,
	},

	/// Retrieve and print a PDU by PduId from the Continuwuity database
	GetShortPdu {
		/// Shortroomid integer
		shortroomid: ShortRoomId,

		/// Shorteventid integer
		shorteventid: ShortEventId,
	},

	/// Attempts to retrieve a PDU from a remote server. **Does not** insert
	///   it into the database
	/// or persist it anywhere.
	GetRemotePdu {
		/// An event ID (a $ followed by the base64 reference hash)
		event_id: OwnedEventId,

		/// Argument for us to attempt to fetch the event from the
		/// specified remote server.
		server: OwnedServerName,
	},

	/// Same as `get-remote-pdu` but accepts a codeblock newline delimited
	///   list of PDUs and a single server to fetch from
	GetRemotePduList {
		/// Argument for us to attempt to fetch all the events from the
		/// specified remote server.
		server: OwnedServerName,

		/// If set, ignores errors, else stops at the first error/failure.
		#[arg(short, long)]
		force: bool,
	},

	/// Gets all the room state events for the specified room.
	///
	/// This is functionally equivalent to `GET
	/// /_matrix/client/v3/rooms/{roomid}/state`, except the admin command does
	/// *not* check if the sender user is allowed to see state events. This is
	/// done because it's implied that server admins here have database access
	/// and can see/get room info themselves anyways if they were malicious
	/// admins.
	///
	/// Of course the check is still done on the actual client API.
	GetRoomState {
		/// Room ID
		room_id: OwnedRoomOrAliasId,
	},

	/// Get and display signing keys from local cache or remote server.
	GetSigningKeys {
		server_name: Option<OwnedServerName>,

		#[arg(long)]
		notary: Option<OwnedServerName>,

		#[arg(short, long)]
		query: bool,
	},

	/// Get and display signing keys from local cache or remote server.
	GetVerifyKeys {
		server_name: Option<OwnedServerName>,
	},

	/// Sends a federation request to the remote server's
	///   `/_matrix/federation/v1/version` endpoint and measures the latency it
	///   took for the server to respond
	Ping {
		server: OwnedServerName,
	},

	/// Forces device lists for all local and remote users to be updated (as
	///   having new keys available)
	ForceDeviceListUpdates,

	/// Change tracing log level/filter on the fly
	///
	/// This accepts the same format as the `log` config option.
	ChangeLogLevel {
		/// Log level/filter
		filter: Option<String>,

		/// Resets the log level/filter to the one in your config
		#[arg(short, long)]
		reset: bool,
	},

	/// Verify JSON signatures
	///
	/// This command needs a JSON blob provided in a Markdown code block below
	/// the command.
	VerifyJson,

	/// Verify PDU
	///
	/// This re-verifies a PDU existing in the database found by ID.
	VerifyPdu {
		event_id: OwnedEventId,
	},

	/// Fetches a PDU from a remote server and attempts to verify/persist it.
	///
	/// This will fetch the PDU and all its missing ancestors from the
	/// specified server, stitching the DAG back together.
	FetchPdu {
		/// The room ID
		room_id: OwnedRoomId,
		/// The event ID to fetch
		event_id: OwnedEventId,
		/// The server to fetch from
		server: OwnedServerName,
	},

	/// Prints the very first PDU in the specified room (typically
	///   m.room.create)
	FirstPduInRoom {
		/// The room ID
		room_id: OwnedRoomId,
	},

	/// Prints the latest ("last") PDU in the specified room (typically a
	///   message)
	LatestPduInRoom {
		/// The room ID
		room_id: OwnedRoomId,
	},

	/// Forcefully replaces the room state of our local copy of the specified
	///   room, with the copy (auth chain and room state events) the specified
	///   remote server says.
	///
	/// A common desire for room deletion is to simply "reset" our copy of the
	/// room. While this admin command is not a replacement for that, if you
	/// know you have split/broken room state and you know another server in the
	/// room that has the best/working room state, this command can let you use
	/// their room state. Such example is your server saying users are in a
	/// room, but other servers are saying they're not in the room in question.
	///
	/// This command will get the latest PDU in the room we know about, and
	/// request the room state at that point in time via
	/// `/_matrix/federation/v1/state/{roomId}`.
	ForceSetRoomStateFromServer {
		/// The impacted room ID
		room_id: OwnedRoomId,
		/// The server we will use to query the room state for
		server_name: OwnedServerName,
		/// The event ID of the latest known PDU in the room. Will be found
		/// automatically if not provided.
		event_id: Option<OwnedEventId>,
		#[arg(short, long)]
		overwrite: bool,
		/// Dump the raw federation state response to a JSON file
		#[arg(long)]
		output: Option<String>,
	},

	/// Runs a server name through Continuwuity's true destination resolution
	///   process
	///
	/// Useful for debugging well-known issues
	ResolveTrueDestination {
		server_name: OwnedServerName,

		#[arg(short, long)]
		no_cache: bool,
	},

	/// Print extended memory usage
	///
	/// Optional argument is a character mask (a sequence of characters in any
	/// order) which enable additional extended statistics. Known characters are
	/// "abdeglmx". For convenience, a '*' will enable everything.
	MemoryStats {
		opts: Option<String>,
	},

	/// Print general tokio runtime metric totals.
	RuntimeMetrics,

	/// Print detailed tokio runtime metrics accumulated since last command
	///   invocation.
	RuntimeInterval,

	/// Print the current time
	Time,

	/// Get database statistics
	DatabaseStats {
		property: Option<String>,

		#[arg(short, long, alias("column"))]
		map: Option<String>,
	},

	/// Trim memory usage
	TrimMemory,

	/// List database files
	DatabaseFiles {
		map: Option<String>,

		#[arg(long)]
		level: Option<i32>,
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

	/// Send a test email to the invoking admin's email address
	SendTestEmail,

	/// Developer test stubs
	#[command(subcommand)]
	#[allow(non_snake_case)]
	#[clap(hide(true))]
	Tester(TesterCommand),
}
