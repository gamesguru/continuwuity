mod append;
mod backfill;
mod build;
mod create;
mod data;
mod redact;
mod repair_unsigned;
use std::{fmt::Write, sync::Arc};

use async_trait::async_trait;
pub use conduwuit_core::matrix::pdu::{PduId, RawPduId, ShortRoomId};
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
	pub async fn drop_duplicate_pdu(&self, pdu_id: &RawPduId) {
		self.db.drop_duplicate_pdu(pdu_id);
	}

	#[inline]
	pub async fn reindex_timeline(&self, room_id: &RoomId) -> Result<usize> {
		self.db.reindex_timeline(room_id).await
	}
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
	pub async fn reorder_timeline(
		&self,
		room_id: &RoomId,
		tail: Option<usize>,
		no_compute_state: bool,
	) -> Result<usize> {
		use std::collections::{HashMap, HashSet};

		use conduwuit_core::matrix::state_res;
		use futures::future::ready;
		use ruma::events::StateEventType;

		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
		let state_lock = self.services.state.mutex.lock(room_id).await;

		// Collect PDUs from the timeline — either all (full reorder) or last N (tail)
		// Only keep (PduCount, origin_server_ts) per event to avoid holding the full
		// PduEvent JSON in memory simultaneously (causes OOM on large rooms).
		let mut entries: HashMap<OwnedEventId, (PduCount, ruma::UInt)> = HashMap::new();
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		let dropped = 0_usize;

		if let Some(limit) = tail {
			info!("reorder_timeline: reading last {limit} PDUs from timeline (tail mode)...");
			// Collect in reverse and record the minimum count seen (oldest in window)
			let mut rev = Box::pin(self.pdus_rev(room_id, None));
			let mut collected = 0_usize;
			while let Some((count, pdu)) = rev.try_next().await? {
				if collected >= limit {
					break;
				}
				entries.insert(pdu.event_id.clone(), (count, pdu.origin_server_ts));
				graph.insert(
					pdu.event_id.clone(),
					pdu.prev_events().map(ToOwned::to_owned).collect(),
				);
				collected = collected.saturating_add(1);
				if collected.is_multiple_of(10000) {
					tokio::task::yield_now().await;
				}
			}
		} else {
			info!("reorder_timeline: reading all PDUs from timeline...");
			let pdus_backfill = self.pdus(room_id, Some(PduCount::min()));
			let pdus_normal = self.pdus(room_id, Some(PduCount::Normal(0)));
			let pdus = pdus_backfill.chain(pdus_normal);
			pin_mut!(pdus);
			while let Some((count, pdu)) = pdus.try_next().await? {
				let eid = pdu.event_id.clone();
				entries.insert(eid.clone(), (count, pdu.origin_server_ts));
				graph.insert(eid, pdu.prev_events().map(ToOwned::to_owned).collect());
				if entries.len().is_multiple_of(10000) {
					info!("reorder_timeline: read {} PDUs so far...", entries.len());
					tokio::task::yield_now().await;
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
		for parents in graph.values_mut() {
			parents.retain(|prev_id| entries.contains_key(prev_id));
		}

		// Topological sort with PduCount as tiebreaker
		info!("reorder_timeline: topological sort of {} events...", graph.len());
		let event_fetch = |event_id: OwnedEventId| {
			let ts = entries
				.get(&event_id)
				.map_or(ruma::UInt::MAX, |&(_, ts)| ts);

			ready(Ok::<_, state_res::Error>((
				ruma::int!(0),
				ruma::MilliSecondsSinceUnixEpoch(ts.into()),
			)))
		};

		let sorted = state_res::lexicographical_topological_sort(&graph, &event_fetch)
			.await
			.map_err(|e| err!(Database("Failed to sort timeline: {e:?}")))?;

		// BACKUP PHASE: Safely backup all JSON to the outlier tables BEFORE deleting
		// them from the timeline. This prevents data loss since
		// remove_from_timeline_by_id deletes the pduid_pdu entries that exclusively
		// hold normal event JSON.
		self.backup_timeline_entries(room_id, shortroomid, &entries)
			.await;

		// Remove old timeline entries (batched cork every 10K avoids giant WriteBatch)
		self.remove_old_timeline_entries(shortroomid, &sorted, &entries)
			.await;

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
		self.reinsert_timeline_entries(
			room_id,
			shortroomid,
			&sorted,
			batch_start,
			no_compute_state,
		)
		.await;

		// Final batch: cork_and_sync ensures WAL is durable when dropped
		let final_sync = self.db.db.cork_and_sync();
		drop(final_sync);
		info!("reorder_timeline: re-insert complete, calculating forward extremities...");

		// Calculate the true DAG forward extremities (events with in-degree 0
		// in the reversed graph). This fixes broken pagination and fork storms.

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

		self.services.state_cache.update_joined_count(room_id).await;
		info!(
			"reorder_timeline: synced {members_synced} membership cache entries, removed \
			 {stale_removed} stale"
		);

		drop(state_lock);

		info!("reorder_timeline: complete, {count} events reordered");

		Ok(count)
	}

	async fn backup_timeline_entries(
		&self,
		room_id: &RoomId,
		shortroomid: ShortRoomId,
		entries: &std::collections::HashMap<OwnedEventId, (PduCount, ruma::UInt)>,
	) {
		info!(
			"reorder_timeline: safely backing up {} events to outlier tables before deletion...",
			entries.len()
		);
		for (event_id, &(old_count, _)) in entries {
			let old_pdu_id: RawPduId = PduId { shortroomid, shorteventid: old_count }.into();
			if let Ok(json) = self.db.get_pdu_json_from_id(&old_pdu_id).await {
				self.db.backup_pdu_to_outlier(room_id, event_id, &json);
			} else {
				warn!("reorder_timeline: could not find JSON for {event_id} in pduid_pdu!");
			}
		}
	}

	async fn remove_old_timeline_entries(
		&self,
		shortroomid: ShortRoomId,
		sorted: &[OwnedEventId],
		entries: &std::collections::HashMap<OwnedEventId, (PduCount, ruma::UInt)>,
	) {
		info!("reorder_timeline: sorted {} events, removing old entries...", sorted.len());
		let mut cork = Some(self.db.db.cork());
		for (i, event_id) in sorted.iter().enumerate() {
			let &(old_count, _) = entries.get(event_id).expect("in sorted list");
			let old_pdu_id: RawPduId = PduId { shortroomid, shorteventid: old_count }.into();
			self.db.remove_from_timeline_by_id(&old_pdu_id, event_id);
			if i.saturating_add(1).is_multiple_of(2000) {
				info!(
					"reorder_timeline: removed {}/{} entries...",
					i.saturating_add(1),
					sorted.len()
				);
			}
			if i.saturating_add(1).is_multiple_of(10000) {
				drop(cork.take());
				tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				cork = Some(self.db.db.cork());
			}
		}
		drop(cork.take());
	}

	async fn reinsert_timeline_entries(
		&self,
		room_id: &RoomId,
		shortroomid: ShortRoomId,
		sorted: &[OwnedEventId],
		batch_start: u64,
		no_compute_state: bool,
	) {
		let count = sorted.len();

		let mut current_shortstatehash = if no_compute_state {
			None
		} else {
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
			let new_count = batch_start
				.saturating_add(u64::try_from(i).unwrap_or(u64::MAX))
				.saturating_add(1);
			let pdu_count = PduCount::Normal(new_count);
			let pdu_id: RawPduId = PduId { shortroomid, shorteventid: pdu_count }.into();

			let pdu = match self.db.get_pdu_in_room(Some(room_id), event_id).await {
				| Ok(p) => p,
				| Err(e) => {
					warn!(
						%event_id,
						"PduEvent missing during re-insertion (skipping): {e}"
					);
					continue;
				},
			};

			let mut json = match self.db.get_non_outlier_pdu_json(&pdu.event_id).await {
				| Ok(j) => j,
				| Err(_) => match self.db.get_pdu_json(&pdu.event_id).await {
					| Ok(j) => j,
					| Err(e) => {
						warn!(
							%event_id,
							"PDU JSON missing during re-insertion (skipping): {e}"
						);
						continue;
					},
				},
			};

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
							if let Err(e) = update_unsigned_prev_content(&mut json, &prev_state) {
								warn!(%event_id, "Failed to repair unsigned.prev_content during reorder: {e}");
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

			self.db.append_pdu(&pdu_id, &pdu, &json, pdu_count).await;
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
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
			}
		}
		drop(cork.take());
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
		use std::{collections::BTreeSet, sync::Arc};

		use futures::StreamExt;

		let room_create = self
			.services
			.state_accessor
			.room_state_get(room_id, &ruma::events::StateEventType::RoomCreate, "")
			.await
			.map_err(|_| err!(Database("Room create event not found")))?;
		let create_content: ruma::events::room::create::RoomCreateEventContent =
			serde_json::from_str(room_create.content().get())
				.map_err(|e| err!(Database("Failed to parse RoomCreateEventContent: {e}")))?;
		let room_version = create_content.room_version;

		let mut stream = std::pin::pin!(self.pdus(room_id, None));
		let mut current_shortstatehash = 0;
		let mut last_added = Arc::new(BTreeSet::new());
		let mut last_removed = Arc::new(BTreeSet::new());
		let mut processed = 0_usize;

		let mut cork = Some(self.db.db.cork());

		while let Some(Ok((_pdu_count, pdu))) = stream.next().await {
			processed = processed.saturating_add(1);

			// Resolve state mathematically
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

			// Compress and save the state delta
			let state_delta = self
				.services
				.state_compressor
				.save_state(room_id, Arc::new(compressed_state))
				.await?;

			current_shortstatehash = state_delta.shortstatehash;
			last_added = state_delta.added;
			last_removed = state_delta.removed;

			// Update the pdu shortstatehash in DB
			let shorteventid = self
				.services
				.short
				.get_or_create_shorteventid(&pdu.event_id)
				.await;

			self.services
				.state
				.set_pdu_shortstatehash(shorteventid, current_shortstatehash);

			if processed.is_multiple_of(1000) {
				info!("rebuild_state: processed {processed} events...");
				drop(cork.take());
				tokio::task::yield_now().await;
				cork = Some(self.db.db.cork());
			}
		}

		drop(cork.take());

		info!(
			"rebuild_state: finished processing {processed} events. Updating room state pointer."
		);

		// Now we must update the room's global state to match the final calculated
		// state
		let state_lock = self.services.state.mutex.lock(room_id).await;
		self.services
			.state
			.force_state_quiet(
				room_id,
				current_shortstatehash,
				last_added,
				last_removed,
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
		let mut pdus = Vec::with_capacity(capacity);

		let mut stream = std::pin::pin!(self.pdus_rev(room_id, None));
		while let Some(Ok((_count, pdu))) = stream.next().await {
			pdus.push(pdu);
			if pdus.len() >= tail {
				break;
			}
		}

		// pdus_rev returns newest first. We need oldest for true_extremities
		pdus.reverse();

		let mut ts_map = HashMap::with_capacity(pdus.len());
		let mut id_map: HashMap<OwnedEventId, u32> = HashMap::with_capacity(pdus.len());
		let mut reverse_id_map: Vec<OwnedEventId> = Vec::with_capacity(pdus.len());

		let get_or_insert_id = |event_id: &OwnedEventId,
		                        id_map: &mut HashMap<OwnedEventId, u32>,
		                        reverse_id_map: &mut Vec<OwnedEventId>|
		 -> u32 {
			if let Some(&id) = id_map.get(event_id) {
				id
			} else {
				let id = u32::try_from(reverse_id_map.len()).unwrap_or(0);
				id_map.insert(event_id.clone(), id);
				reverse_id_map.push(event_id.clone());
				id
			}
		};

		let mut graph: Vec<RoaringBitmap> = Vec::with_capacity(pdus.len());
		let mut sorted: Vec<u32> = Vec::with_capacity(pdus.len());

		for pdu in pdus {
			let event_id = pdu.event_id.clone();
			let id = get_or_insert_id(&event_id, &mut id_map, &mut reverse_id_map);
			let id_usize = usize::try_from(id).expect("u32 fits in usize");

			if id_usize >= graph.len() {
				graph.resize(id_usize.saturating_add(1), RoaringBitmap::new());
			}

			let mut prev_bitmap = RoaringBitmap::new();
			for prev in pdu.prev_events() {
				prev_bitmap.insert(get_or_insert_id(
					&prev.to_owned(),
					&mut id_map,
					&mut reverse_id_map,
				));
			}
			graph[id_usize] = prev_bitmap;
			sorted.push(id);
			ts_map.insert(event_id, pdu.origin_server_ts);
		}

		// Calculate true extremities via roaring bitmap intersections
		let true_extremities_bm = calculate_true_extremities_roaring(&graph, &sorted);

		let current_extremities = self.services.state.get_forward_extremities(room_id);
		let current_set: HashSet<_> = current_extremities.collect().await;

		let mut current_bm = RoaringBitmap::new();
		for eid in &current_set {
			if let Some(&id) = id_map.get(eid) {
				current_bm.insert(id);
			}
		}

		let phantom_tips_bm = detect_phantom_extremities_roaring(&graph, &current_bm);
		let merged_extremities_bm =
			merge_true_extremities_roaring(&true_extremities_bm, &current_bm, &phantom_tips_bm);

		let mut true_extremities_set: HashSet<OwnedEventId> = merged_extremities_bm
			.into_iter()
			.map(|id| reverse_id_map[usize::try_from(id).expect("u32 fits in usize")].clone())
			.collect();

		// Add current extremities that were outside the graph window
		for eid in &current_set {
			if !id_map.contains_key(eid) {
				true_extremities_set.insert(eid.clone());
			}
		}

		// Ensure we have timestamps for all tips we intend to keep
		for eid in &true_extremities_set {
			if !ts_map.contains_key(eid) {
				if let Ok(pdu) = self.get_pdu(eid).await {
					ts_map.insert(eid.to_owned(), pdu.origin_server_ts);
				}
			}
		}

		let mut final_extremities: Vec<OwnedEventId> = true_extremities_set.into_iter().collect();

		final_extremities.sort_by_key(|eid| ts_map.get(eid).copied().unwrap_or_default());

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
}

/// Detect stored extremities that are provably broken: they appear in the
/// window graph AND have children there (meaning they shouldn't be tips).
///
/// Returns the set of phantom tip event IDs. An empty return means the stored
/// extremities are consistent with the local DAG window.
///
/// This avoids false positives from:
/// - Stored extremities outside the tail window (unverifiable)
/// - MAX_FORWARD_EXTREMITIES capping (stored set is a subset of true tips)
pub fn detect_phantom_extremities<S1, S2, S3>(
	graph: &std::collections::HashMap<
		OwnedEventId,
		std::collections::HashSet<OwnedEventId, S2>,
		S1,
	>,
	stored_extremities: &std::collections::HashSet<OwnedEventId, S3>,
) -> Vec<OwnedEventId>
where
	S1: std::hash::BuildHasher,
	S2: std::hash::BuildHasher,
	S3: std::hash::BuildHasher,
{
	let has_children: std::collections::HashSet<&OwnedEventId> =
		graph.values().flat_map(|parents| parents.iter()).collect();

	stored_extremities
		.iter()
		.filter(|eid| has_children.contains(eid))
		.cloned()
		.collect()
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
	fn test_calculate_true_extremities_07_no_cap() {
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
		// No cap here — capping is done at the DB writer level
		// (set_forward_extremities) with MAX_FORWARD_EXTREMITIES = 10.
		assert_eq!(tips.len(), 25);
		assert_eq!(tips[0].as_str(), "$tip0");
		assert_eq!(tips[24].as_str(), "$tip24");
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

	// --- detect_phantom_extremities tests ---

	#[test]
	fn test_phantom_11_no_drift_linear() {
		// Linear chain A -> B -> C, stored extremity is C (correct)
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![b.clone()].into_iter().collect());

		let stored: std::collections::HashSet<OwnedEventId> = vec![c].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert!(phantoms.is_empty(), "correct tip should not be phantom");
	}

	#[test]
	fn test_phantom_12_real_drift() {
		// Linear chain A -> B -> C, but stored extremity is A (has children)
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![b.clone()].into_iter().collect());

		let stored: std::collections::HashSet<OwnedEventId> =
			vec![a.clone()].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert_eq!(phantoms, vec![a], "A has children and is phantom");
	}

	#[test]
	fn test_phantom_13_out_of_window_tolerated() {
		// Window only has B -> C, but stored extremity includes $old (outside window)
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();
		let old = event_id!("$old").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		graph.insert(c.clone(), vec![b.clone()].into_iter().collect());

		// $old is stored but not in the window graph at all
		let stored: std::collections::HashSet<OwnedEventId> = vec![c, old].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert!(phantoms.is_empty(), "out-of-window extremity should not be flagged");
	}

	#[test]
	fn test_phantom_14_capped_subset_ok() {
		// 25 fork tips from a root, but stored set is capped to 10
		let root = event_id!("$root").to_owned();
		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();

		let mut all_tips = Vec::new();
		for i in 0..25 {
			let id: OwnedEventId = format!("$tip{i}").try_into().unwrap();
			graph.insert(id.clone(), vec![root.clone()].into_iter().collect());
			all_tips.push(id);
		}

		// Stored set is a capped subset (first 10 tips)
		let stored: std::collections::HashSet<OwnedEventId> =
			all_tips[..10].iter().cloned().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert!(phantoms.is_empty(), "capped tips are still valid tips");
	}

	#[test]
	fn test_phantom_15_mixed_valid_and_phantom() {
		// A -> B -> C, stored = {A, C}. A is phantom (has child B), C is valid.
		let a = event_id!("$a").to_owned();
		let b = event_id!("$b").to_owned();
		let c = event_id!("$c").to_owned();

		let mut graph: HashMap<OwnedEventId, std::collections::HashSet<OwnedEventId>> =
			HashMap::new();
		graph.insert(b.clone(), vec![a.clone()].into_iter().collect());
		graph.insert(c.clone(), vec![b.clone()].into_iter().collect());

		let stored: std::collections::HashSet<OwnedEventId> =
			vec![a.clone(), c].into_iter().collect();
		let phantoms = detect_phantom_extremities(&graph, &stored);
		assert_eq!(phantoms, vec![a], "only A is phantom, C is valid");
	}

	#[tokio::test]
	async fn test_upgrade_outlier_rejects_soft_failed_and_rejected_events() {
		let _ = rustls::crypto::ring::default_provider().install_default();

		use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

		use conduwuit::{
			Server,
			config::Config,
			log::{Log, LogLevelReloadHandles, capture},
		};
		use figment::providers::Format;
		use ruma::{CanonicalJsonObject, CanonicalJsonValue, RoomId, ServerName, event_id};

		use crate::rooms::timeline::PduEvent;

		struct TempDbGuard {
			path: PathBuf,
		}

		impl Drop for TempDbGuard {
			fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
		}

		static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 =
			std::sync::atomic::AtomicU64::new(0);
		let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		let db_path =
			std::env::temp_dir().join(format!("conduwuit_test_db_upgrade_outlier_{count}"));
		let _ = std::fs::remove_dir_all(&db_path);

		let _guard = TempDbGuard { path: db_path.clone() };

		let figment = figment::Figment::new().merge(figment::providers::Toml::string(&format!(
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

		let services = crate::Services::build(server.clone())
			.await
			.expect("failed to build services");
		let services = services.start().await.expect("failed to start services");

		let room_id = RoomId::parse("!test_room:test.conduwuit.local").unwrap();
		let create_event_id = event_id!("$create_event_id");
		let origin = ServerName::parse("test.conduwuit.local").unwrap();

		let mut create_json = CanonicalJsonObject::new();
		create_json
			.insert("type".to_owned(), CanonicalJsonValue::String("m.room.create".to_owned()));
		create_json.insert(
			"sender".to_owned(),
			CanonicalJsonValue::String("@creator:test.conduwuit.local".to_owned()),
		);
		create_json.insert("state_key".to_owned(), CanonicalJsonValue::String("".to_owned()));
		let mut content_map = BTreeMap::new();
		content_map
			.insert("room_version".to_owned(), CanonicalJsonValue::String("10".to_owned()));
		content_map.insert(
			"creator".to_owned(),
			CanonicalJsonValue::String("@creator:test.conduwuit.local".to_owned()),
		);
		create_json.insert("content".to_owned(), CanonicalJsonValue::Object(content_map));
		create_json.insert(
			"event_id".to_owned(),
			CanonicalJsonValue::String(create_event_id.as_str().to_owned()),
		);
		create_json.insert(
			"room_id".to_owned(),
			CanonicalJsonValue::String(room_id.as_str().to_owned()),
		);
		create_json
			.insert("origin_server_ts".to_owned(), CanonicalJsonValue::Integer(123456789.into()));
		create_json.insert("prev_events".to_owned(), CanonicalJsonValue::Array(vec![]));
		create_json.insert("auth_events".to_owned(), CanonicalJsonValue::Array(vec![]));
		create_json.insert("depth".to_owned(), CanonicalJsonValue::Integer(1.into()));
		let mut hashes = CanonicalJsonObject::new();
		hashes.insert("sha256".to_owned(), CanonicalJsonValue::String("".to_owned()));
		create_json.insert("hashes".to_owned(), CanonicalJsonValue::Object(hashes));
		create_json.insert(
			"signatures".to_owned(),
			CanonicalJsonValue::Object(CanonicalJsonObject::new()),
		);

		let create_pdu =
			PduEvent::from_id_val(&create_event_id, create_json.clone(), Some(&room_id)).unwrap();

		let test_event_id = event_id!("$test_event_id");
		let mut test_json = CanonicalJsonObject::new();
		test_json
			.insert("type".to_owned(), CanonicalJsonValue::String("m.room.message".to_owned()));
		test_json.insert(
			"sender".to_owned(),
			CanonicalJsonValue::String("@creator:test.conduwuit.local".to_owned()),
		);
		test_json.insert("content".to_owned(), CanonicalJsonValue::Object(BTreeMap::new()));
		test_json.insert(
			"event_id".to_owned(),
			CanonicalJsonValue::String(test_event_id.as_str().to_owned()),
		);
		test_json.insert(
			"room_id".to_owned(),
			CanonicalJsonValue::String(room_id.as_str().to_owned()),
		);
		test_json
			.insert("origin_server_ts".to_owned(), CanonicalJsonValue::Integer(123456790.into()));
		test_json.insert("prev_events".to_owned(), CanonicalJsonValue::Array(vec![]));
		test_json.insert("auth_events".to_owned(), CanonicalJsonValue::Array(vec![]));
		test_json.insert("depth".to_owned(), CanonicalJsonValue::Integer(2.into()));
		let mut hashes = CanonicalJsonObject::new();
		hashes.insert("sha256".to_owned(), CanonicalJsonValue::String("".to_owned()));
		test_json.insert("hashes".to_owned(), CanonicalJsonValue::Object(hashes));
		test_json.insert(
			"signatures".to_owned(),
			CanonicalJsonValue::Object(CanonicalJsonObject::new()),
		);

		let test_pdu =
			PduEvent::from_id_val(&test_event_id, test_json.clone(), Some(&room_id)).unwrap();
		let btree_val = test_json
			.into_iter()
			.collect::<BTreeMap<String, CanonicalJsonValue>>();

		// Mark the event as rejected to simulate a previously failed validation
		services
			.rooms
			.pdu_metadata
			.mark_event_rejected(&test_event_id);

		// When rescue-room calls this with skip_soft_fail=false, it MUST return an
		// error.
		let result = services
			.rooms
			.event_handler
			.upgrade_outlier_to_timeline_pdu(
				test_pdu,
				btree_val,
				&create_pdu,
				&origin,
				&room_id,
				false, // skip_soft_fail MUST be false
				true,  // is_forward_extremity (simulating rescue-room forcing a promotion)
			)
			.await;

		assert!(
			result.is_err(),
			"upgrade_outlier_to_timeline_pdu must reject events that are marked rejected when \
			 skip_soft_fail is false"
		);
	}

	#[tokio::test]
	#[ignore]
	async fn test_dags_import() {
		let _ = rustls::crypto::ring::default_provider().install_default();

		use std::{
			collections::BTreeMap,
			fs::File,
			io::{BufRead, BufReader},
			path::PathBuf,
			sync::Arc,
		};

		use conduwuit::{
			Server,
			config::Config,
			log::{Log, LogLevelReloadHandles, capture},
		};
		use figment::providers::Format;
		use ruma::{RoomId, ServerName};
		use serde_json::Value;

		use crate::rooms::timeline::PduEvent;

		struct TempDbGuard {
			path: PathBuf,
		}

		impl Drop for TempDbGuard {
			fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
		}

		static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 =
			std::sync::atomic::AtomicU64::new(0);
		let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_dag_import_{count}"));
		let _ = std::fs::remove_dir_all(&db_path);

		let _guard = TempDbGuard { path: db_path.clone() };

		let figment = figment::Figment::new().merge(figment::providers::Toml::string(&format!(
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

		let services = crate::Services::build(server.clone())
			.await
			.expect("failed to build services");
		let services = services.start().await.expect("failed to start services");

		let room_id = RoomId::parse("!UbCmIlGTHNIgIRZcpt:nheko.im").unwrap();
		let origin = ServerName::parse("nheko.im").unwrap();

		let mut create_event_opt: Option<PduEvent> = None;

		let mut imported = 0;
		let mut failed = 0;

		if let Ok(file) = File::open(
			"/run/media/shane/shane4tb-ent/dags/remote-dag-UbCmIlGTHNIgIRZcpt_nheko.im-v5-nutra.\
			 tk-d153360-383640.jsonl",
		) {
			let reader = BufReader::new(file);
			for line in reader.lines() {
				if let Ok(line) = line {
					if let Ok(val) = serde_json::from_str::<Value>(&line) {
						if let Value::Object(map) = val {
							let event_id_str = map
								.get("event_id")
								.and_then(|v| v.as_str())
								.map(|s| s.to_owned());
							let mut eid: Option<OwnedEventId> = None;
							if let Some(s) = &event_id_str {
								eid = OwnedEventId::try_from(s.as_str()).ok();
							} else if let Some(sha256) = map
								.get("hashes")
								.and_then(|h| h.get("sha256"))
								.and_then(|v| v.as_str())
							{
								eid = OwnedEventId::try_from(format!(
									"${}",
									sha256
										.replace('+', "-")
										.replace('/', "_")
										.trim_end_matches('=')
								))
								.ok();
							}

							if let Some(event_id) = eid {
								if let Ok(c_json) = serde_json::from_value::<CanonicalJsonObject>(
									Value::Object(map.clone()),
								) {
									if create_event_opt.is_none() {
										if let Ok(pdu) = PduEvent::from_id_val(
											&event_id,
											c_json.clone(),
											Some(&room_id),
										) {
											if pdu.kind == TimelineEventType::RoomCreate {
												create_event_opt = Some(pdu);
											}
										}
									}

									let btree_val = c_json
										.into_iter()
										.collect::<BTreeMap<String, ruma::CanonicalJsonValue>>();

									match services
										.rooms
										.event_handler
										.handle_outlier_pdu(
											&origin,
											create_event_opt.as_ref(),
											&event_id,
											&room_id,
											btree_val,
											false,
											true, // skip_sig_verify
											None,
										)
										.await
									{
										| Ok(_) => {
											imported += 1;
										},
										| Err(e) => {
											println!(
												"handle_outlier_pdu error for {event_id}: {e:?}"
											);
											failed += 1;
										},
									}
								}
							}
						}
					}
				}
			}
		}

		println!("Imported: {}, Failed: {}", imported, failed);
	}

	async fn run_test_handle_outlier_pdu(
		room_version_str: &str,
		include_room_id: bool,
		room_id_str: &str,
	) {
		let _ = rustls::crypto::ring::default_provider().install_default();

		use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

		use conduwuit::{
			Server,
			config::Config,
			log::{Log, LogLevelReloadHandles, capture},
		};
		use figment::providers::Format;
		use ruma::{
			CanonicalJsonObject, CanonicalJsonValue, RoomId, RoomVersionId, ServerName, event_id,
		};

		struct TempDbGuard {
			path: PathBuf,
		}

		impl Drop for TempDbGuard {
			fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
		}

		static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 =
			std::sync::atomic::AtomicU64::new(0);
		let count = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		let db_path = std::env::temp_dir().join(format!("conduwuit_test_db_outlier_{count}"));
		let _ = std::fs::remove_dir_all(&db_path);

		let guard = TempDbGuard { path: db_path.clone() };

		let figment = figment::Figment::new().merge(figment::providers::Toml::string(&format!(
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

		let services = crate::Services::build(server.clone())
			.await
			.expect("failed to build services");
		let services = services.start().await.expect("failed to start services");

		let room_id = RoomId::parse(room_id_str).unwrap();
		let create_event_id = event_id!("$create_event_id");
		let join_event_id = event_id!("$join_event_id");
		let room_version_id = RoomVersionId::try_from(room_version_str).unwrap();
		let origin = ServerName::parse("test.conduwuit.local").unwrap();

		// Create a mock create event
		let mut create_json = CanonicalJsonObject::new();
		create_json
			.insert("type".to_owned(), CanonicalJsonValue::String("m.room.create".to_owned()));
		create_json.insert(
			"sender".to_owned(),
			CanonicalJsonValue::String("@creator:test.conduwuit.local".to_owned()),
		);
		create_json.insert("state_key".to_owned(), CanonicalJsonValue::String("".to_owned()));
		let mut content_map = BTreeMap::new();
		content_map.insert(
			"room_version".to_owned(),
			CanonicalJsonValue::String(room_version_str.to_owned()),
		);
		if room_version_str == "5" || room_version_str == "9" || room_version_str == "10" {
			content_map.insert(
				"creator".to_owned(),
				CanonicalJsonValue::String("@creator:test.conduwuit.local".to_owned()),
			);
		}
		create_json.insert("content".to_owned(), CanonicalJsonValue::Object(content_map));
		create_json.insert(
			"event_id".to_owned(),
			CanonicalJsonValue::String(create_event_id.as_str().to_owned()),
		);
		create_json
			.insert("origin_server_ts".to_owned(), CanonicalJsonValue::Integer(123456789.into()));
		create_json.insert("prev_events".to_owned(), CanonicalJsonValue::Array(vec![]));
		create_json.insert("auth_events".to_owned(), CanonicalJsonValue::Array(vec![]));
		create_json.insert("depth".to_owned(), CanonicalJsonValue::Integer(1.into()));
		let mut hashes = CanonicalJsonObject::new();
		hashes.insert(
			"sha256".to_owned(),
			CanonicalJsonValue::String("mock_sha256_hash_value_1".to_owned()),
		);
		create_json.insert("hashes".to_owned(), CanonicalJsonValue::Object(hashes));
		create_json.insert(
			"signatures".to_owned(),
			CanonicalJsonValue::Object(CanonicalJsonObject::new()),
		);
		if include_room_id {
			create_json.insert(
				"room_id".to_owned(),
				CanonicalJsonValue::String(room_id.as_str().to_owned()),
			);
		}

		// Create a mock join event for the creator
		let mut join_json = CanonicalJsonObject::new();
		join_json
			.insert("type".to_owned(), CanonicalJsonValue::String("m.room.member".to_owned()));
		join_json.insert(
			"sender".to_owned(),
			CanonicalJsonValue::String("@creator:test.conduwuit.local".to_owned()),
		);
		join_json.insert(
			"state_key".to_owned(),
			CanonicalJsonValue::String("@creator:test.conduwuit.local".to_owned()),
		);
		join_json.insert(
			"content".to_owned(),
			CanonicalJsonValue::Object(
				vec![("membership".to_owned(), CanonicalJsonValue::String("join".to_owned()))]
					.into_iter()
					.collect(),
			),
		);
		join_json.insert(
			"event_id".to_owned(),
			CanonicalJsonValue::String(join_event_id.as_str().to_owned()),
		);
		join_json
			.insert("origin_server_ts".to_owned(), CanonicalJsonValue::Integer(123456790.into()));
		join_json.insert(
			"prev_events".to_owned(),
			CanonicalJsonValue::Array(vec![CanonicalJsonValue::String(
				create_event_id.as_str().to_owned(),
			)]),
		);
		join_json.insert("depth".to_owned(), CanonicalJsonValue::Integer(2.into()));
		let mut hashes = CanonicalJsonObject::new();
		hashes.insert(
			"sha256".to_owned(),
			CanonicalJsonValue::String("mock_sha256_hash_value_2".to_owned()),
		);
		join_json.insert("hashes".to_owned(), CanonicalJsonValue::Object(hashes));
		join_json.insert(
			"signatures".to_owned(),
			CanonicalJsonValue::Object(CanonicalJsonObject::new()),
		);
		if include_room_id {
			join_json.insert(
				"room_id".to_owned(),
				CanonicalJsonValue::String(room_id.as_str().to_owned()),
			);
		}

		// Auth events for room version <= 11 MUST contain the create event.
		// For room version 12, it is explicitly forbidden and must be empty.
		if room_version_str == "12" {
			join_json.insert("auth_events".to_owned(), CanonicalJsonValue::Array(vec![]));
		} else {
			join_json.insert(
				"auth_events".to_owned(),
				CanonicalJsonValue::Array(vec![CanonicalJsonValue::String(
					create_event_id.as_str().to_owned(),
				)]),
			);
		}

		// Test Case 1: Create event missing. Handling the join event should FAIL.
		let handle_missing_res = services
			.rooms
			.event_handler
			.handle_outlier_pdu::<PduEvent>(
				&origin,
				None,
				join_event_id,
				&room_id,
				join_json.clone(),
				false,
				true, // skip_sig_verify
				Some(&room_version_id),
			)
			.await;
		assert!(
			handle_missing_res.is_err(),
			"Expected handling outlier join event to fail when create event is missing, but it \
			 succeeded: {:?}",
			handle_missing_res
		);

		// Test Case 2: Create event exists.
		// First, handle the create event as an outlier.
		let handle_create_res = services
			.rooms
			.event_handler
			.handle_outlier_pdu::<PduEvent>(
				&origin,
				None,
				create_event_id,
				&room_id,
				create_json,
				false,
				true, // skip_sig_verify
				Some(&room_version_id),
			)
			.await;
		println!(
			"Version {} Create Event handle result: {:?}",
			room_version_str, handle_create_res
		);
		let create_pdu = handle_create_res
			.as_ref()
			.expect("failed to handle create PDU")
			.0
			.clone();
		println!("Created event room_id: {:?}", create_pdu.room_id());
		println!("Created event sender: {:?}", create_pdu.sender());

		// Now, handle the join event as an outlier.
		// For version 12, we must pass the create event since referencing it in
		// auth_events is forbidden. For version <= 11, we pass None to test the
		// fallback lookup path!
		let create_event_param = if room_version_str == "12" {
			Some(&create_pdu)
		} else {
			None
		};

		let handle_success_res = services
			.rooms
			.event_handler
			.handle_outlier_pdu::<PduEvent>(
				&origin,
				create_event_param,
				join_event_id,
				&room_id,
				join_json,
				false,
				true, // skip_sig_verify
				Some(&room_version_id),
			)
			.await;
		println!(
			"Version {} Join Event handle result: {:?}",
			room_version_str, handle_success_res
		);
		assert!(
			handle_success_res.is_ok(),
			"Expected handling outlier join event to succeed when create event is present, but \
			 it failed: {:?}",
			handle_success_res.err()
		);

		// Verify we can retrieve the join event from the outlier database
		let db_res = services
			.rooms
			.timeline
			.db
			.get_pdu_in_room(Some(&room_id), join_event_id)
			.await;
		assert!(db_res.is_ok(), "Expected to retrieve outlier join event: {:?}", db_res.err());
		let pdu = db_res.unwrap();
		assert_eq!(pdu.event_id, join_event_id);

		// Clean up
		drop(guard);
		let _ = server.shutdown();
	}

	#[tokio::test]
	async fn test_backup_pdu_to_outlier_v5() {
		run_test_handle_outlier_pdu("5", true, "!create_event_id:test.conduwuit.local").await;
	}

	#[tokio::test]
	async fn test_backup_pdu_to_outlier_v9() {
		run_test_handle_outlier_pdu("9", true, "!create_event_id:test.conduwuit.local").await;
	}

	#[tokio::test]
	async fn test_backup_pdu_to_outlier_v10() {
		run_test_handle_outlier_pdu("10", true, "!create_event_id:test.conduwuit.local").await;
	}

	#[tokio::test]
	async fn test_backup_pdu_to_outlier_v11() {
		run_test_handle_outlier_pdu("11", true, "!create_event_id:test.conduwuit.local").await;
	}

	#[tokio::test]
	async fn test_backup_pdu_to_outlier_v12() {
		run_test_handle_outlier_pdu("12", false, "!create_event_id").await;
	}
}

pub fn merge_true_extremities<S: ::std::hash::BuildHasher>(
	true_extremities: Vec<&EventId>,
	current_set: &std::collections::HashSet<OwnedEventId, S>,
	phantom_tips: &[OwnedEventId],
) -> std::collections::HashSet<OwnedEventId> {
	let mut true_extremities_set: std::collections::HashSet<OwnedEventId> = true_extremities
		.into_iter()
		.map(ToOwned::to_owned)
		.collect();

	for eid in current_set {
		if !phantom_tips.contains(eid) {
			true_extremities_set.insert(eid.clone());
		}
	}

	true_extremities_set
}

#[cfg(test)]
mod tests_merge {
	use std::collections::HashSet;

	use ruma::{OwnedEventId, event_id};

	use super::*;

	#[test]
	fn test_merge_true_extremities() {
		let e1 = event_id!("$1").to_owned();
		let e2 = event_id!("$2").to_owned();
		let e3 = event_id!("$3").to_owned();
		let e4 = event_id!("$4").to_owned();

		// newly discovered true extremity
		let true_exts = vec![&*e1];

		// current tips in DB
		let current_set: HashSet<OwnedEventId> = vec![e2.clone(), e3.clone(), e4.clone()]
			.into_iter()
			.collect();

		// phantom tips: e2 and e3 are phantoms
		let phantoms = vec![e2.clone(), e3.clone()];

		let result = merge_true_extremities(true_exts, &current_set, &phantoms);

		// Result should be e1 (true) and e4 (preserved from current_set because it's
		// not a phantom)
		assert_eq!(result.len(), 2);
		assert!(result.contains(&e1));
		assert!(result.contains(&e4));
		assert!(!result.contains(&e2));
		assert!(!result.contains(&e3));
	}
}

#[must_use]
pub fn detect_phantom_extremities_roaring(
	graph: &[roaring::RoaringBitmap],
	stored_extremities: &roaring::RoaringBitmap,
) -> roaring::RoaringBitmap {
	let mut has_children = roaring::RoaringBitmap::new();
	for parents in graph {
		has_children |= parents;
	}

	stored_extremities & has_children
}

#[must_use]
pub fn calculate_true_extremities_roaring(
	graph: &[roaring::RoaringBitmap],
	sorted: &[u32],
) -> roaring::RoaringBitmap {
	let mut has_children = roaring::RoaringBitmap::new();
	for parents in graph {
		has_children |= parents;
	}

	let mut true_extremities = roaring::RoaringBitmap::new();
	for &id in sorted {
		if !has_children.contains(id) {
			true_extremities.insert(id);
		}
	}

	if true_extremities.is_empty() {
		if let Some(&last_id) = sorted.last() {
			true_extremities.insert(last_id);
		}
	}

	true_extremities
}

#[must_use]
pub fn merge_true_extremities_roaring(
	true_extremities: &roaring::RoaringBitmap,
	current_set: &roaring::RoaringBitmap,
	phantom_tips: &roaring::RoaringBitmap,
) -> roaring::RoaringBitmap {
	let mut true_extremities_set = true_extremities.clone();
	// or `std::ops::Sub::sub()`
	let valid_current =
		<&roaring::RoaringBitmap as std::ops::Sub>::sub(current_set, phantom_tips);
	true_extremities_set |= valid_current;
	true_extremities_set
}

#[cfg(test)]
mod tests_roaring {
	use roaring::RoaringBitmap;

	use super::*;

	#[test]
	fn test_calculate_true_extremities_roaring_fork() {
		let mut graph = vec![RoaringBitmap::new(); 3];
		graph[1].insert(0); // 1 depends on 0
		graph[2].insert(0); // 2 depends on 0

		let sorted = vec![0, 1, 2];
		let tips = calculate_true_extremities_roaring(&graph, &sorted);

		let mut expected = RoaringBitmap::new();
		expected.insert(1);
		expected.insert(2);

		assert_eq!(tips, expected);
	}

	#[test]
	fn test_detect_phantom_extremities_roaring() {
		let mut graph = vec![RoaringBitmap::new(); 3];
		graph[1].insert(0);
		graph[2].insert(1);

		let mut stored_extremities = RoaringBitmap::new();
		stored_extremities.insert(0); // 0 is a phantom tip because it's a parent
		stored_extremities.insert(2); // 2 is a true tip

		let phantoms = detect_phantom_extremities_roaring(&graph, &stored_extremities);

		let mut expected = RoaringBitmap::new();
		expected.insert(0); // 0 should be detected as phantom

		assert_eq!(phantoms, expected);
	}

	#[test]
	fn test_merge_true_extremities_roaring() {
		let mut true_exts = RoaringBitmap::new();
		true_exts.insert(1);

		let mut current_set = RoaringBitmap::new();
		current_set.insert(2);
		current_set.insert(3);
		current_set.insert(4);

		let mut phantoms = RoaringBitmap::new();
		phantoms.insert(2);
		phantoms.insert(3);

		let result = merge_true_extremities_roaring(&true_exts, &current_set, &phantoms);

		let mut expected = RoaringBitmap::new();
		expected.insert(1); // from true_exts
		expected.insert(4); // from current_set (not a phantom)

		assert_eq!(result, expected);
	}
}
