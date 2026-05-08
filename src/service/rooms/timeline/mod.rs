mod append;
mod backfill;
mod build;
mod create;
mod data;
mod redact;

use std::{fmt::Write, sync::Arc};

use async_trait::async_trait;
pub use conduwuit_core::matrix::pdu::{PduId, RawPduId};
use conduwuit_core::{
	Result, Server, at, err, info,
	matrix::{
		event::Event,
		pdu::{PduCount, PduEvent},
	},
	utils::{MutexMap, MutexMapGuard, future::TryExtExt, stream::TryIgnore},
	warn,
};
use futures::{Future, Stream, TryStreamExt, pin_mut};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, RoomId,
	events::room::encrypted::Relation,
};
use serde::Deserialize;

use self::data::Data;
pub use self::{create::pdu_fits, data::PdusIterItem};
use crate::{
	Dep, account_data, admin, appservice, globals, pusher, rooms, sending, server_keys, users,
};

// Update Relationships
#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: Relation,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtractEventId {
	event_id: OwnedEventId,
}
#[derive(Clone, Debug, Deserialize)]
struct ExtractRelatesToEventId {
	#[serde(rename = "m.relates_to")]
	relates_to: ExtractEventId,
}

#[derive(Deserialize)]
struct ExtractBody {
	body: Option<String>,
}

pub struct Service {
	services: Services,
	db: Data,
	pub mutex_insert: RoomMutexMap,
}

struct Services {
	server: Arc<Server>,
	account_data: Dep<account_data::Service>,
	appservice: Dep<appservice::Service>,
	admin: Dep<admin::Service>,
	alias: Dep<rooms::alias::Service>,
	globals: Dep<globals::Service>,
	short: Dep<rooms::short::Service>,
	state: Dep<rooms::state::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	pdu_metadata: Dep<rooms::pdu_metadata::Service>,
	read_receipt: Dep<rooms::read_receipt::Service>,
	sending: Dep<sending::Service>,
	server_keys: Dep<server_keys::Service>,
	user: Dep<rooms::user::Service>,
	users: Dep<users::Service>,
	pusher: Dep<pusher::Service>,
	threads: Dep<rooms::threads::Service>,
	search: Dep<rooms::search::Service>,
	spaces: Dep<rooms::spaces::Service>,
	event_handler: Dep<rooms::event_handler::Service>,
	outlier: Dep<rooms::outlier::Service>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
pub type RoomMutexGuard = MutexMapGuard<OwnedRoomId, ()>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: Services {
				server: args.server.clone(),
				account_data: args.depend::<account_data::Service>("account_data"),
				appservice: args.depend::<appservice::Service>("appservice"),
				admin: args.depend::<admin::Service>("admin"),
				alias: args.depend::<rooms::alias::Service>("rooms::alias"),
				globals: args.depend::<globals::Service>("globals"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				pdu_metadata: args.depend::<rooms::pdu_metadata::Service>("rooms::pdu_metadata"),
				read_receipt: args.depend::<rooms::read_receipt::Service>("rooms::read_receipt"),
				sending: args.depend::<sending::Service>("sending"),
				server_keys: args.depend::<server_keys::Service>("server_keys"),
				user: args.depend::<rooms::user::Service>("rooms::user"),
				users: args.depend::<users::Service>("users"),
				pusher: args.depend::<pusher::Service>("pusher"),
				threads: args.depend::<rooms::threads::Service>("rooms::threads"),
				search: args.depend::<rooms::search::Service>("rooms::search"),
				spaces: args.depend::<rooms::spaces::Service>("rooms::spaces"),
				outlier: args.depend::<rooms::outlier::Service>("rooms::outlier"),
				event_handler: args
					.depend::<rooms::event_handler::Service>("rooms::event_handler"),
			},
			db: Data::new(&args),
			mutex_insert: RoomMutexMap::new(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_insert = self.mutex_insert.len();
		writeln!(out, "insert_mutex: {mutex_insert}")?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<impl Event> {
		self.first_item_in_room(room_id).await.map(at!(1))
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn first_item_in_room(&self, room_id: &RoomId) -> Result<(PduCount, impl Event)> {
		let pdus = self.pdus(room_id, None);

		pin_mut!(pdus);
		pdus.try_next()
			.await?
			.ok_or_else(|| err!(Request(NotFound("No PDU found in room"))))
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn latest_pdu_in_room(&self, room_id: &RoomId) -> Result<impl Event> {
		self.db.latest_pdu_in_room(room_id).await
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn last_timeline_count(&self, room_id: &RoomId) -> Result<PduCount> {
		self.db.last_timeline_count(room_id).await
	}

	/// Returns the `count` of this pdu's id.
	pub async fn get_pdu_count(&self, event_id: &EventId) -> Result<PduCount> {
		self.db.get_pdu_count(event_id).await
	}

	/// Returns the json of a pdu.
	pub async fn get_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
		self.db.get_pdu_json(event_id).await
	}

	#[inline]
	pub async fn remove_from_timeline(&self, event_id: &EventId) {
		self.db.remove_from_timeline(event_id).await;
	}

	#[inline]
	pub async fn reindex_timeline(&self, room_id: &RoomId) -> Result<usize> {
		self.db.reindex_timeline(room_id).await
	}

	/// Reorder the timeline for a room using topological sort.
	///
	/// Reads all PDUs, builds the DAG from `prev_events`, performs
	/// Kahn's topological sort (via `lexicographical_topological_sort`)
	/// with `origin_server_ts` as tiebreaker, then re-inserts with fresh
	/// sequential `PduCount::Normal` values. This fixes anachronisms
	/// caused by rescued outliers being appended at the end of the
	/// timeline.
	pub async fn reorder_timeline(&self, room_id: &RoomId) -> Result<usize> {
		use std::collections::{HashMap, HashSet};

		use conduwuit_core::matrix::state_res;
		use futures::future::ready;
		use ruma::events::StateEventType;

		let shortroomid = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|_| err!(Database("Room does not exist")))?;

		// Note: intentionally NOT corking the entire operation. A cork here
		// would buffer 164K+ writes (82K deletes + 82K inserts) and trigger a
		// catastrophic RocksDB compaction on flush that locks the server.

		// Collect all PDUs from the timeline
		info!("reorder_timeline: reading all PDUs from timeline...");
		let mut entries: HashMap<OwnedEventId, (PduCount, PduEvent, CanonicalJsonObject)> =
			HashMap::new();
		let mut dropped = 0_usize;
		{
			let pdus = self.pdus(room_id, None);
			pin_mut!(pdus);
			while let Some((count, pdu)) = pdus.try_next().await? {
				// Try non-outlier JSON first, fall back to any JSON (including outlier)
				let json = match self.db.get_non_outlier_pdu_json(&pdu.event_id).await {
					| Ok(json) => json,
					| Err(_) => match self.db.get_pdu_json(&pdu.event_id).await {
						| Ok(json) => {
							warn!(
								event_id = %pdu.event_id,
								"PDU in timeline had no non-outlier JSON, recovered from outlier table"
							);
							json
						},
						| Err(_) => {
							warn!(
								event_id = %pdu.event_id,
								"PDU in timeline has no JSON anywhere — cannot reorder, skipping"
							);
							dropped = dropped.saturating_add(1);
							continue;
						},
					},
				};
				let eid = pdu.event_id.clone();
				entries.insert(eid, (count, pdu, json));
				if entries.len().is_multiple_of(10000) {
					info!("reorder_timeline: read {} PDUs so far...", entries.len());
				}
			}
		}

		if dropped > 0 {
			warn!("{dropped} PDUs had no JSON and were skipped during reorder");
		}

		info!("reorder_timeline: collected {} PDUs ({dropped} dropped)", entries.len());

		if entries.is_empty() {
			return Ok(0);
		}

		// Build the DAG graph for topological sort.
		// IMPORTANT: Only include prev_events that are actually in our entries map.
		// Events referencing missing prev_events (outliers, federation gaps) would
		// otherwise get stuck with non-zero outdegree and be silently dropped from
		// the sort output — then permanently deleted from the timeline.
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> =
			HashMap::with_capacity(entries.len());
		for (event_id, (_, pdu, _)) in &entries {
			let mut parents = HashSet::new();
			for prev_id in pdu.prev_events() {
				if entries.contains_key(prev_id) {
					parents.insert(prev_id.to_owned());
				}
			}
			graph.insert(event_id.clone(), parents);
		}

		// Topological sort with origin_server_ts as tiebreaker
		info!("reorder_timeline: topological sort of {} events...", graph.len());
		let event_fetch = |event_id: OwnedEventId| {
			let ts = entries
				.get(&event_id)
				.map_or_else(|| ruma::uint!(0), |(_, p, _)| p.origin_server_ts);
			ready(Ok::<_, state_res::Error>((
				ruma::int!(0),
				ruma::MilliSecondsSinceUnixEpoch(ts),
			)))
		};

		let sorted = state_res::lexicographical_topological_sort(&graph, &event_fetch)
			.await
			.map_err(|e| err!(Database("Failed to sort timeline: {e:?}")))?;

		info!("reorder_timeline: sorted {} events, removing old entries...", sorted.len());
		// Remove old timeline entries (batched cork every 10K avoids giant WriteBatch)
		let mut cork = Some(self.db.db.cork());
		for (i, event_id) in sorted.iter().enumerate() {
			let (old_count, ..) = entries.get(event_id).expect("in sorted list");
			let old_pdu_id: RawPduId = PduId { shortroomid, shorteventid: *old_count }.into();
			self.db.remove_from_timeline_by_id(&old_pdu_id, event_id);
			if i.saturating_add(1).is_multiple_of(10000) {
				// Drop and re-cork to flush, then yield to let compaction breathe
				drop(cork.take());
				tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				cork = Some(self.db.db.cork());
				info!(
					"reorder_timeline: removed {}/{} entries...",
					i.saturating_add(1),
					sorted.len()
				);
			}
		}
		drop(cork.take());

		// Re-insert in topological order with fresh PduCount values
		let count = sorted.len();
		let batch_start = self
			.services
			.globals
			.next_count_batch(u64::try_from(count).unwrap_or(u64::MAX))?;
		info!(
			"reorder_timeline: re-inserting {count} events in order (counter range \
			 {batch_start}..{})...",
			batch_start.saturating_add(u64::try_from(count).unwrap_or(u64::MAX))
		);
		let mut cork = Some(self.db.db.cork());
		for (i, event_id) in sorted.iter().enumerate() {
			let (_, pdu, json) = entries.get(event_id).expect("in sorted list");
			let new_count = batch_start
				.saturating_add(u64::try_from(i).unwrap_or(u64::MAX))
				.saturating_add(1);
			let pdu_count = PduCount::Normal(new_count);
			let pdu_id: RawPduId = PduId { shortroomid, shorteventid: pdu_count }.into();

			self.db.append_pdu(&pdu_id, pdu, json, pdu_count).await;
			if i.saturating_add(1).is_multiple_of(10000) {
				// Drop and re-cork to flush, then yield to let compaction breathe
				drop(cork.take());
				tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				cork = Some(self.db.db.cork());
				info!("reorder_timeline: inserted {}/{count} events...", i.saturating_add(1));
			}
		}
		// Final batch: cork_and_sync ensures WAL is durable when dropped
		drop(cork.take());
		let final_sync = self.db.db.cork_and_sync();
		drop(final_sync);
		info!("reorder_timeline: re-insert complete, rebuilding shortstatehash chain...");

		// Rebuild per-PDU shortstatehashes: walk the reordered events in sequence
		// and call append_to_state for state events (which updates the room's
		// shortstatehash and records the mapping from shorteventid →
		// shortstatehash). Non-state events inherit the current room shortstatehash.
		// This fixes sync serving stale state snapshots after reorder.
		let state_lock = self.services.state.mutex.lock(room_id).await;

		// Save the room's current shortstatehash BEFORE the walk. The sequential
		// rebuild uses naive "last event wins" which can disagree with state-res.
		// We restore it after so force-set results are preserved.
		let saved_shortstatehash = self.services.state.get_room_shortstatehash(room_id).await;

		let mut state_rebuilt = 0_usize;
		for event_id in &sorted {
			let (_, pdu, _) = entries.get(event_id).expect("in sorted list");

			match self.services.state.append_to_state(pdu, room_id).await {
				| Ok(new_shortstatehash) => {
					self.services
						.state
						.set_room_state(room_id, new_shortstatehash, &state_lock);
				},
				| Err(e) => {
					warn!(
						"reorder_timeline: append_to_state failed for {event_id}: {e}; skipping \
						 shortstatehash rebuild for this event"
					);
				},
			}

			state_rebuilt = state_rebuilt.saturating_add(1);
			if state_rebuilt.is_multiple_of(10000) {
				info!(
					"reorder_timeline: rebuilt shortstatehash for {state_rebuilt}/{count} \
					 events..."
				);
			}
		}

		// Restore the room's shortstatehash to what it was before the walk.
		// The sequential rebuild gives each event a pdu_shortstatehash for
		// visibility, but the room's current state should reflect the
		// authoritative state (e.g. from force-set-room-state-from-server),
		// not the naive sequential result.
		if let Ok(ssh) = saved_shortstatehash {
			self.services
				.state
				.set_room_state(room_id, ssh, &state_lock);

			// Also update the tip event's pdu_shortstatehash so /state/
			// and compare-room-state see the correct state at the tip.
			if let Some(last_event_id) = sorted.last() {
				let shorteventid = self
					.services
					.short
					.get_or_create_shorteventid(last_event_id)
					.await;
				self.services
					.state
					.set_pdu_shortstatehash(shorteventid, ssh);
			}

			info!("reorder_timeline: restored room shortstatehash to {ssh}");
		}

		// Collapse forward extremities to just the last event in the
		// sorted timeline. This heals DAG fractures caused by
		// force-set-room-state-from-server or other admin operations
		// that scatter extremities across all state events.
		if let Some(last_event_id) = sorted.last() {
			self.services
				.state
				.set_forward_extremities(
					room_id,
					std::iter::once(last_event_id.as_ref()),
					&state_lock,
				)
				.await;

			info!("reorder_timeline: collapsed extremities to single tip: {last_event_id}");
		}

		// Rebuild membership cache from the authoritative state snapshot.
		// This fixes stale/missing entries left by previous DAG fractures.

		// Collect unique member user_ids from the timeline's m.room.member events
		let mut member_users: HashSet<ruma::OwnedUserId> = HashSet::new();
		for event_id in &sorted {
			if let Ok(pdu) = self.get_pdu(event_id).await {
				if pdu.kind.to_string() == "m.room.member" {
					if let Some(sk) = pdu.state_key() {
						if let Ok(uid) = ruma::OwnedUserId::try_from(sk) {
							member_users.insert(uid);
						}
					}
				}
			}
		}

		let mut members_synced = 0_usize;
		for user_id in &member_users {
			let Ok(pdu) = self
				.services
				.state_accessor
				.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
				.await
			else {
				continue;
			};

			let content: serde_json::Value = pdu.get_content_as_value();
			let membership = content
				.get("membership")
				.and_then(|v| v.as_str())
				.unwrap_or("leave");

			match membership {
				| "join" => {
					self.services
						.state_cache
						.mark_as_joined(user_id, room_id)
						.await;
					members_synced = members_synced.saturating_add(1);
				},
				| "leave" | "ban" => {
					self.services
						.state_cache
						.mark_as_left(user_id, room_id, None)
						.await;
					members_synced = members_synced.saturating_add(1);
				},
				| _ => {},
			}
		}
		self.services.state_cache.update_joined_count(room_id).await;
		info!("reorder_timeline: synced {members_synced} membership cache entries");

		drop(state_lock);
		info!(
			"reorder_timeline: complete, {count} events reordered, {state_rebuilt} state \
			 snapshots rebuilt"
		);

		Ok(count)
	}

	/// Returns the json of a pdu.
	#[inline]
	pub async fn get_non_outlier_pdu_json(
		&self,
		event_id: &EventId,
	) -> Result<CanonicalJsonObject> {
		self.db.get_non_outlier_pdu_json(event_id).await
	}

	/// Returns the pdu's id.
	#[inline]
	pub async fn get_pdu_id(&self, event_id: &EventId) -> Result<RawPduId> {
		self.db.get_pdu_id(event_id).await
	}

	/// Returns the pdu.
	#[inline]
	pub async fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
		self.db.get_non_outlier_pdu_in_room(None, event_id).await
	}

	/// Returns the pdu, populating room_id.
	#[inline]
	pub async fn get_non_outlier_pdu_in_room(
		&self,
		room_id: Option<&RoomId>,
		event_id: &EventId,
	) -> Result<PduEvent> {
		self.db.get_non_outlier_pdu_in_room(room_id, event_id).await
	}

	/// Checks if pdu exists directly in the timeline (non-outlier).
	#[inline]
	pub async fn non_outlier_pdu_exists(&self, event_id: &EventId) -> bool {
		self.db.non_outlier_pdu_exists(event_id).await.is_ok()
	}

	/// Checks if all PDUs exist directly in the timeline (non-outlier).
	#[inline]
	pub async fn non_outlier_pdus_exist<'a, I>(&self, event_ids: I) -> bool
	where
		I: Iterator<Item = &'a EventId> + Send,
	{
		for event_id in event_ids {
			if !self.non_outlier_pdu_exists(event_id).await {
				return false;
			}
		}
		true
	}

	/// Returns the pdu.
	///
	/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
	#[inline]
	pub async fn get_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
		self.db.get_pdu_in_room(None, event_id).await
	}

	/// Returns the pdu, populating room_id.
	///
	/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
	#[inline]
	pub async fn get_pdu_in_room(
		&self,
		room_id: Option<&RoomId>,
		event_id: &EventId,
	) -> Result<PduEvent> {
		self.db.get_pdu_in_room(room_id, event_id).await
	}

	/// Returns the pdu.
	///
	/// This does __NOT__ check the outliers `Tree`.
	#[inline]
	pub async fn get_pdu_from_id(&self, pdu_id: &RawPduId) -> Result<PduEvent> {
		self.db.get_pdu_from_id_in_room(None, pdu_id).await
	}

	/// Returns the pdu, populating room_id.
	///
	/// This does __NOT__ check the outliers `Tree`.
	#[inline]
	pub async fn get_pdu_from_id_in_room(
		&self,
		room_id: Option<&RoomId>,
		pdu_id: &RawPduId,
	) -> Result<PduEvent> {
		self.db.get_pdu_from_id_in_room(room_id, pdu_id).await
	}

	/// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
	#[inline]
	pub async fn get_pdu_json_from_id(&self, pdu_id: &RawPduId) -> Result<CanonicalJsonObject> {
		self.db.get_pdu_json_from_id(pdu_id).await
	}

	/// Checks if pdu exists
	///
	/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
	#[inline]
	pub fn pdu_exists<'a>(
		&'a self,
		event_id: &'a EventId,
	) -> impl Future<Output = bool> + Send + 'a {
		self.db.pdu_exists(event_id).is_ok()
	}

	/// Removes a pdu and creates a new one with the same id.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn replace_pdu(&self, pdu_id: &RawPduId, pdu_json: &CanonicalJsonObject) -> Result {
		self.db.replace_pdu(pdu_id, pdu_json).await
	}

	/// Returns an iterator over all PDUs in a room. Unknown rooms produce no
	/// items.
	#[inline]
	pub fn all_pdus<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = PdusIterItem> + Send + 'a {
		self.pdus(room_id, None).ignore_err()
	}

	/// Reverse iteration starting after `until`.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn pdus_rev<'a>(
		&'a self,
		room_id: &'a RoomId,
		until: Option<PduCount>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.db
			.pdus_rev(room_id, until.unwrap_or_else(PduCount::max))
	}

	/// Forward iteration starting after `from`.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn pdus<'a>(
		&'a self,
		room_id: &'a RoomId,
		from: Option<PduCount>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.db.pdus(room_id, from.unwrap_or_else(PduCount::min))
	}
}
