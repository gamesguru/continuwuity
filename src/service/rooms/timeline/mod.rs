mod append;
mod backfill;
mod build;
mod create;
mod data;
pub mod extremities;
mod heal;
mod metadata;
mod redact;
mod repair_unsigned;
use std::{fmt::Write, sync::Arc};

use async_trait::async_trait;
pub use conduwuit_core::matrix::pdu::{PduId, RawPduId, ShortRoomId};
use conduwuit_core::{
	Result, Server, SyncMutex, at, debug, err, info,
	matrix::{
		event::Event,
		pdu::{PduCount, PduEvent},
	},
	utils::{MutexMap, MutexMapGuard, future::TryExtExt, stream::TryIgnore},
	warn,
};
use futures::{Future, Stream, StreamExt, TryStreamExt, pin_mut};
use lru_cache::LruCache;
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, RoomId,
	events::{TimelineEventType, room::encrypted::Relation},
};
use serde::Deserialize;

use self::data::Data;
pub use self::{
	create::pdu_fits,
	data::PdusIterItem,
	extremities::{
		calculate_true_extremities, calculate_true_extremities_roaring,
		detect_phantom_extremities_roaring, merge_true_extremities_roaring,
	},
	heal::{HealOptions, HealResult},
	metadata::EventMetadata,
	repair_unsigned::update_unsigned_prev_content,
};
use crate::{
	Dep, account_data, admin, appservice, globals, pusher, rooms,
	rooms::short::{ShortEventId, ShortStateHash},
	sending, server_keys, users,
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
	pub next_shortstatehash_cache: SyncMutex<LruCache<(ShortRoomId, PduCount), ShortStateHash>>,
	pub prev_shortstatehash_cache: SyncMutex<LruCache<(ShortRoomId, PduCount), ShortStateHash>>,
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
	state_compressor: Dep<rooms::state_compressor::Service>,
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
	auth_chain: Dep<rooms::auth_chain::Service>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
pub type RoomMutexGuard = MutexMapGuard<OwnedRoomId, ()>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let cache_capacity =
			f64::from(config.shortstatehash_cache_capacity) * config.cache_capacity_modifier;
		let cache_capacity = conduwuit_core::utils::math::usize_from_f64(cache_capacity)?;

		Ok(Arc::new(Self {
			next_shortstatehash_cache: SyncMutex::new(LruCache::new(cache_capacity / 2)),
			prev_shortstatehash_cache: SyncMutex::new(LruCache::new(cache_capacity / 2)),
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
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
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
				auth_chain: args.depend::<rooms::auth_chain::Service>("rooms::auth_chain"),
			},
			db: Data::new(&args),
			mutex_insert: RoomMutexMap::new(),
			mutex_fetch: MutexMap::new(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let next_cache_len = self.next_shortstatehash_cache.lock().len();
		let next_cache_bytes = next_cache_len.saturating_mul(
			size_of::<(ShortRoomId, PduCount)>().saturating_add(size_of::<ShortStateHash>()),
		);
		let next_bytes = conduwuit_core::utils::bytes::pretty(next_cache_bytes);
		writeln!(out, "next_shortstatehash_cache: {next_cache_len} ({next_bytes})")?;

		let prev_cache_len = self.prev_shortstatehash_cache.lock().len();
		let prev_cache_bytes = prev_cache_len.saturating_mul(
			size_of::<(ShortRoomId, PduCount)>().saturating_add(size_of::<ShortStateHash>()),
		);
		let prev_bytes = conduwuit_core::utils::bytes::pretty(prev_cache_bytes);
		writeln!(out, "prev_shortstatehash_cache: {prev_cache_len} ({prev_bytes})")?;

		let mutex_insert = self.mutex_insert.len();
		writeln!(out, "insert_mutex: {mutex_insert}")?;
		let mutex_fetch = self.mutex_fetch.len();
		writeln!(out, "fetch_mutex: {mutex_fetch}")?;

		Ok(())
	}

	async fn clear_cache(&self) {
		self.next_shortstatehash_cache.lock().clear();
		self.prev_shortstatehash_cache.lock().clear();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub fn db_batch(&self) -> database::rocksdb::WriteBatch { self.db.db_batch() }

	pub fn db_apply_batch(&self, batch: &database::rocksdb::WriteBatch) {
		self.db.db_apply_batch(batch);
	}

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

	/// Returns the shortstatehash of the room at the event directly preceding
	/// the exclusive `before` param. `before` does not have to be a valid
	/// count or in the room.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn prev_shortstatehash(
		&self,
		room_id: &RoomId,
		before: PduCount,
	) -> Result<ShortStateHash> {
		let shortroomid: ShortRoomId = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

		if let Some(hash) = self
			.prev_shortstatehash_cache
			.lock()
			.get_mut(&(shortroomid, before))
		{
			return Ok(*hash);
		}

		let before_pdu = PduId { shortroomid, shorteventid: before };

		let prev_count = self.db.prev_timeline_count(&before_pdu).await?;
		let prev_pdu = PduId { shortroomid, shorteventid: prev_count };

		let shorteventid = self.get_shorteventid_from_pdu_id(&prev_pdu).await?;

		let result = self.services.state.get_shortstatehash(shorteventid).await;

		if let Ok(hash) = result {
			self.prev_shortstatehash_cache
				.lock()
				.insert((shortroomid, before), hash);
		}

		result
	}

	/// Returns the shortstatehash of the room at the event directly following
	/// the exclusive `after` param. `after` does not have to be a valid count
	/// or in the room.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn next_shortstatehash(
		&self,
		room_id: &RoomId,
		after: PduCount,
	) -> Result<ShortStateHash> {
		let shortroomid: ShortRoomId = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

		if let Some(hash) = self
			.next_shortstatehash_cache
			.lock()
			.get_mut(&(shortroomid, after))
		{
			return Ok(*hash);
		}

		let after_pdu = PduId { shortroomid, shorteventid: after };

		let next_count = match self.db.next_timeline_count(&after_pdu).await {
			| Ok(count) => count,
			| Err(e) if e.is_not_found() => {
				let current = self.services.state.get_room_shortstatehash(room_id).await?;
				self.next_shortstatehash_cache
					.lock()
					.insert((shortroomid, after), current);
				return Ok(current);
			},
			| Err(e) => return Err(e),
		};
		let next_pdu = PduId { shortroomid, shorteventid: next_count };

		let shorteventid = self.get_shorteventid_from_pdu_id(&next_pdu).await?;

		let result = self.services.state.get_shortstatehash(shorteventid).await;

		if let Ok(hash) = result {
			self.next_shortstatehash_cache
				.lock()
				.insert((shortroomid, after), hash);
		}

		result
	}

	/// Returns the shortstatehash of the room at the event
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn get_shortstatehash(
		&self,
		room_id: &RoomId,
		count: PduCount,
	) -> Result<ShortStateHash> {
		let shortroomid: ShortRoomId = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

		let pdu_id = PduId { shortroomid, shorteventid: count };

		let shorteventid = self.get_shorteventid_from_pdu_id(&pdu_id).await?;

		self.services.state.get_shortstatehash(shorteventid).await
	}

	/// Returns the `shorteventid` from the `pdu_id`
	pub async fn get_shorteventid_from_pdu_id(&self, pdu_id: &PduId) -> Result<ShortEventId> {
		let event_id = self.get_event_id_from_pdu_id(pdu_id).await?;

		self.services.short.get_shorteventid(&event_id).await
	}

	/// Returns the `event_id` from the `pdu_id`
	pub async fn get_event_id_from_pdu_id(&self, pdu_id: &PduId) -> Result<OwnedEventId> {
		let pdu_id: RawPduId = (*pdu_id).into();

		self.get_pdu_from_id(&pdu_id).await.map(|pdu| pdu.event_id)
	}

	/// Returns the `count` of this pdu's id.
	pub async fn get_pdu_count(&self, event_id: &EventId) -> Result<PduCount> {
		self.db.get_pdu_count(event_id).await
	}

	pub async fn outlier_pdu_exists(&self, event_id: &EventId) -> Result<()> {
		self.db.outlier_pdu_exists(event_id).await
	}

	/// Returns the EventMetadata for a PDU.
	pub async fn get_event_metadata(&self, event_id: &EventId) -> Result<EventMetadata> {
		self.db.get_event_metadata(event_id).await
	}

	/// Returns the json of a pdu.
	pub async fn get_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
		self.db.get_pdu_json(event_id).await
	}

	#[inline]
	pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
		self.db.get_outlier_pdu_json(event_id).await
	}

	#[inline]
	pub async fn remove_from_timeline(&self, event_id: &EventId) {
		self.db.remove_from_timeline(event_id).await;
	}

	#[inline]
	pub async fn drop_duplicate_pdu(&self, pdu_id: &RawPduId) {
		self.db.drop_duplicate_pdu(pdu_id);
	}

	#[inline]
	pub async fn reindex_timeline(&self, room_id: &RoomId) -> Result<usize> {
		self.db.reindex_timeline(room_id).await
	}
}

impl Service {
	/// Rebuild the topological index for a room using proper DAG
	/// topological sort.
	///
	/// Reads all PDUs, builds the DAG from `prev_events`, performs a
	/// topological sort (parents before children, Kahn's algorithm with
	/// chronological tiebreaking), then rebuilds the
	/// `roomid_topologicalorder_pducount` index with correct
	/// `local_topological_depth` values computed as
	/// `max(parent_depths) + 1`. Stream order
	/// (`room_pducount_eventid`) is NEVER modified — it is immutable
	/// arrival-time ordering.
	///
	/// Optionally recomputes state snapshots incrementally and repairs
	/// `unsigned.prev_content` on state events.
	pub async fn reorder_timeline(
		&self,
		room_id: &RoomId,
		no_compute_state: bool,
	) -> Result<usize> {
		use std::collections::{HashMap, HashSet};

		use ruma::events::StateEventType;

		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
		let state_lock = self.services.state.mutex.lock(room_id).await;

		// Collect all PDUs from the timeline.
		// We need (PduCount, origin_server_ts) per event — the PduCount is the
		// existing immutable stream order which we preserve.
		let mut entries: HashMap<OwnedEventId, (PduCount, ruma::UInt)> = HashMap::new();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		let dropped = 0_usize;

		debug!("reorder_timeline: reading all PDUs from timeline...");
		let pdus_backfill = self.pdus(room_id, Some(PduCount::min()));
		let pdus_normal = self.pdus(room_id, Some(PduCount::Normal(0)));
		let pdus = pdus_backfill.chain(pdus_normal);
		pin_mut!(pdus);
		while let Some((count, pdu)) = pdus.try_next().await? {
			let eid = pdu.event_id.clone();
			entries.insert(eid.clone(), (count, pdu.origin_server_ts));
			graph.insert(eid, pdu.prev_events().map(ToOwned::to_owned).collect());
			if entries.len().is_multiple_of(10000) {
				debug!("reorder_timeline: read {} PDUs so far...", entries.len());
				tokio::task::yield_now().await;
			}
		}

		if dropped > 0 {
			warn!("{dropped} PDUs had no JSON and were skipped during reorder");
		}

		debug!("reorder_timeline: collected {} PDUs ({dropped} dropped)", entries.len());

		if entries.is_empty() {
			return Ok(0);
		}

		// Retain only edges within our event set for both topo sort and extremities.
		for parents in graph.values_mut() {
			parents.retain(|prev_id| entries.contains_key(prev_id));
		}

		// Topological sort: parents before children (Kahn's algorithm).
		// Tiebreak on origin_server_ts then event_id for determinism.
		let start = std::time::Instant::now();
		debug!("reorder_timeline: topologically sorting {} events...", entries.len());
		let sorted = topo_sort_dag(&entries, &graph);
		debug!(
			"reorder_timeline: topo sort took {:?} ({} events)",
			start.elapsed(),
			sorted.len()
		);

		if sorted.len() != entries.len() {
			warn!(
				"reorder_timeline: topo sort dropped {} events (cycles or disconnected)",
				entries.len().saturating_sub(sorted.len())
			);
		}

		// Rebuild topological index only -- stream order is immutable.
		let count = sorted.len();
		let reindex_start = std::time::Instant::now();
		debug!("reorder_timeline: rebuilding topological index for {count} events...");

		if !no_compute_state {
			// Full mode: rebuild topo index + recompute state snapshots
			let final_ssh = self
				.rebuild_topo_index_with_state(room_id, shortroomid, &sorted, &entries)
				.await;
			debug!("reorder_timeline: topo rebuild+state took {:?}", reindex_start.elapsed());

			if let Some(ssh) = final_ssh {
				if ssh != 0 {
					self.services
						.state
						.set_room_state(room_id, ssh, &state_lock);
					debug!("reorder_timeline: updated room shortstatehash to {ssh}");
				}
			}
		} else {
			// Fast mode: rebuild topo index only, no state computation
			let mut cork = Some(self.db.db.cork());
			for (i, event_id) in sorted.iter().enumerate() {
				let &(existing_count, _) = entries.get(event_id).expect("in sorted list");
				let pdu_id: RawPduId = PduId {
					shortroomid,
					shorteventid: existing_count,
				}
				.into();

				let local_topo_depth = u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1);
				self.db.reindex_topo(&pdu_id, event_id, local_topo_depth);

				if i.saturating_add(1).is_multiple_of(10000) {
					drop(cork.take());
					tokio::task::yield_now().await;
					cork = Some(self.db.db.cork());
				}
			}
			drop(cork.take());
			debug!("reorder_timeline: topo rebuild took {:?}", reindex_start.elapsed());
		}

		// Final batch: cork_and_sync ensures WAL is durable when dropped
		let final_sync = self.db.db.cork_and_sync();
		drop(final_sync);
		debug!("reorder_timeline: topo rebuild complete, calculating forward extremities...");

		// Calculate the true DAG forward extremities (events with in-degree 0
		// in the reversed graph). This fixes broken pagination and fork storms.

		// TODO: why not just use `calculate_true_extremities_roaring()` here?
		let mut true_extremities: Vec<OwnedEventId> = calculate_true_extremities(&graph, &sorted)
			.into_iter()
			.map(ToOwned::to_owned)
			.collect();

		// Preserve outlier extremities (e.g. from force-set-state) that are not in the
		// timeline.
		let current_exts: Vec<OwnedEventId> = self
			.services
			.state
			.get_forward_extremities(room_id)
			.collect()
			.await;
		for ext in current_exts {
			if !entries.contains_key(&ext) {
				true_extremities.push(ext);
			}
		}

		if !true_extremities.is_empty() {
			self.services
				.state
				.set_forward_extremities(
					room_id,
					true_extremities.clone().into_iter(),
					&state_lock,
				)
				.await;

			info!(
				"reorder_timeline: set forward extremities to {} true DAG tips",
				true_extremities.len()
			);
		}

		debug!("reorder_timeline: skipped repair unsigned per metadata design");

		// Rebuild membership cache from the authoritative state snapshot.
		// This fixes stale/missing entries left by previous DAG fractures.
		let mut members_synced = 0_usize;
		let mut state_joined: HashSet<ruma::OwnedUserId> = HashSet::new();
		let mut state_invited: HashSet<ruma::OwnedUserId> = HashSet::new();

		let mut room_ssh_opt = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.ok();
		if room_ssh_opt.is_none() {
			if let Some(latest_eid) = sorted.last() {
				if let Ok(ssh) = self
					.services
					.state_accessor
					.pdu_shortstatehash(latest_eid)
					.await
				{
					self.services
						.state
						.set_room_state(room_id, ssh, &state_lock);
					info!(
						"reorder_timeline: bootstrapped room state to shortstatehash {ssh} from \
						 latest event {latest_eid}"
					);
					room_ssh_opt = Some(ssh);
				}
			}
		}

		// Single pass over state snapshot — check-before-write avoids
		// redundant DB writes for users whose cache is already correct.
		if let Some(room_ssh) = room_ssh_opt {
			let state_full = self.services.state_accessor.state_full(room_ssh);
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
						// mark_as_invited requires sender; skip cache update
						// for invites here — update_joined_count will
						// reconcile.
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

		let sync_start = std::time::Instant::now();
		self.services.state_cache.update_joined_count(room_id).await;
		info!(
			"reorder_timeline: synced {members_synced} membership cache entries, removed \
			 {stale_removed} stale"
		);
		debug!("reorder_timeline: sweep cache took {:?}", sync_start.elapsed());

		drop(state_lock);

		debug!("reorder_timeline: complete, {count} events reordered (topo index/state)");

		Ok(count)
	}

	/// Rebuild topological index with incremental state computation.
	///
	/// For each event in topo-sorted order: removes old topo entry,
	/// computes `local_topological_depth` as position in topo-sorted
	/// list, writes new topo key, and optionally recomputes state
	/// snapshots. Stream order is NOT touched.
	async fn rebuild_topo_index_with_state(
		&self,
		room_id: &RoomId,
		shortroomid: ShortRoomId,
		sorted: &[OwnedEventId],
		entries: &std::collections::HashMap<OwnedEventId, (PduCount, ruma::UInt)>,
	) -> Option<u64> {
		let count = sorted.len();

		let mut current_shortstatehash = {
			let mut ssh = 0;
			if let Some(oldest_event_id) = sorted.first() {
				if let Ok(oldest_pdu) = self
					.db
					.get_pdu_in_room(Some(room_id), oldest_event_id)
					.await
				{
					if let Some(prev) = oldest_pdu.prev_events.first() {
						if let Ok(prev_ssh) =
							self.services.state_accessor.pdu_shortstatehash(prev).await
						{
							ssh = prev_ssh;
						}
					}
				}
			}
			Some(ssh)
		};

		let mut cork = Some(self.db.db.cork());
		for (i, event_id) in sorted.iter().enumerate() {
			// Use the existing stream order count -- do NOT fabricate a new one
			let Some(&(existing_count, _)) = entries.get(event_id) else {
				continue;
			};
			let pdu_id: RawPduId = PduId {
				shortroomid,
				shorteventid: existing_count,
			}
			.into();

			let (pdu, mut json) = match self.db.get_from_eventid_pdu(event_id).await {
				| Ok(res) => res,
				| Err(e) => {
					warn!(
						%event_id,
						"PDU missing during topo rebuild (skipping): {e}"
					);
					continue;
				},
			};

			// Events being reindexed are definitively in the timeline; any
			// rejection flags are stale and would poison state resolution
			// if left in place. Soft-fail flags are intentional and persist.
			self.services.pdu_metadata.unmark_event_rejected(event_id);

			let local_topo_depth = u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1);

			// Rebuild topo index entry with new depth
			self.db.reindex_topo(&pdu_id, event_id, local_topo_depth);

			// State computation — uses existing pdu_id (unchanged stream order)
			let mut json_modified = false;
			if let Some(mut ssh) = current_shortstatehash {
				let shorteventid = self
					.services
					.short
					.get_or_create_shorteventid(&pdu.event_id)
					.await;
				self.services
					.state
					.set_pdu_shortstatehash(shorteventid, ssh);

				if let Some(state_key) = &pdu.state_key {
					// Repair unsigned.prev_content for historical/backfilled events while we have
					// the state snapshot!
					if ssh != 0 {
						if let Ok(prev_state) = self
							.services
							.state_accessor
							.state_get(ssh, &pdu.kind.to_string().into(), state_key)
							.await
						{
							if update_unsigned_prev_content(&mut json, &prev_state).is_ok() {
								json_modified = true;
							}
						}
					}

					let states_parents = if ssh != 0 {
						self.services
							.state_compressor
							.load_shortstatehash_info(ssh)
							.await
							.unwrap_or_default()
					} else {
						Vec::new()
					};
					let shortstatekey = self
						.services
						.short
						.get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)
						.await;
					let new = self
						.services
						.state_compressor
						.compress_state_event(shortstatekey, &pdu.event_id)
						.await;
					let replaces = states_parents.last().and_then(|info| {
						info.full_state.as_ref().expect("top frame").iter().find(
							|bytes: &&rooms::state_compressor::CompressedStateEvent| {
								bytes.starts_with(&shortstatekey.to_be_bytes())
							},
						)
					});

					if Some(&new) != replaces {
						if let Ok(new_ssh) = self.services.globals.next_count() {
							let mut statediffnew =
								rooms::state_compressor::CompressedState::new();
							statediffnew.insert(new);
							let mut statediffremoved =
								rooms::state_compressor::CompressedState::new();
							if let Some(replaces) = replaces {
								statediffremoved.insert(*replaces);
							}
							let _ = self.services.state_compressor.save_state_from_diff(
								new_ssh,
								Arc::new(statediffnew),
								Arc::new(statediffremoved),
								2,
								states_parents,
							);
							ssh = new_ssh;
						}
					}
				}
				current_shortstatehash = Some(ssh);
			}

			// Only write JSON when unsigned.prev_content was actually repaired
			if json_modified {
				self.db.update_pdu_json(event_id, &json);
			}

			if i.saturating_add(1).is_multiple_of(2000) {
				debug!(
					"reorder_timeline: rebuilt {}/{count} topo entries...",
					i.saturating_add(1)
				);
			}
			if i.saturating_add(1).is_multiple_of(10000) {
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
			}
		}
		drop(cork.take());

		current_shortstatehash.filter(|&ssh| ssh != 0)
	}

	/// Prune fork storms down to operationally relevant tips using tail-based
	/// recalculation. This is a convenience wrapper around
	/// `recalculate_extremities` with standardized logging.
	pub async fn prune_extremities(&self, room_id: &RoomId, tail: usize) {
		match self.recalculate_extremities(room_id, tail, true).await {
			| Ok((true, tips)) => info!(
				%room_id, tail, tips,
				"pruned extremities via tail-based recalculation"
			),
			| Ok((false, tips)) => info!(
				%room_id, tail, tips,
				"extremities already consistent after recalculation"
			),
			| Err(e) => warn!(
				%room_id, tail,
				"failed to prune extremities: {e}"
			),
		}
	}

	/// Incrementally rebuilds the true state of the room by iterating through
	/// the timeline in its current PduCount order, resolving the state for
	/// each event, and updating the DB. This heals a fractured room state
	/// without re-ordering events or generating new PduCounts, preventing UI
	/// sync spam.
	#[tracing::instrument(skip(self), level = "info")]
	pub async fn rebuild_state(&self, room_id: &RoomId) -> Result<()> {
		use std::{
			collections::{BTreeSet, HashMap, HashSet},
			sync::Arc,
		};

		use futures::StreamExt;

		let original_room_shortstatehash = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.ok();

		// Stream events in topological order (already rebuilt by reorder_timeline).
		// Collect minimal metadata for the multi-head merge at the end.
		info!("rebuild_state: streaming events in topological order...");
		let stream_start = std::time::Instant::now();

		let mut events_meta: Vec<(OwnedEventId, Vec<OwnedEventId>, Option<String>, u64)> =
			Vec::new();
		let mut room_version = ruma::RoomVersionId::V1;
		let mut room_version_found = false;

		let mut stream = std::pin::pin!(self.topo_pdus(room_id, None));
		while let Some(Ok((_pdu_count, pdu))) = stream.next().await {
			let eid = pdu.event_id().to_owned();
			let prev: Vec<OwnedEventId> = pdu.prev_events().map(ToOwned::to_owned).collect();
			let state_key = pdu.state_key().map(ToOwned::to_owned);
			let depth = u64::from(pdu.depth());

			// Timeline events are authoritative; clear any stale rejection
			// flags that would otherwise poison the state resolution below.
			// Soft-fail flags are intentional and must persist.
			self.services.pdu_metadata.unmark_event_rejected(&eid);

			if !room_version_found && *pdu.kind() == TimelineEventType::RoomCreate {
				if let Ok(create_content) = serde_json::from_str::<
					ruma::events::room::create::RoomCreateEventContent,
				>(pdu.content().get())
				{
					room_version = create_content.room_version;
					room_version_found = true;
				}
			}

			events_meta.push((eid, prev, state_key, depth));
		}

		debug!(
			"rebuild_state: loaded {} event metadata in {:?}",
			events_meta.len(),
			stream_start.elapsed(),
		);

		// Build event-id set for filtering missing parents + forward extremity calc
		let event_set: HashSet<&OwnedEventId> = events_meta.iter().map(|(eid, ..)| eid).collect();

		let rebuild_start = std::time::Instant::now();
		debug!("rebuild_state: starting state resolution...");

		let mut ssh_cache: HashMap<OwnedEventId, u64> = HashMap::new();
		let mut resolved_state_cache: HashMap<Vec<u64>, u64> = HashMap::new();
		let mut processed = 0_usize;
		let mut single_parent_count = 0_usize;
		let mut no_parent_count = 0_usize;
		let mut cache_hit_count = 0_usize;
		let mut fork_resolve_count = 0_usize;
		let mut cumulative_resolve_time = std::time::Duration::ZERO;
		let empty_ssh = self
			.services
			.state_compressor
			.save_state(room_id, Arc::new(BTreeSet::new()))
			.await?
			.shortstatehash;

		let mut cork = Some(self.db.db.cork());
		let total_events = events_meta.len();
		let mut current_shortstatehash = 0_u64;

		for (eid, prev_events, state_key, _depth) in &events_meta {
			processed = processed.saturating_add(1);

			if processed.is_multiple_of(1000) {
				debug!(
					"rebuild_state: {}/{} events | single:{} none:{} cached:{} resolved:{} | \
					 cumulative_resolve: {:?} | elapsed: {:?}",
					processed,
					total_events,
					single_parent_count,
					no_parent_count,
					cache_hit_count,
					fork_resolve_count,
					cumulative_resolve_time,
					rebuild_start.elapsed(),
				);
			}

			// Find parent state — only consider parents that exist in our event set
			let prev_sshs: Vec<u64> = prev_events
				.iter()
				.filter(|prev_id| event_set.contains(prev_id))
				.filter_map(|prev_id| ssh_cache.get(prev_id).copied())
				.collect();

			let mut unique_sshs = prev_sshs.clone();
			unique_sshs.sort_unstable();
			unique_sshs.dedup();

			let loop_start = std::time::Instant::now();

			let state_before = match unique_sshs.len() {
				| 1 => {
					single_parent_count = single_parent_count.saturating_add(1);
					unique_sshs[0]
				},
				| 0 => {
					no_parent_count = no_parent_count.saturating_add(1);
					empty_ssh
				},
				| _ => {
					if let Some(&cached_ssh) = resolved_state_cache.get(&unique_sshs) {
						cache_hit_count = cache_hit_count.saturating_add(1);
						cached_ssh
					} else {
						// Slow path for forks: fetch PDU from DB and run state resolution
						let pdu = self.get_pdu(eid).await?;
						let state_after_opt = self
							.services
							.event_handler
							.state_at_incoming_resolved(&pdu, room_id, &room_version)
							.await?;
						let state_after = state_after_opt.unwrap_or_default();
						let compressed_state: BTreeSet<_> = self
							.services
							.state_compressor
							.compress_state_events(state_after.iter().map(|(k, id)| (k, &**id)))
							.collect()
							.await;

						let state_delta = self
							.services
							.state_compressor
							.save_state_with_parent(
								room_id,
								Some(unique_sshs[0]),
								Arc::new(compressed_state),
							)
							.await?;

						let ssh = state_delta.shortstatehash;
						resolved_state_cache.insert(unique_sshs, ssh);

						let slow_path_elapsed = loop_start.elapsed();
						fork_resolve_count = fork_resolve_count.saturating_add(1);
						cumulative_resolve_time =
							cumulative_resolve_time.saturating_add(slow_path_elapsed);

						if slow_path_elapsed.as_millis() > 50 {
							debug!(
								"rebuild_state: SLOW fork #{fork_resolve_count} for {eid} ({} \
								 parents, {} unique ssh) took {:?}",
								prev_sshs.len(),
								prev_sshs.iter().collect::<HashSet<_>>().len(),
								slow_path_elapsed
							);
						}

						ssh
					}
				},
			};

			let mut state_after = state_before;

			if let Some(sk) = state_key {
				let states_parents = if state_before != 0 {
					self.services
						.state_compressor
						.load_shortstatehash_info(state_before)
						.await
						.unwrap_or_default()
				} else {
					Vec::new()
				};
				// Need the event type — fetch from DB only for state events
				let pdu = self.get_pdu(eid).await?;
				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(&pdu.kind().to_string().into(), sk)
					.await;
				let new = self
					.services
					.state_compressor
					.compress_state_event(shortstatekey, pdu.event_id())
					.await;
				let replaces = states_parents.last().and_then(|info| {
					info.full_state.as_ref().expect("top frame").iter().find(
						|bytes: &&rooms::state_compressor::CompressedStateEvent| {
							bytes.starts_with(&shortstatekey.to_be_bytes())
						},
					)
				});

				if Some(&new) != replaces {
					if let Ok(new_ssh) = self.services.globals.next_count() {
						let mut statediffnew = rooms::state_compressor::CompressedState::new();
						statediffnew.insert(new);
						let mut statediffremoved =
							rooms::state_compressor::CompressedState::new();
						if let Some(replaces) = replaces {
							statediffremoved.insert(*replaces);
						}
						let _ = self.services.state_compressor.save_state_from_diff(
							new_ssh,
							Arc::new(statediffnew.clone()),
							Arc::new(statediffremoved.clone()),
							2, // diff_to_sibling
							states_parents,
						);
						state_after = new_ssh;
					}
				}
			}

			ssh_cache.insert(eid.clone(), state_after);
			self.services.state.set_pdu_shortstatehash(
				self.services.short.get_or_create_shorteventid(eid).await,
				state_after,
			);
			current_shortstatehash = state_after;

			if processed.is_multiple_of(1000) {
				info!("rebuild_state: processed {processed} events...");
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
			}

			let full_loop_elapsed = loop_start.elapsed();
			if full_loop_elapsed.as_millis() > 100 {
				warn!(
					"rebuild_state: full loop iteration for {eid} took {:?}",
					full_loop_elapsed
				);
			}
		}

		drop(cork.take());

		debug!(
			"rebuild_state: DONE {processed} events in {:?} | single:{single_parent_count} \
			 none:{no_parent_count} cached:{cache_hit_count} resolved:{fork_resolve_count} | \
			 cumulative_resolve: {:?}",
			rebuild_start.elapsed(),
			cumulative_resolve_time,
		);

		// Final multi-head resolution: find all forward extremities (events with no
		// children in the DAG), collect their unique SSHs, and merge them.
		// This handles disconnected components whose states were never merged
		// during the linear walk.
		let mut has_children: HashSet<&OwnedEventId> = HashSet::new();
		for (_, prev_events, ..) in &events_meta {
			for parent in prev_events {
				if event_set.contains(parent) {
					has_children.insert(parent);
				}
			}
		}
		let extremity_sshs: Vec<u64> = events_meta
			.iter()
			.map(|(eid, ..)| eid)
			.filter(|eid| !has_children.contains(eid))
			.filter_map(|eid| ssh_cache.get(eid).copied())
			.collect::<HashSet<_>>()
			.into_iter()
			.collect();

		let num_extremities = events_meta
			.iter()
			.map(|(eid, ..)| eid)
			.filter(|eid| !has_children.contains(eid))
			.count();

		if extremity_sshs.len() > 1 {
			debug!(
				"rebuild_state: {} forward extremities with {} unique SSHs — merging \
				 disconnected components...",
				num_extremities,
				extremity_sshs.len(),
			);

			// Load full compressed state for each unique SSH
			let mut all_compressed = BTreeSet::new();
			for &ssh in &extremity_sshs {
				if let Ok(info) = self
					.services
					.state_compressor
					.load_shortstatehash_info(ssh)
					.await
				{
					if let Some(frame) = info.last() {
						if let Some(full_state) = frame.full_state.as_ref() {
							for entry in full_state.as_ref() {
								all_compressed.insert(*entry);
							}
						}
					}
				}
			}

			// Build ssk -> set of shorteventid values to detect conflicts
			let mut ssk_values: HashMap<u64, HashSet<u64>> = HashMap::new();
			for bytes in &all_compressed {
				let mut ssk_bytes = [0_u8; 8];
				ssk_bytes.copy_from_slice(&bytes[0..8]);
				let ssk = u64::from_be_bytes(ssk_bytes);
				let mut id_bytes = [0_u8; 8];
				id_bytes.copy_from_slice(&bytes[8..16]);
				let sei = u64::from_be_bytes(id_bytes);
				ssk_values.entry(ssk).or_default().insert(sei);
			}

			let conflicting: Vec<_> = ssk_values
				.iter()
				.filter(|(_, values)| values.len() > 1)
				.map(|(ssk, _)| *ssk)
				.collect();

			if conflicting.is_empty() {
				// No conflicts — trivial union merge
				debug!(
					"rebuild_state: trivial merge of {} state entries from {} components",
					ssk_values.len(),
					extremity_sshs.len(),
				);
				let merged_ssh = self
					.services
					.state_compressor
					.save_state(room_id, Arc::new(all_compressed))
					.await?
					.shortstatehash;
				current_shortstatehash = merged_ssh;
			} else {
				// Conflicting keys exist — need to pick winners
				// For non-auth conflicts, pick the event with the latest depth
				debug!(
					"rebuild_state: {} conflicting keys across {} components — resolving...",
					conflicting.len(),
					extremity_sshs.len(),
				);

				// Build ShortEventId -> depth map only for conflicting SEIs
				// using pre-computed depth from events_meta
				let depth_by_eid: HashMap<&OwnedEventId, u64> = events_meta
					.iter()
					.map(|(eid, _, _, depth)| (eid, *depth))
					.collect();
				let mut sei_depth: HashMap<u64, u64> = HashMap::new();
				let conflicting_seis: HashSet<u64> = ssk_values
					.iter()
					.filter(|(_, values)| values.len() > 1)
					.flat_map(|(_, values)| values.iter().copied())
					.collect();
				for &sei in &conflicting_seis {
					if let Ok(eid) = self
						.services
						.short
						.get_eventid_from_short::<OwnedEventId>(sei)
						.await
					{
						if let Some(&depth) = depth_by_eid.get(&eid) {
							sei_depth.insert(sei, depth);
						}
					}
				}

				// Build the final state: for each ssk, if non-conflicting keep it;
				// if conflicting, pick winner by latest depth (matching state_res behavior)
				let mut final_state = BTreeSet::new();
				for (&ssk, values) in &ssk_values {
					if values.len() == 1 {
						// Non-conflicting — keep the only value
						let sei = *values.iter().next().unwrap();
						final_state
							.insert(rooms::state_compressor::compress_state_event(ssk, sei));
					} else {
						// Conflicting — pick winner by highest depth
						let mut best_sei = 0_u64;
						let mut best_depth = 0_u64;
						for &sei in values {
							let depth = sei_depth.get(&sei).copied().unwrap_or(0);
							if depth > best_depth || best_sei == 0 {
								best_depth = depth;
								best_sei = sei;
							}
						}
						final_state
							.insert(rooms::state_compressor::compress_state_event(ssk, best_sei));
					}
				}

				debug!("rebuild_state: merged state has {} entries", final_state.len());
				let merged_ssh = self
					.services
					.state_compressor
					.save_state(room_id, Arc::new(final_state))
					.await?
					.shortstatehash;
				current_shortstatehash = merged_ssh;
			}
		} else {
			debug!(
				"rebuild_state: all forward extremities share a single SSH — no multi-head \
				 merge needed",
			);
		}

		let (total_added, total_removed) = if let Some(old_ssh) = original_room_shortstatehash {
			let old_info = self
				.services
				.state_compressor
				.load_shortstatehash_info(old_ssh)
				.await
				.unwrap_or_default();
			let new_info = self
				.services
				.state_compressor
				.load_shortstatehash_info(current_shortstatehash)
				.await
				.unwrap_or_default();
			let empty = BTreeSet::new();
			let old_full = old_info
				.last()
				.and_then(|info| info.full_state.as_ref())
				.map_or(&empty, |a| &**a);
			let new_full = new_info
				.last()
				.and_then(|info| info.full_state.as_ref())
				.map_or(&empty, |a| &**a);
			let added: BTreeSet<_> = new_full.difference(old_full).copied().collect();
			let removed: BTreeSet<_> = old_full.difference(new_full).copied().collect();
			(Arc::new(added), Arc::new(removed))
		} else {
			let new_info = self
				.services
				.state_compressor
				.load_shortstatehash_info(current_shortstatehash)
				.await
				.unwrap_or_default();
			let new_full = new_info
				.last()
				.and_then(|info| info.full_state.as_ref())
				.cloned()
				.unwrap_or_default();
			(new_full, Arc::new(BTreeSet::new()))
		};

		// Now we must update the room's global state to match the final calculated
		// state
		let state_lock = self.services.state.mutex.lock(room_id).await;
		self.services
			.state
			.force_state_quiet(
				room_id,
				current_shortstatehash,
				total_added,
				total_removed,
				&state_lock,
			)
			.await?;

		Ok(())
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
	) -> Result<(bool, usize)> {
		use std::collections::{HashMap, HashSet};

		use futures::StreamExt;
		use roaring::RoaringBitmap;
		use ruma::OwnedEventId;

		let state_lock = self.services.state.mutex.lock(room_id).await;

		let capacity = if tail == usize::MAX { 0 } else { tail };
		let mut eids = Vec::with_capacity(capacity);

		let mut stream = std::pin::pin!(self.db.room_event_ids_rev(room_id, None));
		while let Some(Ok(eid)) = stream.next().await {
			eids.push(eid);
			if eids.len() >= tail {
				break;
			}
		}

		// room_event_ids_rev returns newest first. We need oldest for true_extremities
		eids.reverse();

		let mut short_ids: Vec<ShortEventId> = Vec::with_capacity(eids.len());
		for eid in &eids {
			let short = self.services.short.get_shorteventid(eid).await?;
			short_ids.push(short);
		}

		let mut ts_map = HashMap::with_capacity(eids.len());
		let mut id_map: HashMap<ShortEventId, u32> = HashMap::with_capacity(eids.len());
		let mut reverse_id_map: Vec<ShortEventId> = Vec::with_capacity(eids.len());

		let get_or_insert_id = |short: ShortEventId,
		                        id_map: &mut HashMap<ShortEventId, u32>,
		                        reverse_id_map: &mut Vec<ShortEventId>|
		 -> u32 {
			if let Some(&id) = id_map.get(&short) {
				id
			} else {
				let id = u32::try_from(reverse_id_map.len()).unwrap_or(0);
				id_map.insert(short, id);
				reverse_id_map.push(short);
				id
			}
		};

		let mut graph: Vec<RoaringBitmap> = Vec::with_capacity(eids.len());
		let mut sorted: Vec<u32> = Vec::with_capacity(eids.len());

		for short in &short_ids {
			let id = get_or_insert_id(*short, &mut id_map, &mut reverse_id_map);
			let id_usize = usize::try_from(id).expect("u32 fits in usize");

			if id_usize >= graph.len() {
				graph.resize(id_usize.saturating_add(1), RoaringBitmap::new());
			}

			let mut prev_bitmap = RoaringBitmap::new();
			let prev_shorts = self
				.db
				.get_shortprevevents(*short)
				.await
				.unwrap_or_default();
			for prev_short in prev_shorts {
				prev_bitmap.insert(get_or_insert_id(
					prev_short,
					&mut id_map,
					&mut reverse_id_map,
				));
			}
			graph[id_usize] = prev_bitmap;
			sorted.push(id);
		}

		// Calculate true extremities via roaring bitmap intersections
		let true_extremities_bm = calculate_true_extremities_roaring(&graph, &sorted);

		let current_extremities = self.services.state.get_forward_extremities(room_id);
		let current_set: HashSet<_> = current_extremities.collect().await;

		let mut current_bm = RoaringBitmap::new();
		for eid in &current_set {
			if let Ok(short) = self.services.short.get_shorteventid(eid).await {
				if let Some(&id) = id_map.get(&short) {
					current_bm.insert(id);
				}
			}
		}

		let phantom_tips_bm = detect_phantom_extremities_roaring(&graph, &current_bm);
		let merged_extremities_bm =
			merge_true_extremities_roaring(&true_extremities_bm, &current_bm, &phantom_tips_bm);

		let mut true_extremities_set: HashSet<OwnedEventId> = HashSet::with_capacity(
			usize::try_from(merged_extremities_bm.len()).unwrap_or(usize::MAX),
		);
		for id in merged_extremities_bm {
			let short = reverse_id_map[usize::try_from(id).expect("u32 fits in usize")];
			if let Ok(eid) = self.services.short.get_eventid_from_short(short).await {
				true_extremities_set.insert(eid);
			}
		}

		// Add current extremities that were outside the graph window
		for eid in &current_set {
			if let Ok(short) = self.services.short.get_shorteventid(eid).await {
				if !id_map.contains_key(&short) {
					true_extremities_set.insert(eid.clone());
				}
			} else {
				// If we can't even get its shorteventid, still preserve it just in case
				true_extremities_set.insert(eid.clone());
			}
		}

		// Ensure we have timestamps for all tips we intend to keep
		for eid in &true_extremities_set {
			if !ts_map.contains_key(eid) {
				if let Ok(ts) = self.db.get_origin_server_ts(eid).await {
					ts_map.insert(eid.to_owned(), ts);
				}
			}
		}

		let mut final_extremities: Vec<OwnedEventId> = true_extremities_set.into_iter().collect();

		final_extremities.sort_by_key(|eid| {
			ts_map
				.get(eid)
				.copied()
				.unwrap_or_else(|| ruma::MilliSecondsSinceUnixEpoch(0_u32.into()))
		});

		let num_true_extremities = final_extremities.len();

		// If the finalized extremities perfectly match the current DB, we skip
		let final_set: HashSet<_> = final_extremities.iter().cloned().collect();
		if final_set == current_set {
			return Ok((false, num_true_extremities));
		}

		if update_db {
			// STRICT OVERWRITE: Erases phantom tips that fell out of the window.
			// set_forward_extremities enforces MAX_FORWARD_EXTREMITIES cap.
			self.services
				.state
				.set_forward_extremities(room_id, final_extremities.into_iter(), &state_lock)
				.await;
		}

		Ok((true, num_true_extremities))
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

	/// Fetch multiple PDUs in parallel from the database.
	pub fn multi_get_pdus<'a, S>(
		&'a self,
		room_id: Option<&'a RoomId>,
		event_ids: S,
	) -> impl Stream<Item = Result<PduEvent>> + Send + 'a
	where
		S: Stream<Item = OwnedEventId> + Send + 'a,
	{
		self.db.multi_get_pdus(room_id, event_ids)
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
	pub async fn replace_pdu(
		&self,
		pdu_id: &RawPduId,
		pdu_json: &CanonicalJsonObject,
		event_id: &EventId,
	) -> Result {
		self.db.replace_pdu(pdu_id, pdu_json, event_id).await
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

	pub fn topo_pdus_rev<'a>(
		&'a self,
		room_id: &'a RoomId,
		until: Option<PduCount>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.db
			.topo_pdus_rev(room_id, until.unwrap_or_else(PduCount::max))
	}

	#[tracing::instrument(skip(self), level = "info")]
	pub async fn fix_pdu_event_ids(&self) -> Result<usize> { self.db.fix_pdu_event_ids().await }

	/// Forward iteration starting after `from`.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn pdus<'a>(
		&'a self,
		room_id: &'a RoomId,
		from: Option<PduCount>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.db.pdus(room_id, from.unwrap_or_else(PduCount::min))
	}

	/// Forward iteration using topological ordering, starting after `from`.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn topo_pdus<'a>(
		&'a self,
		room_id: &'a RoomId,
		from: Option<PduCount>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.db
			.topo_pdus(room_id, from.unwrap_or_else(PduCount::min))
	}
}

impl Service {
	pub fn multi_get_shortauthevents<'a, I>(
		&'a self,
		shorteventids: I,
	) -> impl Stream<Item = Result<Vec<ShortEventId>>> + Send + 'a
	where
		I: Stream<Item = ShortEventId> + Send + 'a,
	{
		self.db.multi_get_shortauthevents(shorteventids)
	}
}

/// Topological sort of a DAG using Kahn's algorithm.
///
/// Returns events in parent-before-child order. When multiple events have
/// in-degree 0 simultaneously, tiebreaks on `origin_server_ts` first
/// (chronological ordering within the same DAG level), then falls back to
/// `event_id` (content hash) for determinism when timestamps collide.
/// Events involved in cycles are appended at the end in the same order.
pub fn topo_sort_dag<S1, S2>(
	entries: &std::collections::HashMap<OwnedEventId, (PduCount, ruma::UInt), S1>,
	graph: &std::collections::HashMap<
		OwnedEventId,
		std::collections::HashSet<OwnedEventId, S2>,
		S1,
	>,
) -> Vec<OwnedEventId>
where
	S1: std::hash::BuildHasher,
	S2: std::hash::BuildHasher,
{
	use std::collections::{BinaryHeap, HashMap, HashSet};

	let n = entries.len();

	// Build forward adjacency (parent -> children) and in-degree counts.
	let mut children: HashMap<&OwnedEventId, Vec<&OwnedEventId>> = HashMap::with_capacity(n);
	let mut in_degree: HashMap<&OwnedEventId, usize> = HashMap::with_capacity(n);

	for event_id in entries.keys() {
		in_degree.entry(event_id).or_insert(0);
	}

	for (event_id, parents) in graph {
		if !entries.contains_key(event_id) {
			continue;
		}
		for parent in parents {
			if entries.contains_key(parent) {
				children.entry(parent).or_default().push(event_id);
				let deg = in_degree.entry(event_id).or_insert(0);
				*deg = deg.saturating_add(1);
			}
		}
	}

	// Min-heap by (ts, event_id) for chronological tiebreaking with
	// deterministic hash-based fallback when timestamps collide.
	let mut heap: BinaryHeap<std::cmp::Reverse<(u64, &OwnedEventId)>> =
		BinaryHeap::with_capacity(n);
	for (event_id, deg) in &in_degree {
		if *deg == 0 {
			let ts = entries.get(*event_id).map_or(0, |(_, ts)| u64::from(*ts));
			heap.push(std::cmp::Reverse((ts, *event_id)));
		}
	}

	let mut result = Vec::with_capacity(n);
	let mut visited: HashSet<&OwnedEventId> = HashSet::with_capacity(n);

	while let Some(std::cmp::Reverse((_, event_id))) = heap.pop() {
		if !visited.insert(event_id) {
			continue;
		}
		result.push(event_id.clone());

		if let Some(kids) = children.get(event_id) {
			for &child in kids {
				if let Some(deg) = in_degree.get_mut(child) {
					*deg = deg.saturating_sub(1);
					if *deg == 0 {
						let ts = entries.get(child).map_or(0, |(_, ts)| u64::from(*ts));
						heap.push(std::cmp::Reverse((ts, child)));
					}
				}
			}
		}
	}

	// Append any remaining events (cycles) in ts then event_id order
	if result.len() < n {
		let mut remaining: Vec<&OwnedEventId> = entries
			.keys()
			.filter(|eid| !visited.contains(eid))
			.collect();
		remaining.sort_by(|a, b| {
			let ts_a = entries.get(*a).map_or(0, |(_, ts)| u64::from(*ts));
			let ts_b = entries.get(*b).map_or(0, |(_, ts)| u64::from(*ts));
			ts_a.cmp(&ts_b).then_with(|| a.cmp(b))
		});
		result.extend(remaining.into_iter().cloned());
	}

	result
}
