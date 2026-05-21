mod append;
mod backfill;
mod build;
mod create;
mod data;
mod redact;
mod repair_unsigned;

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
use futures::{Future, Stream, StreamExt, TryStreamExt, pin_mut};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, RoomId,
	events::{TimelineEventType, room::encrypted::Relation},
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
	pub mutex_fetch: MutexMap<OwnedEventId, ()>,
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
	state_compressor: Dep<rooms::state_compressor::Service>,
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
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
				event_handler: args
					.depend::<rooms::event_handler::Service>("rooms::event_handler"),
			},
			db: Data::new(&args),
			mutex_insert: RoomMutexMap::new(),
			mutex_fetch: MutexMap::new(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_insert = self.mutex_insert.len();
		writeln!(out, "insert_mutex: {mutex_insert}")?;
		let mutex_fetch = self.mutex_fetch.len();
		writeln!(out, "fetch_mutex: {mutex_fetch}")?;

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
}

struct HealerSuppressGuard<'a> {
	globals: &'a globals::Service,
	room_id: OwnedRoomId,
}

impl Drop for HealerSuppressGuard<'_> {
	fn drop(&mut self) { self.globals.suppress_healer.remove(&self.room_id); }
}

impl Service {
	/// Reorder the timeline for a room using topological sort.
	///
	/// Reads all PDUs, builds the DAG from `prev_events`, performs
	/// Kahn's topological sort (via `lexicographical_topological_sort`)
	/// with `origin_server_ts` as tiebreaker, then re-inserts with fresh
	/// sequential `PduCount::Normal` values. This fixes anachronisms
	/// caused by rescued outliers being appended at the end of the
	/// timeline.
	pub async fn reorder_timeline(&self, room_id: &RoomId, tail: Option<usize>) -> Result<usize> {
		use std::collections::{HashMap, HashSet};

		use conduwuit_core::matrix::state_res;
		use futures::future::ready;
		use ruma::events::StateEventType;

		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
		let state_lock = self.services.state.mutex.lock(room_id).await;

		self.services
			.globals
			.suppress_healer
			.insert(room_id.to_owned());
		let _suppress_guard = HealerSuppressGuard {
			globals: &self.services.globals,
			room_id: room_id.to_owned(),
		};

		// Note: intentionally NOT corking the entire operation. A cork here
		// would buffer 164K+ writes (82K deletes + 82K inserts) and trigger a
		// catastrophic RocksDB compaction on flush that locks the server.

		// Collect PDUs from the timeline — either all (full reorder) or last N (tail)
		info!("reorder_timeline: reading all PDUs from timeline...");
		let mut entries: HashMap<OwnedEventId, (PduCount, PduEvent, CanonicalJsonObject)> =
			HashMap::new();
		let mut dropped = 0_usize;
		let mut tail_min_count: Option<PduCount> = None;

		if let Some(limit) = tail {
			info!("reorder_timeline: reading last {limit} PDUs from timeline (tail mode)...");
			// Collect in reverse and record the minimum count seen (oldest in window)
			let mut min_count = PduCount::max();
			let mut rev = Box::pin(self.pdus_rev(room_id, None));
			let mut collected = 0_usize;
			while let Some((count, pdu)) = rev.try_next().await? {
				if collected >= limit {
					break;
				}
				let json = match self.db.get_non_outlier_pdu_json(&pdu.event_id).await {
					| Ok(j) => j,
					| Err(_) => match self.db.get_pdu_json(&pdu.event_id).await {
						| Ok(j) => j,
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
				if count < min_count {
					min_count = count;
				}
				entries.insert(pdu.event_id.clone(), (count, pdu, json));
				collected = collected.saturating_add(1);
			}
			if min_count != PduCount::max() {
				tail_min_count = Some(min_count);
			}
		} else {
			info!("reorder_timeline: reading all PDUs from timeline...");
			let pdus_backfill = self.pdus(room_id, Some(PduCount::min()));
			let pdus_normal = self.pdus(room_id, Some(PduCount::Normal(0)));
			let pdus = pdus_backfill.chain(pdus_normal);
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
			let (old_count, pdu, ..) = entries.get(event_id).expect("in sorted list");
			let old_pdu_id: RawPduId = PduId { shortroomid, shorteventid: *old_count }.into();
			// Deindex old pdu_id from search before removal
			if pdu.kind == TimelineEventType::RoomMessage {
				if let Ok(content) = pdu.get_content::<ExtractBody>() {
					if let Some(body) = &content.body {
						self.services
							.search
							.deindex_pdu(shortroomid, &old_pdu_id, body);
					}
				}
			}
			self.db.remove_from_timeline_by_id(&old_pdu_id, event_id);
			if i.saturating_add(1).is_multiple_of(2000) {
				info!(
					"reorder_timeline: removed {}/{} entries...",
					i.saturating_add(1),
					sorted.len()
				);
			}
			if i.saturating_add(1).is_multiple_of(10000) {
				// Drop and re-cork to flush, then yield to let compaction breathe
				drop(cork.take());
				tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				cork = Some(self.db.db.cork());
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
			// Re-index search with new pdu_id
			if pdu.kind == TimelineEventType::RoomMessage {
				if let Ok(content) = pdu.get_content::<ExtractBody>() {
					if let Some(body) = &content.body {
						self.services.search.index_pdu(shortroomid, &pdu_id, body);
					}
				}
			}
			if i.saturating_add(1).is_multiple_of(2000) {
				info!("reorder_timeline: inserted {}/{count} events...", i.saturating_add(1));
			}
			if i.saturating_add(1).is_multiple_of(10000) {
				// Flush batch and yield so the executor can handle other work.
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
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

		// Save the room's current shortstatehash BEFORE the walk. The sequential
		// rebuild uses naive "last event wins" which can disagree with state-res.
		// We restore it after so force-set results are preserved.
		let saved_shortstatehash = self.services.state.get_room_shortstatehash(room_id).await;

		// Compute the FOUNDATION state: the state BEFORE the oldest timeline
		// event. This is the correct starting point for the state walk.
		//
		// Strategy (in priority order):
		// 1. Use pdu_shortstatehash from the oldest event's prev_events. These are
		//    outliers whose hashes were never modified by reorder-timeline, so they
		//    give the correct pre-timeline state.
		// 2. If prev_events don't have SSH (never-processed outliers), fall back to
		//    subtracting timeline state events from the full state. This works when all
		//    state changes for a key are within the timeline, but can drop keys that
		//    have pre-timeline values.
		// 3. If prev_events is empty (m.room.create), use empty state.
		//
		// Without this seeding, the walk starts from the room's tip state
		// and every historical event inherits the full future state
		// ("Time-Travel State Bug").
		if let Some(oldest_event_id) = sorted.first() {
			let mut foundation_set = false;

			// Tail mode: the foundation is simply the event immediately before the
			// window boundary — look it up via pdus_rev starting just below min_count.
			if let Some(min_count) = tail_min_count {
				if let Some((_, pre_pdu)) = Box::pin(self.pdus_rev(room_id, Some(min_count)))
					.try_next()
					.await?
				{
					if let Ok(ssh) = self
						.services
						.state_accessor
						.pdu_shortstatehash(&pre_pdu.event_id)
						.await
					{
						if ssh != 0 {
							self.services
								.state
								.set_room_state(room_id, ssh, &state_lock);
							info!(
								"reorder_timeline: tail: seeded walk from pre-window event {} \
								 (SSH {ssh})",
								pre_pdu.event_id
							);
							foundation_set = true;
						}
					}
				}
			}

			if !foundation_set {
				if let Some((_, oldest_pdu, _)) = entries.get(oldest_event_id) {
					let prev_events: Vec<_> = oldest_pdu.prev_events().collect();

					if prev_events.is_empty() {
						// No prev_events → m.room.create → empty foundation.
						// save_state with empty set creates a new SSH.
						let empty = rooms::state_compressor::CompressedState::new();
						if let Ok(result) = self
							.services
							.state_compressor
							.save_state(room_id, Arc::new(empty))
							.await
						{
							self.services.state.set_room_state(
								room_id,
								result.shortstatehash,
								&state_lock,
							);
							info!(
								"reorder_timeline: seeded walk from empty foundation \
								 (m.room.create, no prev_events)"
							);
							foundation_set = true;
						}
					} else {
						// Try each prev_event for an uncorrupted pdu_shortstatehash.
						// These are outliers — reorder-timeline never touches their
						// SSH, so they're guaranteed uncontaminated.
						for prev_id in prev_events {
							if let Ok(ssh) = self
								.services
								.state_accessor
								.pdu_shortstatehash(prev_id)
								.await
							{
								self.services
									.state
									.set_room_state(room_id, ssh, &state_lock);
								info!(
									"reorder_timeline: seeded walk from prev_event {prev_id} \
									 SSH {ssh}"
								);
								foundation_set = true;
								break;
							}
						}
					}
				}
			}

			// Safe fallback: try the oldest event's own pdu_shortstatehash.
			// If the room was never previously reordered, this is pristine.
			if !foundation_set {
				if let Ok(ssh) = self
					.services
					.state_accessor
					.pdu_shortstatehash(oldest_event_id)
					.await
				{
					self.services
						.state
						.set_room_state(room_id, ssh, &state_lock);
					info!("reorder_timeline: seeded walk from oldest event SSH {ssh} (fallback)");
					foundation_set = true;
				}
			}

			// Last-resort fallback: subtract timeline state events from full state.
			// Works when all state changes for a key originate in the
			// timeline (locally-created rooms, single-join federated
			// rooms). Can drop keys that have outlier-only pre-timeline
			// values — but this is still better than the tip state.
			if !foundation_set {
				if let Ok(ssh) = saved_shortstatehash {
					// Collect shorteventids for all state events in the timeline
					let mut timeline_shorteventids: HashSet<u64> = HashSet::new();
					for event_id in &sorted {
						if let Some((_, pdu, _)) = entries.get(event_id) {
							if pdu.state_key.is_some() {
								let seid = self
									.services
									.short
									.get_or_create_shorteventid(&pdu.event_id)
									.await;
								timeline_shorteventids.insert(seid);
							}
						}
					}

					// Load the full state and filter out timeline state events
					let state_info_result = self
						.services
						.state_compressor
						.load_shortstatehash_info(ssh)
						.await;

					if let Ok(state_info) = state_info_result {
						let full_state_opt =
							state_info.last().and_then(|info| info.full_state.clone());

						if let Some(full_state) = full_state_opt {
							use rooms::state_compressor::parse_compressed_state_event;

							let foundation: rooms::state_compressor::CompressedState = full_state
								.iter()
								.filter(|entry| {
									let (_ssk, seid) = parse_compressed_state_event(**entry);
									!timeline_shorteventids.contains(&seid)
								})
								.copied()
								.collect();

							if let Ok(result) = self
								.services
								.state_compressor
								.save_state(room_id, Arc::new(foundation))
								.await
							{
								self.services.state.set_room_state(
									room_id,
									result.shortstatehash,
									&state_lock,
								);
								info!(
									"reorder_timeline: seeded walk from subtracted foundation \
									 state {} ({} timeline state events removed, fallback)",
									result.shortstatehash,
									timeline_shorteventids.len()
								);
							}
						}
					}
				}
			}
		}

		let foundation_ssh = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.ok();
		let mut state_rebuilt = 0_usize;
		let mut walk_ssh: Option<u64> = foundation_ssh;
		for event_id in &sorted {
			let (_, pdu, _) = entries.get(event_id).expect("in sorted list");

			match self.services.state.append_to_state(pdu, room_id).await {
				| Ok(new_ssh) =>
					if walk_ssh != Some(new_ssh) {
						self.services
							.state
							.set_room_state(room_id, new_ssh, &state_lock);
						walk_ssh = Some(new_ssh);
					},
				| Err(e) => {
					warn!(
						"reorder_timeline: append_to_state failed for {event_id}: {e}; skipping \
						 shortstatehash rebuild for this event"
					);
				},
			}

			state_rebuilt = state_rebuilt.saturating_add(1);
			if state_rebuilt.is_multiple_of(5000) {
				info!(
					"reorder_timeline: rebuilt shortstatehash for {state_rebuilt}/{count} \
					 events..."
				);
				tokio::task::yield_now().await;
			}
		}

		// Post-walk invariant check: verify SSH diversity.
		// If there were state events but the SSH never changed, the walk
		// failed to advance state properly (regression guard).
		let state_event_count = sorted
			.iter()
			.filter(|eid| {
				entries
					.get(*eid)
					.is_some_and(|(_, pdu, _)| pdu.state_key.is_some())
			})
			.count();

		if state_event_count > 1 && walk_ssh == foundation_ssh {
			// walk_ssh never diverged from the foundation — every state
			// event produced the same SSH it started with. This should be
			// impossible unless append_to_state is broken.
			warn!(
				"reorder_timeline: INVARIANT VIOLATION — {state_event_count} state events but \
				 SSH never advanced from foundation. Per-event state snapshots may be incorrect."
			);
		} else {
			info!(
				"reorder_timeline: walk complete — {state_rebuilt} events, {state_event_count} \
				 state events processed"
			);
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

			// NOTE: Do NOT overwrite the tip event's pdu_shortstatehash here.
			// pdu_shortstatehash stores the state BEFORE the event (per spec).
			// The room's SSH is the state AFTER all events. These must diverge.

			info!("reorder_timeline: restored room shortstatehash to {ssh}");
		}

		// Calculate the true DAG forward extremities using the extracted pure function.
		// This preserves rescued state events (which have no children locally) as
		// extremities so they can naturally merge into the DAG via state resolution.
		let true_extremities = calculate_true_extremities(&graph, &sorted);

		if !true_extremities.is_empty() {
			self.services
				.state
				.set_forward_extremities(room_id, true_extremities.iter().copied(), &state_lock)
				.await;

			info!(
				"reorder_timeline: set forward extremities to {} true DAG tips",
				true_extremities.len()
			);
		}

		// Repair unsigned.prev_content JSON values which may have been missed during
		// DAG holes
		if let Err(e) = self.repair_room_unsigned(room_id).await {
			warn!("reorder_timeline: failed to repair unsigned payload values: {e}");
		}

		// Rebuild membership cache from the authoritative state snapshot.
		// This fixes stale/missing entries left by previous DAG fractures.
		let mut members_synced = 0_usize;
		let mut state_joined: HashSet<ruma::OwnedUserId> = HashSet::new();
		let mut state_invited: HashSet<ruma::OwnedUserId> = HashSet::new();

		// Single pass over state snapshot — check-before-write avoids
		// redundant DB writes for users whose cache is already correct.
		let state_full = self.services.state_accessor.state_full(
			self.services
				.state
				.get_room_shortstatehash(room_id)
				.await
				.unwrap_or_default(),
		);
		let mut state_full = std::pin::pin!(state_full);
		while let Some(((event_type, state_key), pdu)) = state_full.next().await {
			if event_type != StateEventType::RoomMember {
				continue;
			}
			let Ok(uid) = ruma::OwnedUserId::try_from(state_key.as_str()) else {
				continue;
			};

			let content: serde_json::Value = pdu.get_content_as_value();
			let membership = content
				.get("membership")
				.and_then(|v| v.as_str())
				.unwrap_or("leave");

			match membership {
				| "join" => {
					state_joined.insert(uid.clone());
					if !self.services.state_cache.is_joined(&uid, room_id).await {
						self.services
							.state_cache
							.mark_as_joined_silent(&uid, room_id)
							.await;
						members_synced = members_synced.saturating_add(1);
					}
				},
				| "invite" => {
					state_invited.insert(uid.clone());
					// mark_as_invited requires sender; skip cache update for
					// invites here — update_joined_count will reconcile.
				},
				| _ => {
					if self
						.services
						.state_cache
						.is_invited_or_joined(&uid, room_id)
						.await
					{
						self.services
							.state_cache
							.mark_as_left_silent(&uid, room_id)
							.await;
						members_synced = members_synced.saturating_add(1);
					}
				},
			}
		}

		// Sweep stale joined cache entries
		let cached_members: Vec<ruma::OwnedUserId> = self
			.services
			.state_cache
			.room_members(room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		let mut stale_removed = 0_usize;
		for user_id in &cached_members {
			// Symmetric guard: only purge if they are neither joined NOR invited.
			if !state_joined.contains(user_id) && !state_invited.contains(user_id) {
				self.services
					.state_cache
					.mark_as_left_silent(user_id, room_id)
					.await;
				stale_removed = stale_removed.saturating_add(1);
			}
		}

		// Sweep stale invited cache entries
		let cached_invited: Vec<ruma::OwnedUserId> = self
			.services
			.state_cache
			.room_members_invited(room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		for user_id in &cached_invited {
			// Only purge if they are neither invited NOR joined.
			// If they transitioned to joined, mark_as_left would accidentally nuke their
			// valid join.
			if !state_invited.contains(user_id) && !state_joined.contains(user_id) {
				self.services
					.state_cache
					.mark_as_left_silent(user_id, room_id)
					.await;
				stale_removed = stale_removed.saturating_add(1);
			}
		}

		self.services.state_cache.update_joined_count(room_id).await;
		info!(
			"reorder_timeline: synced {members_synced} membership cache entries, removed \
			 {stale_removed} stale"
		);

		drop(state_lock);
		info!(
			"reorder_timeline: complete, {count} events reordered, {state_rebuilt} state \
			 snapshots rebuilt"
		);

		Ok(count)
	}

	/// Automatically recalculates the true topological DAG forward extremities
	/// by querying the last `tail` events from the room's timeline and
	/// analyzing their `prev_events` graph to find all nodes with out-degree
	/// 0. Optionally overwrites the stored forward extremities if `update_db`
	///    is true.
	/// Returns true if the extremities were changed (or would be changed).
	#[tracing::instrument(skip(self), level = "info")]
	pub async fn recalculate_extremities(
		&self,
		room_id: &RoomId,
		tail: usize,
		update_db: bool,
	) -> Result<bool> {
		use std::collections::{HashMap, HashSet};

		use futures::StreamExt;
		use ruma::OwnedEventId;

		let state_lock = self.services.state.mutex.lock(room_id).await;

		let mut pdus = Vec::with_capacity(tail);
		let mut graph = HashMap::with_capacity(tail);
		let mut sorted = Vec::with_capacity(tail);

		let mut stream = std::pin::pin!(self.pdus_rev(room_id, None));
		while let Some(Ok((_count, pdu))) = stream.next().await {
			// ALGORITHMIC DAG HEALING:
			// Do NOT include soft-failed events in the topological graph.
			// By ignoring them, their parents will naturally have out-degree 0
			// and become the true DAG tips, burying the soft-failed event.
			if self
				.services
				.pdu_metadata
				.is_event_soft_failed(&pdu.event_id)
				.await
			{
				continue;
			}

			pdus.push(pdu);
			if pdus.len() >= tail {
				break;
			}
		}

		// pdus_rev returns newest first. We need oldest for true_extremities
		pdus.reverse();

		for pdu in pdus {
			let event_id = pdu.event_id.clone();
			let prev_events: HashSet<OwnedEventId> = pdu.prev_events.iter().cloned().collect();
			graph.insert(event_id.clone(), prev_events);
			sorted.push(event_id);
		}

		let true_extremities = calculate_true_extremities(&graph, &sorted);

		let current_extremities = self.services.state.get_forward_extremities(room_id);
		let current_set: HashSet<_> = current_extremities.map(ToOwned::to_owned).collect().await;
		let new_set: HashSet<_> = true_extremities.iter().map(|e| (*e).to_owned()).collect();

		if current_set == new_set {
			return Ok(false);
		}

		if update_db {
			// STRICT OVERWRITE: Erases phantom tips that fell out of the window.
			// set_forward_extremities enforces MAX_FORWARD_EXTREMITIES cap.
			self.services
				.state
				.set_forward_extremities(room_id, true_extremities.into_iter(), &state_lock)
				.await;
		}

		Ok(true)
	}

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

pub fn calculate_true_extremities<'a, S1, S2>(
	graph: &std::collections::HashMap<
		OwnedEventId,
		std::collections::HashSet<OwnedEventId, S2>,
		S1,
	>,
	sorted: &'a [OwnedEventId],
) -> Vec<&'a EventId>
where
	S1: std::hash::BuildHasher,
	S2: std::hash::BuildHasher,
{
	let mut has_children: std::collections::HashSet<OwnedEventId> =
		std::collections::HashSet::new();
	for parents in graph.values() {
		for parent in parents {
			has_children.insert(parent.clone());
		}
	}

	let mut true_extremities: Vec<&EventId> = sorted
		.iter()
		.filter(|eid| !has_children.contains(*eid))
		.map(AsRef::as_ref)
		.collect();

	if true_extremities.is_empty() {
		if let Some(last_event_id) = sorted.last() {
			true_extremities.push(last_event_id.as_ref());
		}
	}

	true_extremities
}

pub fn update_unsigned_prev_content(
	pdu_json: &mut CanonicalJsonObject,
	prev_state: &PduEvent,
) -> Result<()> {
	let unsigned = pdu_json.entry("unsigned".to_owned()).or_insert_with(|| {
		ruma::CanonicalJsonValue::Object(std::collections::BTreeMap::default())
	});

	if let ruma::CanonicalJsonValue::Object(unsigned) = unsigned {
		// Idempotently remove old (possibly wrong/missing) fields
		unsigned.remove("prev_content");
		unsigned.remove("prev_sender");
		unsigned.remove("replaces_state");

		let prev_content_value = prev_state.get_content_as_value();
		unsigned.insert(
			"prev_content".to_owned(),
			ruma::CanonicalJsonValue::Object(
				conduwuit_core::utils::to_canonical_object(prev_content_value).map_err(|e| {
					conduwuit::err!(Database(error!(
						"Failed to convert prev_state to canonical JSON: {e}"
					)))
				})?,
			),
		);
		unsigned.insert(
			"prev_sender".to_owned(),
			ruma::CanonicalJsonValue::String(prev_state.sender().to_string()),
		);
		unsigned.insert(
			"replaces_state".to_owned(),
			ruma::CanonicalJsonValue::String(prev_state.event_id().to_string()),
		);
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use ruma::event_id;

	use super::*;

	#[test]
	fn test_calculate_true_extremities_00_single_tip() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());

		let sorted = vec![a.clone(), b.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![b.as_ref()];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_01_fork() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![a.clone()].into_iter().collect());

		let sorted = vec![a.clone(), b.clone(), c.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![b.as_ref(), c.as_ref()];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_02_diamond() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();
		let d = event_id!("$d").to_owned();
		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();

		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(d.clone(), vec![b.clone(), c.clone()].into_iter().collect());

		let sorted = vec![a, b, c, d.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![&*d];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_03_islands() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let x = event_id!("$x").to_owned();
		let y = event_id!("$y").to_owned();
		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();

		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(y.clone(), vec![x.clone()].into_iter().collect());

		let sorted = vec![a.clone(), b.clone(), x.clone(), y.clone()];
		let mut tips = calculate_true_extremities(&graph, &sorted);
		tips.sort();

		let mut expected: Vec<&EventId> = vec![&*b, &*y];
		expected.sort();

		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_04_missing_parents() {
		let a = event_id!("$a").to_owned();
		let z = event_id!("$z").to_owned(); // not in sorted, but referenced
		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();

		graph.insert(a.clone(), vec![z.clone()].into_iter().collect());

		let sorted = vec![a.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);
		let expected: Vec<&EventId> = vec![&*a];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_05_missing_from_graph() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		// Graph only knows about A's parents (none). B is omitted from the map
		// entirely.
		graph.insert(a.clone(), std::collections::HashSet::new());

		let sorted = vec![a.clone(), b.clone()];
		let mut tips = calculate_true_extremities(&graph, &sorted);

		// Because B is in `sorted` and nothing in `graph` lists B as a parent, B must
		// be a tip. A is also a tip because nothing lists it as a parent.
		tips.sort();
		let mut expected: Vec<&EventId> = vec![&*a, &*b];
		expected.sort();

		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_06_cycle_fallback() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(a.clone(), vec![b.clone()].into_iter().collect());

		let sorted = vec![a.clone(), b.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);

		// Fallback returns the last element in `sorted`
		let expected: Vec<&EventId> = vec![&*b];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_07_cap() {
		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		let mut sorted = Vec::new();
		let root = event_id!("$root").to_owned();
		sorted.push(root.clone());

		for i in 0..25 {
			let id: OwnedEventId = format!("$tip{i}").try_into().unwrap();
			graph.insert(id.clone(), vec![root.clone()].into_iter().collect());
			sorted.push(id);
		}

		let tips = calculate_true_extremities(&graph, &sorted);
		assert_eq!(tips.len(), 20);
		assert_eq!(tips[0].as_str(), "$tip24");
		assert_eq!(tips[19].as_str(), "$tip5");
	}

	#[test]
	fn test_calculate_true_extremities_08_empty_input() {
		let graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		let sorted = vec![];
		let tips = calculate_true_extremities(&graph, &sorted);
		assert!(tips.is_empty(), "Empty graph should return empty extremities");
	}

	#[test]
	fn test_calculate_true_extremities_09_extraneous_graph_data() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let old = event_id!("$old").to_owned();
		let older = event_id!("$older").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();

		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		// Extraneous data outside the 'sorted' window
		graph.insert(old.clone(), vec![older.clone()].into_iter().collect());

		let sorted = vec![a.clone(), b.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);

		let expected: Vec<&EventId> = vec![&*b];
		assert_eq!(tips, expected);
	}

	#[test]
	fn test_calculate_true_extremities_10_out_of_order() {
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();

		// Linear chain: A -> B -> C
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![b.clone()].into_iter().collect());

		// Array is passed in completely scrambled chronological order
		let sorted = vec![c.clone(), a.clone(), b.clone()];
		let tips = calculate_true_extremities(&graph, &sorted);

		// Even though C was first in the array, A and B are in has_children.
		// The algorithm correctly identifies C as the sole extremity.
		let expected: Vec<&EventId> = vec![&*c];
		assert_eq!(tips, expected);
	}
}
