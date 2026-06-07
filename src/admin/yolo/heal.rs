use std::{
	collections::{HashMap, HashSet, VecDeque},
	fmt::Write,
};

use conduwuit::{
	Result, err, info,
	matrix::{Event, pdu::PduEvent},
	state_res, warn,
};
use futures::{FutureExt, StreamExt, future::ready};
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName, RoomId,
	events::{StateEventType, TimelineEventType},
};

use crate::admin_command;

#[admin_command]
#[allow(clippy::fn_params_excessive_bools)]
pub(super) async fn rescue_room(
	&self,
	room_id: OwnedRoomId,
	force: bool,
	nuclear: bool,
	all: bool,
	timeline_limit: Option<usize>,
	reorder: bool,
	heal_from: Vec<OwnedServerName>,
) -> Result {
	self.bail_restricted()?;

	// --heal-from implies --force (no point doing full state_res per
	// outlier when we're about to overwrite state from the backbone)
	let force = force || !heal_from.is_empty();

	if all {
		let mut room_ids: HashSet<OwnedRoomId> = HashSet::new();
		let mut outliers = self.services.rooms.outlier.stream();

		while let Some((_event_id, pdu)) = outliers.next().await {
			if let Some(room_id) = pdu.room_id() {
				room_ids.insert(room_id.to_owned());
			} else {
				// V3+ rooms: PDU JSON doesn't contain room_id.
				// We need a way to find the room association.
				// For --all, we might have to scan roomid_outliereventid.
				// But we can also just try to find it from the event_id if it's
				// a create event, or just ignore for now as it's expensive.
				if let Some(room_id) = pdu.room_id_or_hash() {
					room_ids.insert(room_id);
				}
			}
		}
		drop(outliers);

		if room_ids.is_empty() {
			return self.write_str("No outliers found in any room.").await;
		}

		self.write_str(&format!(
			"Found outliers in {} rooms. Starting rescue...",
			room_ids.len()
		))
		.await?;

		let mut total_rescued = 0_usize;
		for room_id in room_ids {
			if Box::pin(self.rescue_room(room_id, force, nuclear, false, None, false, vec![]))
				.await
				.is_ok()
			{
				total_rescued = total_rescued.saturating_add(1);
			}
		}

		return self
			.write_str(&format!("Finished rescue attempt for {total_rescued} rooms."))
			.await;
	}

	let mut outliers: HashMap<OwnedEventId, PduEvent> = self
		.services
		.rooms
		.outlier
		.room_stream(&room_id)
		.map(|(event_id, pdu)| (event_id, pdu))
		.collect()
		.await;

	if let Some(limit) = timeline_limit {
		self.write_str(&format!("Including last {limit} timeline PDUs for re-processing..."))
			.await?;
		let timeline_pdus: Vec<(OwnedEventId, PduEvent)> = self
			.services
			.rooms
			.timeline
			.pdus_rev(&room_id, None)
			.filter_map(|item| ready(item.ok()))
			.take(limit)
			.map(|(_, pdu)| (pdu.event_id().to_owned(), pdu))
			.collect()
			.await;

		for (event_id, pdu) in timeline_pdus {
			if outliers.contains_key(&event_id) {
				continue;
			}
			outliers.insert(event_id, pdu);
		}
	}

	if outliers.is_empty() {
		return self.write_str("No outliers found in this room.").await;
	}

	// Build the graph for topological sort.
	// Only include prev_events that exist in our outlier set to avoid events
	// being dropped from the sort output due to unresolvable parents.
	let mut graph: HashMap<OwnedEventId, HashSet<OwnedEventId>> =
		HashMap::with_capacity(outliers.len());
	for (event_id, pdu) in &outliers {
		let mut parents = HashSet::new();
		for prev_id in pdu.prev_events() {
			if outliers.contains_key(prev_id) {
				parents.insert(prev_id.to_owned());
			}
		}
		graph.insert(event_id.clone(), parents);
	}

	let event_fetch = |event_id: OwnedEventId| {
		let pdu = if let Some(p) = outliers.get(&event_id) {
			Some(p.clone())
		} else {
			self.services
				.rooms
				.timeline
				.get_pdu(&event_id)
				.now_or_never()
				.and_then(Result::ok)
		};

		let ts = pdu.map_or_else(|| ruma::uint!(0), |p| p.origin_server_ts);
		ready(Ok::<_, state_res::Error>((ruma::int!(0), ruma::MilliSecondsSinceUnixEpoch(ts))))
	};

	let sorted = state_res::lexicographical_topological_sort(&graph, &event_fetch)
		.await
		.map_err(|e| err!(Database("Failed to sort outliers: {e:?}")))?;

	// Find the create event first to use as the foundation
	let mut create_event = self
		.services
		.rooms
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await
		.ok();

	// If it's still missing, see if it's in our outlier list
	if create_event.is_none() {
		create_event = outliers
			.values()
			.find(|pdu| pdu.kind == TimelineEventType::RoomCreate)
			.cloned();
	}

	let create_event =
		create_event.ok_or_else(|| err!("Failed to find create event for room."))?;

	// Build a map of current timeline state events for supersession checks.
	// For each (event_type, state_key) we track (origin_server_ts, depth, event_id)
	// to determine which event is "newer" using the same 3 tiebreakers as
	// state resolution: origin_server_ts, then depth, then event_id.
	let mut current_state: HashMap<(String, String), (ruma::UInt, ruma::UInt, OwnedEventId)> =
		HashMap::new();
	if let Ok(state_hash) = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await
	{
		// Collect into Vec FIRST to drop the zero-copy RocksDB iterator
		// before the write/fetch phase. Holding an iterator across .await points
		// risks SEGV if compaction invalidates the underlying memory.
		let state_eids: Vec<_> = self
			.services
			.rooms
			.state_accessor
			.state_full(state_hash)
			.map(|((event_type, state_key), event)| {
				(event_type.to_string(), state_key.to_string(), event.event_id().to_owned())
			})
			.collect()
			.await;

		for (event_type, state_key, eid) in state_eids {
			// Fetch the full PduEvent for depth access
			if let Ok(full_pdu) = self.services.rooms.timeline.get_pdu(&eid).await {
				current_state.insert(
					(event_type, state_key),
					(full_pdu.origin_server_ts, full_pdu.depth, eid),
				);
			}
		}
	}

	let mut count = 0_usize;
	let mut skipped = 0_usize;
	let mut failed = 0_usize;

	// Build dependency graph for Concurrent DAG Execution
	let mut indegree: HashMap<OwnedEventId, usize> = HashMap::with_capacity(sorted.len());
	let mut dependents: HashMap<OwnedEventId, Vec<OwnedEventId>> =
		HashMap::with_capacity(sorted.len());
	let sorted_set: HashSet<OwnedEventId> = sorted.iter().cloned().collect();

	for id in &sorted {
		let pdu = outliers.get(id).expect("in sorted list");
		let mut deps = HashSet::new();
		for dep in pdu.prev_events().chain(pdu.auth_events()) {
			if sorted_set.contains(dep) {
				deps.insert(dep.to_owned());
			}
		}
		indegree.insert(id.clone(), deps.len());
		for dep in deps {
			dependents.entry(dep).or_default().push(id.clone());
		}
	}

	let mut ready_queue: VecDeque<OwnedEventId> = indegree
		.iter()
		.filter(|(_, count)| **count == 0)
		.map(|(id, _)| id.clone())
		.collect();

	let mut pending_futures = futures::stream::FuturesUnordered::new();
	// Limit concurrency to avoid memory exhaustion during heavy state-res
	let max_concurrency = self.services.server.concurrency_scaled(4);

	loop {
		while pending_futures.len() < max_concurrency && !ready_queue.is_empty() {
			let event_id = ready_queue.pop_front().unwrap();
			let pdu = outliers.get(&event_id).expect("in sorted list").clone();
			let origin = pdu
				.origin
				.clone()
				.unwrap_or_else(|| pdu.sender.server_name().to_owned());

			// Fast path check
			let mut is_skipped = false;
			let mut is_forced = false;

			if !force {
				if let Some(state_key) = &pdu.state_key {
					let key = (pdu.kind.to_string(), state_key.to_string());
					if let Some((curr_ts, curr_depth, curr_eid)) = current_state.get(&key) {
						let dominated = (pdu.origin_server_ts, pdu.depth, &pdu.event_id)
							< (*curr_ts, *curr_depth, curr_eid);
						if dominated {
							is_skipped = true;
						}
					}
				}
			} else {
				is_forced = true;
			}

			let event_handler = self.services.rooms.event_handler.clone();
			let pdu_metadata = self.services.rooms.pdu_metadata.clone();
			let timeline = self.services.rooms.timeline.clone();
			let room_id_c = room_id.clone();
			let create_event_c = create_event.clone();

			pending_futures.push(async move {
				if is_skipped {
					return (event_id, Ok(None));
				}

				if is_forced {
					pdu_metadata.clear_pdu_markers(&event_id);
					if timeline
						.promote_outlier(&room_id_c, &event_id)
						.await
						.is_ok()
					{
						return (event_id, Ok(Some(())));
					}
					return (event_id, Ok(None));
				}

				let pdu_json = match timeline.get_pdu_json(&event_id).await {
					| Ok(j) => j,
					| Err(e) => return (event_id, Err(e)),
				};

				pdu_metadata.clear_pdu_markers(&event_id);

				let res = Box::pin(event_handler.upgrade_outlier_to_timeline_pdu(
					pdu,
					pdu_json,
					&create_event_c,
					&origin,
					&room_id_c,
					true,
					// is_forward_extremity
					true,
				))
				.await;

				(event_id, res.map(|o| o.map(|_| ())))
			});
		}

		if let Some((event_id, res)) = pending_futures.next().await {
			match res {
				| Ok(Some(())) => count = count.saturating_add(1),
				| Ok(None) => skipped = skipped.saturating_add(1),
				| Err(e) => {
					failed = failed.saturating_add(1);
					warn!(%event_id, "rescue-room: failed to upgrade outlier: {e}");
				},
			}

			if let Some(deps) = dependents.get(&event_id) {
				for dep in deps {
					let count = indegree.get_mut(dep).unwrap();
					*count = count.saturating_sub(1);
					if *count == 0 {
						ready_queue.push_back(dep.clone());
					}
				}
			}

			if count.is_multiple_of(50) && count > 0 {
				tokio::task::yield_now().await;
			}
		} else if ready_queue.is_empty() && pending_futures.is_empty() {
			break;
		} else {
			warn!("Cycle detected or missing dependencies in rescue_room DAG execution!");
			break;
		}
	}

	let msg = match (skipped > 0, failed > 0) {
		| (true, true) => format!(
			"Rescued {count} PDUs in room {room_id} (skipped {skipped} superseded, {failed} \
			 failed)."
		),
		| (true, false) => {
			format!("Rescued {count} PDUs in room {room_id} (skipped {skipped} superseded).")
		},
		| (false, true) => format!(
			"Rescued {count} PDUs in room {room_id} ({failed} failed — check server logs for \
			 details)."
		),
		| (false, false) => format!("Rescued {count} PDUs in room {room_id}."),
	};
	self.write_str(&msg).await?;

	if reorder {
		self.write_str(&format!("\nRunning reorder-timeline for {room_id}..."))
			.await?;
		let n = Box::pin(
			self.services
				.rooms
				.timeline
				.reorder_timeline(&room_id, None, false),
		)
		.await?;
		self.write_str(&format!("Reordered {n} events. Clients should re-sync."))
			.await?;
	}

	if !heal_from.is_empty() {
		// Find the latest local event to use as at_event for bootstrapping
		let at_event = self
			.services
			.rooms
			.timeline
			.latest_pdu_in_room(&room_id)
			.await
			.ok()
			.map(|pdu| pdu.event_id().to_owned());

		self.write_str(&format!(
			"\nHealing state from {:?} (force-set-state --overwrite)...",
			heal_from.iter().map(|s| s.as_str()).collect::<Vec<_>>()
		))
		.await?;

		Box::pin(self.force_set_state(
			room_id.clone(),
			heal_from,
			at_event,
			true,  // overwrite
			true,  // skip_sig_verify
			true,  // absolute
			None,  // output
			None,  // input
			false, // dry_run
			false, // skip_membership_rebuild
		))
		.await?;

		self.write_str("Heal complete. Room state synchronized with backbone.")
			.await?;
	}

	Ok(())
}

#[admin_command]
pub(super) async fn rescue_pdu(&self, event_id: OwnedEventId, force: bool) -> Result {
	self.bail_restricted()?;

	let pdu_json = self
		.services
		.rooms
		.timeline
		.get_pdu_json(&event_id)
		.await
		.map_err(|_| err!("PDU not found in database."))?;

	let pdu: PduEvent = serde_json::from_value(serde_json::to_value(&pdu_json)?)?;
	let room_id = pdu
		.room_id()
		.ok_or_else(|| err!("PDU has no room_id."))?
		.to_owned();

	// Clear all soft-fail and rejection markers when rescuing unconditionally
	// (if an admin is rescuing a PDU, they definitely want it un-rejected)
	self.services
		.rooms
		.pdu_metadata
		.clear_pdu_markers(&event_id);

	// --force fast path: bypass state resolution and auth entirely.
	// Required for ancient events where remote servers have pruned historical
	// state (404 Pdu state not found) or the origin is gone.
	if force {
		self.services
			.rooms
			.timeline
			.promote_outlier(&room_id, &event_id)
			.await?;
		return self
			.write_str(&format!(
				"Force-promoted {event_id} into the timeline (bypassed state resolution)."
			))
			.await;
	}

	let create_event = self
		.services
		.rooms
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await
		.map_err(|_| err!("Failed to find create event for room."))?;

	let origin = pdu
		.origin
		.clone()
		.unwrap_or_else(|| pdu.sender.server_name().to_owned());

	// Lenient path: falls back to current room state when no server can
	// provide /state_ids for this historical event.
	Box::pin(
		self.services
			.rooms
			.event_handler
			.upgrade_outlier_to_timeline_pdu(
				pdu,
				pdu_json,
				&create_event,
				&origin,
				&room_id,
				true, // skip_soft_fail: always lenient for admin rescue
				true, // is_forward_extremity
			),
	)
	.await?;

	self.write_str("Successfully rescued PDU.").await
}

#[admin_command]
pub(super) async fn clean_corrupt_rooms(&self, execute: bool) -> Result {
	use futures::StreamExt;
	use ruma::RoomId;

	let ours = self.services.globals.server_name();
	let mut corrupt = Vec::new();
	let mut total = 0_usize;

	let mut raw_stream = self.services.rooms.metadata.iter_ids();

	while let Some(room_id) = raw_stream.next().await {
		total = total.saturating_add(1);
		let s = room_id.as_str();

		let valid = s.starts_with('!') && s.len() <= 255 && <&RoomId>::try_from(s).is_ok();
		if !valid && s.starts_with('!') {
			corrupt.push(room_id.to_owned());
		}
	}

	self.write_str(&format!("Scanned {total} rooms, found {} corrupt entries\n", corrupt.len()))
		.await?;

	for room_id in &corrupt {
		self.write_str(&format!("  corrupt: {} ({} bytes)\n", room_id, room_id.as_str().len()))
			.await?;
		if execute {
			let prefix = (ours, conduwuit_database::Interfix, &**room_id);
			let _ =
				self.services.rooms.state_cache.server_rooms_remove_raw(
					&conduwuit_database::serialize_to_vec(prefix).unwrap(),
				);
			self.services.rooms.metadata.remove_room_raw(room_id);
		}
	}

	if !execute {
		self.write_str(
			"\nDry run — corrupt entries are found using raw bytes. Use --execute to remove \
			 individual entries.\n",
		)
		.await
	} else {
		self.write_str("\nNote: Removed malformed room IDs from the serverroomids tree.\n")
			.await
	}
}

#[admin_command]
pub(super) async fn check_rooms(&self, problems_only: bool, fix: bool) -> Result {
	let ours = self.services.globals.server_name();

	let room_ids: Vec<_> = self
		.services
		.rooms
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let n_rooms = room_ids.len();
	self.write_str(&format!("Scanning {n_rooms} rooms...\n"))
		.await?;

	let mut total_rooms = 0_usize;
	let mut problem_rooms = 0_usize;
	let mut fixed_rooms = 0_usize;
	let mut output = String::new();

	for room_id in &room_ids {
		total_rooms = total_rooms.saturating_add(1);
		info!(room_id = %room_id, scanned = total_rooms, total = n_rooms, "check-rooms scanning room");
		let mut issues: Vec<String> = Vec::new();
		let room_str = room_id.as_str();

		// Corrupt room ID check
		if <&RoomId>::try_from(room_str).is_err() || !room_str.is_ascii() {
			issues.push(format!("CORRUPT_ID ({} bytes, non-parseable)", room_str.len()));
			// Can't do further checks on a corrupt ID
			problem_rooms = problem_rooms.saturating_add(1);
			writeln!(output, "FAIL {} -- {}", room_str, issues.join(", ")).ok();
			continue;
		}

		// Create event check
		let create_state = self
			.services
			.rooms
			.state_accessor
			.room_state_get(room_id, &StateEventType::RoomCreate, "")
			.await;

		match &create_state {
			| Ok(create_pdu) => {
				let create_id = create_pdu.event_id();
				let soft_failed = self
					.services
					.rooms
					.pdu_metadata
					.is_event_soft_failed(create_id)
					.await;
				if soft_failed {
					issues.push("SOFT_FAILED_CREATE".to_owned());
				}
			},
			| Err(_) => {
				issues.push("MISSING_CREATE".to_owned());
			},
		}

		// Local user check
		let has_local = self
			.services
			.rooms
			.state_cache
			.active_local_users_in_room(room_id)
			.boxed()
			.next()
			.await
			.is_some();

		if !has_local {
			let we_participate = self
				.services
				.rooms
				.state_cache
				.server_in_room(ours, room_id)
				.await;

			if we_participate {
				issues.push("ORPHANED (server listed, 0 local users)".to_owned());
			}
		}

		// Forward extremities check
		let would_change = self
			.services
			.rooms
			.timeline
			.recalculate_extremities(room_id, 100, fix)
			.await
			.unwrap_or(false);

		if would_change {
			if fix {
				issues.push("EXTREMITIES_DRIFT (Fixed)".to_owned());
			} else {
				issues.push("EXTREMITIES_DRIFT (DAG tips silently broken)".to_owned());
			}
		}

		let ext_count = self
			.services
			.rooms
			.state
			.get_forward_extremities(room_id)
			.count()
			.await;

		if ext_count == 0 {
			issues.push("ZERO_EXTREMITIES (stuck DAG)".to_owned());
		} else if ext_count > 10 {
			issues.push(format!("EXCESSIVE_EXTREMITIES ({ext_count} tips)"));
		}

		// Membership cache drift check
		let cache_joined = self
			.services
			.rooms
			.state_cache
			.room_joined_count(room_id)
			.await
			.unwrap_or(0);

		// Get actual state member count (joined)
		let state_joined: u64 = self
			.services
			.rooms
			.state_cache
			.room_members(room_id)
			.count()
			.await
			.try_into()
			.unwrap_or(0);

		if cache_joined != state_joined {
			issues.push(format!("MEMBERSHIP_DRIFT (cache={cache_joined}, state={state_joined})"));

			if fix {
				self.services
					.rooms
					.state_cache
					.update_joined_count(room_id)
					.await;
				issues.push("FIXED".to_owned());
				fixed_rooms = fixed_rooms.saturating_add(1);
			}
		}

		if issues.is_empty() {
			if !problems_only {
				writeln!(output, "OK   {room_id} (ext={ext_count}, joined={cache_joined})").ok();
			}
		} else {
			problem_rooms = problem_rooms.saturating_add(1);
			writeln!(output, "FAIL {room_id} -- {}", issues.join(", ")).ok();
		}

		// Flush every 25 rooms to show live progress
		if total_rooms.is_multiple_of(25) {
			if !output.is_empty() {
				self.write_str(&output).await?;
				output.clear();
			}
			info!(
				scanned = total_rooms,
				total = n_rooms,
				problems = problem_rooms,
				"check-rooms progress"
			);
		}
	}

	if !output.is_empty() {
		self.write_str(&output).await?;
	}

	let mut summary =
		format!("\n**Scan complete:** {total_rooms} rooms checked, {problem_rooms} with issues.");
	if fix && fixed_rooms > 0 {
		write!(summary, " {fixed_rooms} membership caches repaired.").ok();
	}
	summary.push('\n');

	self.write_str(&summary).await
}
