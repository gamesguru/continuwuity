use std::{
	collections::{BTreeSet, HashMap, HashSet},
	sync::Arc,
};

use conduwuit_core::{Result, debug, info, matrix::event::Event, warn};
use futures::StreamExt;
use ruma::{OwnedEventId, RoomId, events::TimelineEventType};

use super::{Service, TimelineStateResolver};
use crate::rooms;

impl Service {
	/// Incrementally rebuilds the true state of the room by iterating through
	/// the timeline in its current PduCount order, resolving the state for
	/// each event, and updating the DB. This heals a fractured room state
	/// without re-ordering events or generating new PduCounts, preventing UI
	/// sync spam.
	#[tracing::instrument(skip(self), level = "info")]
	pub async fn rebuild_state(&self, room_id: &RoomId) -> Result<()> {
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
					"rebuild_state: {}/{} events | single:{} none:{} resolved:{} | \
					 cumulative_resolve: {:?} | elapsed: {:?}",
					processed,
					total_events,
					single_parent_count,
					no_parent_count,
					fork_resolve_count,
					cumulative_resolve_time,
					rebuild_start.elapsed(),
				);
			}

			let pdu = self.get_pdu(eid).await?;
			let loop_start = std::time::Instant::now();

			// Find parent state — only consider parents that exist in our event set
			let event_set_refs: HashSet<&ruma::EventId> =
				event_set.iter().map(|id| &***id).collect();
			let state_before = self
				.resolve_state_before(
					&mut TimelineStateResolver {
						room_id,
						room_version: &room_version,
						event_set: &event_set_refs,
						ssh_cache: &ssh_cache,
						resolved_state_cache: &mut resolved_state_cache,
						empty_ssh,
					},
					&pdu,
				)
				.await?;

			// Update statistics
			let prev_sshs: Vec<u64> = prev_events
				.iter()
				.filter(|prev_id| event_set.contains(prev_id))
				.filter_map(|prev_id| ssh_cache.get(prev_id).copied())
				.collect();
			let mut unique_sshs = prev_sshs.clone();
			unique_sshs.sort_unstable();
			unique_sshs.dedup();
			match unique_sshs.len() {
				| 1 => {
					single_parent_count = single_parent_count.saturating_add(1);
				},
				| 0 => {
					no_parent_count = no_parent_count.saturating_add(1);
				},
				| _ => {
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
				},
			}

			let mut state_after = state_before;

			if state_key.is_some() {
				// State event — need to compute the state diff
				let pdu = self.get_pdu(eid).await?;
				let (_, mut json) = self.db.get_from_eventid_pdu(eid).await?;
				let pdu_id: conduwuit_core::matrix::pdu::RawPduId =
					conduwuit_core::matrix::pdu::PduId {
						shortroomid: self.services.short.get_or_create_shortroomid(room_id).await,
						shorteventid: conduwuit_core::matrix::pdu::PduCount::Normal(0),
					}
					.into();
				self.compute_state_for_event(&pdu, eid, &mut json, &mut state_after, &pdu_id)
					.await;
			} else {
				// Non-state event — just set the pdu_shortstatehash
				let shorteventid = self.services.short.get_or_create_shorteventid(eid).await;
				self.services
					.state
					.set_pdu_shortstatehash(shorteventid, state_before);
			}

			ssh_cache.insert(eid.clone(), state_after);
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
			 none:{no_parent_count} resolved:{fork_resolve_count} | cumulative_resolve: {:?}",
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
				if let Some(full_state) = self.services.state_compressor.get_full_state(ssh).await
				{
					for entry in full_state.as_ref() {
						all_compressed.insert(*entry);
					}
				}
			}

			// Build ssk -> set of shorteventid values to detect conflicts
			let mut ssk_values: HashMap<u64, HashSet<u64>> = HashMap::new();
			for bytes in &all_compressed {
				let (ssk, sei) = rooms::state_compressor::parse_compressed_state_event(*bytes);
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

		let (total_added, total_removed) = self
			.services
			.state_compressor
			.diff_full_state(original_room_shortstatehash.unwrap_or(0), current_shortstatehash)
			.await;

		// Now update the room's global state to match final calculated state
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
}
