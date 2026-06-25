use std::collections::{HashMap, HashSet};

use conduwuit_core::{
	Result, info,
	matrix::{
		Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	warn,
};
use futures::StreamExt;
use ruma::{OwnedEventId, RoomId};

use super::{Service, extremities::calculate_true_extremities, metadata::EventMetadata};
use crate::rooms::short::ShortEventId;

/// Statistics returned from `reindex_short`.
#[derive(Debug, Default)]
pub struct ReindexStats {
	pub total_events: usize,
	pub missing_pdu: usize,
	pub hash_mismatches: usize,
	pub repaired_short_ids: usize,
	pub repaired_metadata: usize,
	pub repaired_prev_events: usize,
	pub repaired_auth_events: usize,
	pub repaired_auth_chains: usize,
	pub repaired_topo_index: usize,
	pub repaired_relations: usize,
	pub repaired_references: usize,
	pub repaired_search_index: usize,
	pub extremities_updated: bool,
	pub extremities_count: usize,
}

impl std::fmt::Display for ReindexStats {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"total={}, missing_pdu={}, hash_mismatches={}, short_ids={}, metadata={}, \
			 prev_events={}, auth_events={}, auth_chains={}, topo_index={}, relations={}, \
			 references={}, search={}, extremities={} (updated={})",
			self.total_events,
			self.missing_pdu,
			self.hash_mismatches,
			self.repaired_short_ids,
			self.repaired_metadata,
			self.repaired_prev_events,
			self.repaired_auth_events,
			self.repaired_auth_chains,
			self.repaired_topo_index,
			self.repaired_relations,
			self.repaired_references,
			self.repaired_search_index,
			self.extremities_count,
			self.extremities_updated,
		)
	}
}

impl Service {
	/// Sweep all events in a room and repopulate any missing derived data
	/// from the `eventid_pdu` source of truth.
	///
	/// This is safe to run at any time — it only writes missing entries and
	/// never overwrites existing data.
	pub async fn reindex_short(&self, room_id: &RoomId) -> Result<ReindexStats> {
		let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;
		let state_lock = self.services.state.mutex.lock(room_id).await;
		let room_version = self.services.state.get_room_version(room_id).await?;
		let mut stats = ReindexStats::default();

		info!("reindex_short: collecting all events for {room_id}...");

		// Phase 1: Collect all events from the stream index
		let mut events: Vec<(PduCount, OwnedEventId)> = Vec::new();
		{
			let mut stream = std::pin::pin!(self.db.room_event_ids_rev(room_id, None));
			while let Some(Ok(eid)) = stream.next().await {
				if let Ok(count) = self.get_pdu_count(&eid).await {
					events.push((count, eid));
				}
			}
		}
		events.reverse(); // Forward order (oldest first)
		stats.total_events = events.len();
		info!("reindex_short: found {} events in stream index", events.len());

		// Phase 2: For each event, read PDU JSON and repair derived data
		let cork = self.db.db.cork();

		// Graph for extremity computation
		let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> = HashMap::new();
		// Auth chain cache for incremental computation
		let mut auth_chain_cache: HashMap<ShortEventId, Vec<ShortEventId>> = HashMap::new();

		for (count, event_id) in &events {
			let Ok((pdu, json)) = self.db.get_from_eventid_pdu(event_id).await else {
				stats.missing_pdu = stats.missing_pdu.saturating_add(1);
				continue;
			};

			// --- Event ID hash validation ---
			if let Ok(expected_id) =
				conduwuit_core::matrix::event::gen_event_id(&json, &room_version)
			{
				if expected_id != *event_id {
					warn!("reindex_short: hash mismatch for {event_id}: expected {expected_id}");
					stats.hash_mismatches = stats.hash_mismatches.saturating_add(1);
				}
			}

			// --- Short ID mappings ---
			let was_missing = self
				.services
				.short
				.get_shorteventid(event_id)
				.await
				.is_err();
			let short_eid = self
				.services
				.short
				.get_or_create_shorteventid(event_id)
				.await;
			if was_missing {
				stats.repaired_short_ids = stats.repaired_short_ids.saturating_add(1);
			}

			let pdu_id: RawPduId = PduId { shortroomid, shorteventid: *count }.into();

			// --- eventid_metadata ---
			if self.db.get_event_metadata(event_id).await.is_err() {
				let meta = EventMetadata {
					short_room_id: shortroomid,
					origin_server_ts: pdu.origin_server_ts().0,
					depth: pdu.depth(),
					pdu_count: Some(count.into_unsigned()),
					..Default::default()
				};
				if let Ok(bytes) = bincode::serialize(&meta) {
					self.db.store_eventid_metadata(event_id.as_bytes(), bytes);
					stats.repaired_metadata = stats.repaired_metadata.saturating_add(1);
				}
			}

			// --- shorteventid_shortprevevents ---
			let prev_event_ids: Vec<OwnedEventId> =
				pdu.prev_events().map(ToOwned::to_owned).collect();

			if self
				.db
				.get_shortprevevents(short_eid)
				.await
				.map_or(true, |v| v.is_empty())
				&& !prev_event_ids.is_empty()
			{
				let mut prev_shorts = Vec::with_capacity(prev_event_ids.len());
				for prev_id in &prev_event_ids {
					prev_shorts.push(
						self.services
							.short
							.get_or_create_shorteventid(prev_id)
							.await,
					);
				}
				self.db.store_shortprevevents(short_eid, &prev_shorts);
				stats.repaired_prev_events = stats.repaired_prev_events.saturating_add(1);
			}

			// --- shorteventid_shortauthevents ---
			let auth_event_ids: Vec<OwnedEventId> =
				pdu.auth_events().map(ToOwned::to_owned).collect();

			let auth_shorts: Vec<ShortEventId> = {
				let mut shorts = Vec::with_capacity(auth_event_ids.len());
				for auth_id in &auth_event_ids {
					shorts.push(
						self.services
							.short
							.get_or_create_shorteventid(auth_id)
							.await,
					);
				}
				shorts
			};

			if self
				.db
				.get_shortauthevents(short_eid)
				.await
				.map_or(true, |v| v.is_empty())
				&& !auth_shorts.is_empty()
			{
				self.db.store_shortauthevents(short_eid, &auth_shorts);
				stats.repaired_auth_events = stats.repaired_auth_events.saturating_add(1);
			}

			// --- shorteventid_authchain (incremental) ---
			// auth_chain[e] = auth_events(e) ∪ ⋃(auth_chain[ae])
			if self
				.services
				.auth_chain
				.get_cached_eventid_authchain(&[short_eid])
				.await
				.is_err()
			{
				let mut full_chain: HashSet<ShortEventId> = HashSet::new();
				for &auth_short in &auth_shorts {
					full_chain.insert(auth_short);
					// Use our local cache (built during this sweep) for ancestors
					if let Some(ancestor_chain) = auth_chain_cache.get(&auth_short) {
						full_chain.extend(ancestor_chain.iter().copied());
					}
				}
				let mut chain_vec: Vec<ShortEventId> = full_chain.into_iter().collect();
				chain_vec.sort_unstable();
				chain_vec.dedup();

				self.services
					.auth_chain
					.cache_auth_chain_vec(vec![short_eid], &chain_vec);
				auth_chain_cache.insert(short_eid, chain_vec);
				stats.repaired_auth_chains = stats.repaired_auth_chains.saturating_add(1);
			} else if let Ok(existing) = self
				.services
				.auth_chain
				.get_cached_eventid_authchain(&[short_eid])
				.await
			{
				// Populate local cache for descendants
				auth_chain_cache.insert(short_eid, existing.to_vec());
			}

			// --- tofrom_relation ---
			if let Ok(content) = pdu.get_content::<super::ExtractRelatesToEventId>() {
				if let Ok(related_count) = self.get_pdu_count(&content.relates_to.event_id).await
				{
					self.services
						.pdu_metadata
						.add_relation(*count, related_count);
					stats.repaired_relations = stats.repaired_relations.saturating_add(1);
				}
			}

			// --- referencedevents ---
			if !prev_event_ids.is_empty() {
				self.services
					.pdu_metadata
					.mark_as_referenced(room_id, prev_event_ids.iter().map(AsRef::as_ref));
				stats.repaired_references = stats.repaired_references.saturating_add(1);
			}

			// --- tokenids (search index) ---
			self.index_pdu_search(shortroomid, &pdu_id, &pdu);
			stats.repaired_search_index = stats.repaired_search_index.saturating_add(1);

			// Build graph for extremity computation
			graph.insert(event_id.clone(), prev_event_ids.into_iter().collect());
		}

		// --- Forward extremities (roomid_pduleaves) ---
		let event_set: HashSet<&OwnedEventId> = events.iter().map(|(_, e)| e).collect();
		for parents in graph.values_mut() {
			parents.retain(|prev_id| event_set.contains(prev_id));
		}

		let sorted: Vec<OwnedEventId> = events.iter().map(|(_, e)| e.clone()).collect();
		let tips = calculate_true_extremities(&graph, &sorted);

		let extremity_count = tips.len();
		if !tips.is_empty() {
			self.services
				.state
				.set_forward_extremities(
					room_id,
					tips.into_iter().map(ToOwned::to_owned),
					&state_lock,
				)
				.await;
			stats.extremities_count = extremity_count;
			stats.extremities_updated = true;
		}

		drop(cork);
		drop(state_lock);

		info!("reindex_short: completed for {room_id}: {stats}");
		Ok(stats)
	}
}
