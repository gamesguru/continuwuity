use std::{
	collections::{HashMap, HashSet, VecDeque},
	fmt::Write,
};

use conduwuit::{
	Err, PduCount, Result, err, info,
	matrix::{Event, pdu::PduEvent},
	state_res,
	utils::stream::BroadbandExt,
	warn,
};
use futures::{FutureExt, StreamExt, future::ready, pin_mut};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName,
	OwnedUserId, RoomId, RoomVersionId,
	api::federation::event::{get_event, get_missing_events, get_room_state},
	events::{StateEventType, TimelineEventType},
};
use serde_json::Value as JsonValue;
use tokio::io::AsyncWriteExt;

use crate::admin_command;

#[admin_command]
pub(super) async fn audit_auth_chain(
	&self,
	room_id: OwnedRoomId,
	fetch: bool,
	verbose: bool,
) -> Result {
	use std::{borrow::Borrow, time::Instant};

	use futures::TryStreamExt;

	// Resolve room and get current state hash, fallback to extremities if no state
	let state_ids: Vec<OwnedEventId> = match self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await
	{
		| Ok(sstatehash) =>
			self.services
				.rooms
				.state_accessor
				.state_full_ids(sstatehash)
				.map(|(_, id)| id)
				.collect()
				.await,
		| Err(_) =>
			self.services
				.rooms
				.state
				.get_forward_extremities(&room_id)
				.map(ToOwned::to_owned)
				.collect()
				.await,
	};

	if state_ids.is_empty() {
		return Err!(
			"Room {room_id} has no state and no forward extremities — completely empty?"
		);
	}

	self.write_str(&format!(
		"Auditing auth chain for {room_id} ({} seed events)...\n",
		state_ids.len()
	))
	.await?;

	// Walk the full auth chain for all current state
	let started = Instant::now();
	let auth_chain: HashSet<OwnedEventId> = self
		.services
		.rooms
		.auth_chain
		.event_ids_iter(&room_id, state_ids.iter().map(Borrow::borrow))
		.try_collect()
		.await
		.unwrap_or_default();

	self.write_str(&format!(
		"Auth chain: {} events (walked in {:.1?})\n",
		auth_chain.len(),
		started.elapsed()
	))
	.await?;

	// Bucket: timeline / outlier-only / missing
	let mut in_timeline: usize = 0;
	let mut in_outlier: usize = 0;
	let mut missing: Vec<OwnedEventId> = Vec::new();

	for event_id in &auth_chain {
		if self.services.rooms.timeline.pdu_exists(event_id).await {
			in_timeline = in_timeline.saturating_add(1);
		} else if self
			.services
			.rooms
			.outlier
			.get_pdu_outlier(event_id)
			.await
			.is_ok()
		{
			in_outlier = in_outlier.saturating_add(1);
			if verbose {
				self.write_str(&format!("  OUTLIER: {event_id}\n")).await?;
			}
		} else {
			if verbose {
				self.write_str(&format!("  MISSING: {event_id}\n")).await?;
			}
			missing.push(event_id.clone());
		}
	}

	self.write_str(&format!(
		"Results: {in_timeline} timeline, {in_outlier} outlier-only, {} missing\n",
		missing.len()
	))
	.await?;

	if missing.is_empty() || !fetch {
		if !missing.is_empty() {
			self.write_str("Hint: rerun with --fetch to attempt recovery from room servers.\n")
				.await?;
		}
		return Ok(());
	}

	// --fetch: reuse the battle-tested outlier fetch pipeline (32-server EMA
	// fallback, backoff, full signature validation, rate-limit tracking)

	self.write_str(&format!(
		"Fetching {} missing events via fetch_and_handle_outliers pipeline...\n",
		missing.len(),
	))
	.await?;

	let fetched = self
		.services
		.rooms
		.event_handler
		.fetch_and_handle_outliers(
			self.services.globals.server_name(),
			missing.iter().map(AsRef::as_ref),
			None::<&PduEvent>,
			&room_id,
		)
		.await;

	self.write_str(&format!(
		"Fetched {}/{} missing auth events (now in outlier store).\n",
		fetched.len(),
		missing.len()
	))
	.await
}

#[admin_command]
pub(super) async fn audit_membership(
	&self,
	room_id: OwnedRoomId,
	server: Option<OwnedServerName>,
	at_event: Option<OwnedEventId>,
	clean: bool,
) -> Result {
	// ── Phase 1: Timeline vs State Snapshot ──────────────────────────────
	self.write_str("**Phase 1: Timeline vs State Snapshot**\n")
		.await?;

	let mut timeline_membership: HashMap<OwnedUserId, (String, String)> = HashMap::new();

	let pdus = self
		.services
		.rooms
		.timeline
		.pdus(&room_id, Some(PduCount::min()));

	pin_mut!(pdus);
	let mut timeline_count = 0_usize;

	while let Some(Ok((_count, pdu))) = pdus.next().await {
		if pdu.kind != TimelineEventType::RoomMember {
			continue;
		}

		let Some(state_key) = pdu.state_key() else {
			continue;
		};

		let content: JsonValue = pdu.get_content_as_value();
		let membership = content
			.get("membership")
			.and_then(|v| v.as_str())
			.unwrap_or("unknown")
			.to_owned();

		let event_id = pdu.event_id().to_string();

		if let Ok(user_id) = OwnedUserId::try_from(state_key) {
			timeline_membership.insert(user_id, (membership, event_id));
		}

		timeline_count = timeline_count.saturating_add(1);
	}

	let state_hash = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	let state = self.services.rooms.state_accessor.state_full(state_hash);

	pin_mut!(state);
	let mut state_membership: HashMap<OwnedUserId, (String, String)> = HashMap::new();

	while let Some(((event_type, state_key), pdu)) = state.next().await {
		if event_type != StateEventType::RoomMember {
			continue;
		}

		let content: JsonValue = pdu.get_content_as_value();
		let membership = content
			.get("membership")
			.and_then(|v| v.as_str())
			.unwrap_or("unknown")
			.to_owned();

		let event_id = pdu.event_id().to_string();

		if let Ok(user_id) = OwnedUserId::try_from(state_key.as_str()) {
			state_membership.insert(user_id, (membership, event_id));
		}
	}

	let mut divergences = Vec::new();
	let mut total_purged = 0_usize;
	let max_clean_passes = 50_usize;
	let mut pass_num = 0_usize;

	loop {
		pass_num = pass_num.saturating_add(1);
		let mut pass_purged = 0_usize;

		// Rebuild timeline membership for this pass
		let mut tl_membership_pass: HashMap<OwnedUserId, (String, String)> = HashMap::new();
		let pdus_pass = self
			.services
			.rooms
			.timeline
			.pdus(&room_id, Some(PduCount::min()));

		pin_mut!(pdus_pass);
		while let Some(Ok((_count, pdu))) = pdus_pass.next().await {
			if pdu.kind != TimelineEventType::RoomMember {
				continue;
			}
			let Some(state_key) = pdu.state_key() else {
				continue;
			};
			let membership = pdu
				.get_content_as_value()
				.get("membership")
				.and_then(|v| v.as_str())
				.unwrap_or("leave")
				.to_owned();
			let event_id = pdu.event_id().to_string();
			if let Ok(user_id) = OwnedUserId::try_from(state_key) {
				tl_membership_pass.insert(user_id, (membership, event_id));
			}
		}

		for (user_id, (tl_membership, tl_event)) in &tl_membership_pass {
			let is_divergent = match state_membership.get(user_id) {
				// Membership type differs (join vs leave, etc) — always divergent
				| Some((st_membership, _)) if st_membership != tl_membership => true,
				// Same membership but different event ID — only divergent for leave/ban
				// (multiple join events with different IDs are just renames)
				| Some((st_membership, st_event))
					if st_event != tl_event
						&& (st_membership == "leave" || st_membership == "ban") =>
					true,
				// User in timeline but absent from state
				| None if tl_membership == "join" || tl_membership == "invite" => true,
				| _ => false,
			};

			if is_divergent && clean {
				if let Ok(event_id) = OwnedEventId::try_from(tl_event.as_str()) {
					if let Ok(pdu_json) =
						self.services.rooms.timeline.get_pdu_json(&event_id).await
					{
						self.services.rooms.outlier.add_pdu_outlier(
							&event_id,
							&pdu_json,
							Some(&room_id),
						);
					}
					self.services
						.rooms
						.timeline
						.remove_from_timeline(&event_id)
						.await;
					self.services
						.rooms
						.pdu_metadata
						.mark_event_soft_failed(&event_id);

					pass_purged = pass_purged.saturating_add(1);
					total_purged = total_purged.saturating_add(1);

					if total_purged <= 100 {
						divergences.push(format!(
							"PURGED {user_id}: demoted `{tl_membership}` (via {tl_event}) to \
							 outlier",
						));
					}
					continue;
				}
			}

			if !clean {
				// Original diagnostic output (only on non-clean runs)
				match state_membership.get(user_id) {
					| Some((st_membership, st_event)) if st_membership != tl_membership => {
						divergences.push(format!(
							"WARN {user_id}: timeline says `{tl_membership}` (via {tl_event}) \
							 but state says `{st_membership}` (via {st_event})"
						));
					},
					| Some((_, st_event)) if st_event != tl_event => {
						divergences.push(format!(
							"DIFF {user_id}: `{tl_membership}` {tl_event} {st_event}"
						));
					},
					| None if tl_membership == "join" || tl_membership == "invite" => {
						divergences.push(format!(
							"MISSING {user_id}: timeline says `{tl_membership}` (via \
							 {tl_event}) but user is ABSENT from state snapshot"
						));
					},
					| _ => {},
				}
			}
		}

		if !clean || pass_purged == 0 || pass_num >= max_clean_passes {
			// Update timeline_membership from the final pass for ghost counting
			timeline_membership = tl_membership_pass;
			if clean && pass_num >= max_clean_passes && pass_purged > 0 {
				divergences.push(format!(
					"WARN: hit max {max_clean_passes} clean passes, {total_purged} total purged \
					 — remaining divergences may need manual inspection"
				));
			}
			break;
		}
	}

	if clean && total_purged > 100 {
		divergences.push(format!(
			"... and {} more purged (truncated)",
			total_purged.saturating_sub(100)
		));
	}

	// Count ghosts (federation imports with no local timeline events) by
	// membership — these are expected, not actionable anomalies.
	let mut ghost_count = 0_usize;
	let mut ghost_joined = 0_usize;
	let mut ghost_left = 0_usize;
	let mut ghost_banned = 0_usize;
	for (user_id, (st_membership, _)) in &state_membership {
		if !timeline_membership.contains_key(user_id) {
			ghost_count = ghost_count.saturating_add(1);
			match st_membership.as_str() {
				| "join" => ghost_joined = ghost_joined.saturating_add(1),
				| "leave" => ghost_left = ghost_left.saturating_add(1),
				| "ban" => ghost_banned = ghost_banned.saturating_add(1),
				| _ => {},
			}
		}
	}

	let mut out = format!(
		"Phase 1 for {room_id}:\n- Timeline membership events: {timeline_count}\n- Unique users \
		 in timeline: {}\n- Users in state snapshot: {}\n- Ghosts (federation imports, no \
		 timeline): {ghost_count} (joined={ghost_joined}, left={ghost_left}, \
		 banned={ghost_banned})\n",
		timeline_membership.len(),
		state_membership.len(),
	);

	if divergences.is_empty() {
		writeln!(out, "\nNo actionable divergences.").expect("fmt");
	} else {
		writeln!(out, "\n**{} actionable divergence(s):**", divergences.len()).expect("fmt");
		for d in &divergences {
			writeln!(out, "- {d}").expect("fmt");
		}
	}

	self.write_str(&out).await?;

	// ── Phase 2: State Snapshot vs Cache ─────────────────────────────────
	self.write_str("\n**Phase 2: State Snapshot vs Cache**\n")
		.await?;

	let mut state_joined: HashSet<OwnedUserId> = HashSet::new();
	let mut state_invited: HashSet<OwnedUserId> = HashSet::new();
	let mut state_left = 0_usize;
	let mut state_banned = 0_usize;
	let mut state_knocked = 0_usize;

	for (user_id, (membership, _)) in &state_membership {
		match membership.as_str() {
			| "join" => {
				state_joined.insert(user_id.clone());
			},
			| "invite" => {
				state_invited.insert(user_id.clone());
			},
			| "leave" => state_left = state_left.saturating_add(1),
			| "ban" => state_banned = state_banned.saturating_add(1),
			| "knock" => state_knocked = state_knocked.saturating_add(1),
			| _ => {},
		}
	}

	let cached_joined = self
		.services
		.rooms
		.state_cache
		.room_joined_count(&room_id)
		.await
		.unwrap_or(0);

	let cached_invited = self
		.services
		.rooms
		.state_cache
		.room_invited_count(&room_id)
		.await
		.unwrap_or(0);

	// Collect actual cache members for bidirectional comparison
	let cached_joined_members: HashSet<OwnedUserId> = self
		.services
		.rooms
		.state_cache
		.room_members(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let cached_invited_members: HashSet<OwnedUserId> = self
		.services
		.rooms
		.state_cache
		.room_members_invited(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut cache_mismatches = Vec::new();

	// Check state → cache (MISSING: in state but not cache)
	for user_id in &state_joined {
		if !cached_joined_members.contains(user_id) {
			cache_mismatches
				.push(format!("MISSING {user_id}: state says JOINED but cache says NOT joined"));
		}
	}

	for user_id in &state_invited {
		if !cached_invited_members.contains(user_id) {
			cache_mismatches.push(format!(
				"MISSING {user_id}: state says INVITED but cache says NOT invited"
			));
		}
	}

	// Check cache → state (EXTRA: in cache but not state)
	for user_id in &cached_joined_members {
		if !state_joined.contains(user_id) {
			cache_mismatches
				.push(format!("EXTRA {user_id}: cache says JOINED but state says NOT joined"));
		}
	}

	for user_id in &cached_invited_members {
		if !state_invited.contains(user_id) {
			cache_mismatches
				.push(format!("EXTRA {user_id}: cache says INVITED but state says NOT invited"));
		}
	}

	let total_members = state_joined
		.len()
		.saturating_add(state_invited.len())
		.saturating_add(state_left)
		.saturating_add(state_banned)
		.saturating_add(state_knocked);

	let counts_line = format!(
		"- Total members in state: {total_members}\n- Joined: state={}, \
		 cache={cached_joined}\n- Invited: state={}, cache={cached_invited}\n- Left: \
		 state={state_left}\n- Banned: state={state_banned}\n- Knocked: state={state_knocked}",
		state_joined.len(),
		state_invited.len()
	);

	if cache_mismatches.is_empty() {
		self.write_str(&format!(
			"OK: Membership cache is consistent for {room_id}\n{counts_line}"
		))
		.await?;
	} else {
		let mut out = format!(
			"Membership cache divergences for {room_id}:\n{counts_line}\n\n**{} \
			 mismatch(es):**\n",
			cache_mismatches.len()
		);

		for m in cache_mismatches.iter().take(100) {
			writeln!(out, "- {m}").expect("fmt");
		}
		if cache_mismatches.len() > 100 {
			writeln!(out, "- ... and {} more", cache_mismatches.len().saturating_sub(100))
				.expect("fmt");
		}

		self.write_str(&out).await?;
	}

	// ── Phase 2.5: Aggregate count cross-check + active healing ──────────
	let state_joined_count: u64 = state_joined
		.len()
		.try_into()
		.expect("joined count overflow");
	let cached_joined_u64 = self
		.services
		.rooms
		.state_cache
		.room_joined_count(&room_id)
		.await
		.unwrap_or(0);

	if cached_joined_u64 != state_joined_count || !cache_mismatches.is_empty() {
		self.write_str(&format!(
			"\n✗ CACHE INCONSISTENCY (state: {state_joined_count}, cache: {cached_joined_u64}, \
			 mismatches: {}). Healing…",
			cache_mismatches.len()
		))
		.await?;

		// Heal EXTRA users (in cache but not state)
		for user_id in &cached_joined_members {
			if !state_joined.contains(user_id) {
				self.services
					.rooms
					.state_cache
					.mark_as_left_silent(user_id, &room_id)
					.await;
			}
		}
		for user_id in &cached_invited_members {
			if !state_invited.contains(user_id) {
				self.services
					.rooms
					.state_cache
					.mark_as_left_silent(user_id, &room_id)
					.await;
			}
		}

		// Heal MISSING users (in state but not cache)
		for user_id in &state_joined {
			if !cached_joined_members.contains(user_id) {
				self.services
					.rooms
					.state_cache
					.mark_as_joined_silent(user_id, &room_id)
					.await;
			}
		}

		for user_id in &state_invited {
			if !cached_invited_members.contains(user_id) {
				// Heal invite by fetching the actual PDU from the authoritative state
				if let Ok(pdu) = self
					.services
					.rooms
					.state_accessor
					.state_get(state_hash, &StateEventType::RoomMember, user_id.as_str())
					.await
				{
					let _ = self
						.services
						.rooms
						.state_cache
						.update_membership(&room_id, user_id, &pdu, false)
						.await;
				}
			}
		}

		self.services
			.rooms
			.state_cache
			.update_joined_count(&room_id)
			.await;
		self.write_str("\n✓ Cache repaired.\n").await?;
	}

	// ── Phase 3: Remote comparison (optional) ────────────────────────────
	if let Some(ref server) = server {
		self.write_str(&format!("\n**Phase 3: Local vs Remote ({server})**\n"))
			.await?;

		let latest_event_id = match at_event {
			| Some(ref eid) => eid.clone(),
			| None => self
				.services
				.rooms
				.timeline
				.latest_pdu_in_room(&room_id)
				.await?
				.event_id()
				.to_owned(),
		};

		match self
			.services
			.sending
			.send_federation_request(server, get_room_state::v1::Request {
				room_id: room_id.clone(),
				event_id: latest_event_id.clone(),
			})
			.await
		{
			| Ok(response) => {
				let room_version = self
					.services
					.rooms
					.state
					.get_room_version_or_fallback(&room_id)
					.await?;

				let mut remote_members: HashMap<String, String> = HashMap::new();
				let mut sig_failed: usize = 0;
				let mut parse_failed: usize = 0;

				for pdu_raw in &response.pdus {
					let (event_id, value) = match self
						.services
						.server_keys
						.validate_and_add_event_id(pdu_raw, &room_version)
						.await
					{
						| Ok(r) => r,
						| Err(e) => {
							sig_failed = sig_failed.saturating_add(1);
							if let Ok((eid, val)) =
								conduwuit::matrix::event::gen_event_id_canonical_json(
									pdu_raw,
									&room_version,
								) {
								warn!(
									"audit_membership: PDU {eid} failed sig verify, storing as \
									 rejected outlier: {e}"
								);
								self.services.rooms.outlier.add_pdu_outlier(
									&eid,
									&val,
									Some(&room_id),
								);
								self.services
									.rooms
									.pdu_metadata
									.mark_event_soft_failed(&eid);
							}
							continue;
						},
					};

					let Ok(pdu) = PduEvent::from_id_val(&event_id, value, Some(room_id.as_ref()))
					else {
						parse_failed = parse_failed.saturating_add(1);
						continue;
					};

					if pdu.kind != TimelineEventType::RoomMember {
						continue;
					}

					if let Some(state_key) = pdu.state_key() {
						let content: JsonValue = pdu.get_content_as_value();
						let membership = content
							.get("membership")
							.and_then(|v| v.as_str())
							.unwrap_or("unknown")
							.to_owned();

						remote_members.insert(state_key.to_owned(), membership);
					}
				}

				// Inject the tip event into remote_members, if it is a member event!

				if let Ok(tip_pdu) = self.services.rooms.timeline.get_pdu(&latest_event_id).await
				{
					if tip_pdu.kind == TimelineEventType::RoomMember {
						if let Some(state_key) = tip_pdu.state_key() {
							let content: JsonValue = tip_pdu.get_content_as_value();

							let membership = content
								.get("membership")
								.and_then(|v| v.as_str())
								.unwrap_or("unknown")
								.to_owned();

							remote_members.insert(state_key.to_owned(), membership);
						}
					}
				}

				let mut local_members: HashMap<String, (String, String)> = HashMap::new();

				for (user_id, (membership, eid)) in &state_membership {
					local_members.insert(user_id.to_string(), (membership.clone(), eid.clone()));
				}

				let remote_joined = remote_members.values().filter(|m| *m == "join").count();
				let remote_invited = remote_members.values().filter(|m| *m == "invite").count();
				let remote_left = remote_members.values().filter(|m| *m == "leave").count();
				let remote_banned = remote_members.values().filter(|m| *m == "ban").count();
				self.write_str(&format!(
					"Remote {server}: {} total member events, joined={remote_joined}, \
					 invited={remote_invited}, left={remote_left}, banned={remote_banned}\n",
					remote_members.len()
				))
				.await?;

				let mut remote_diffs: Vec<(u64, String)> = Vec::new();

				let now_secs = std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.unwrap_or_default()
					.as_secs();

				let format_age = |age_secs: u64| -> String {
					let days = age_secs / 86400;
					let hours = age_secs / 3600;
					if age_secs > 86400 {
						format!("{days}d ago")
					} else if age_secs > 3600 {
						format!("{hours}h ago")
					} else {
						format!("{age_secs}s ago")
					}
				};

				for (user, remote_ms) in &remote_members {
					match local_members.get(user) {
						| Some((local_ms, eid)) if local_ms != remote_ms => {
							let age_secs = if let Ok(eid) = OwnedEventId::parse(eid) {
								self.services
									.rooms
									.timeline
									.get_pdu(&eid)
									.await
									.ok()
									.map_or(u64::MAX, |p| {
										let ms = u64::from(p.origin_server_ts);
										now_secs.saturating_sub(ms / 1000)
									})
							} else {
								u64::MAX
							};
							let age = if age_secs < u64::MAX {
								format_age(age_secs)
							} else {
								String::from("unknown")
							};
							remote_diffs.push((
								age_secs,
								format!(
									"WARN {user}: local=`{local_ms}`, {server}=`{remote_ms}` \
									 (event: {eid}, {age})"
								),
							));
						},
						| None if remote_ms == "join" || remote_ms == "invite" => {
							remote_diffs.push((
								u64::MAX,
								format!(
									"MISSING {user}: ABSENT locally but {server} says \
									 `{remote_ms}`"
								),
							));
						},
						| _ => {},
					}
				}

				for (user, (local_ms, eid)) in &local_members {
					if !remote_members.contains_key(user)
						&& (local_ms == "join" || local_ms == "invite")
					{
						let age_secs = if let Ok(eid) = OwnedEventId::parse(eid) {
							self.services
								.rooms
								.timeline
								.get_pdu(&eid)
								.await
								.ok()
								.map_or(u64::MAX, |p| {
									let ms = u64::from(p.origin_server_ts);
									now_secs.saturating_sub(ms / 1000)
								})
						} else {
							u64::MAX
						};
						let age = if age_secs < u64::MAX {
							format_age(age_secs)
						} else {
							String::from("unknown")
						};
						remote_diffs.push((
							age_secs,
							format!(
								"GHOST {user}: local says `{local_ms}` but ABSENT on {server} \
								 (event: {eid}, {age})"
							),
						));
					}
				}

				// Sort newest first (smallest age_secs first)
				remote_diffs.sort_by_key(|(age, _)| *age);

				let failure_summary = if sig_failed > 0 || parse_failed > 0 {
					format!(" (sig_failed={sig_failed}, parse_failed={parse_failed})")
				} else {
					String::new()
				};

				if remote_diffs.is_empty() {
					self.write_str(&format!(
						"OK: Local and {server} agree on membership ({} members, \
						 joined={remote_joined}, invited={remote_invited}, left={remote_left}, \
						 banned={remote_banned}){failure_summary}",
						remote_members.len()
					))
					.await?;
				} else {
					let mut out = format!(
						"Remote membership diffs vs {server} ({} diff(s)){failure_summary}:\n",
						remote_diffs.len()
					);
					for (_, d) in &remote_diffs {
						writeln!(out, "- {d}").expect("fmt");
					}
					self.write_str(&out).await?;
				}
			},
			| Err(e) => {
				self.write_str(&format!("Failed to fetch state from {server}: {e}"))
					.await?;
			},
		}
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
pub(super) async fn list_outliers(
	&self,
	room_id: Option<OwnedRoomOrAliasId>,
	sender: Option<OwnedUserId>,
	limit: Option<usize>,
	rejected: bool,
	clear: bool,
	reverse: bool,
) -> Result {
	let limit = limit.unwrap_or(100);

	let mut outliers: Vec<(OwnedEventId, PduEvent)> = if let Some(room) = room_id {
		let room_id = self.services.rooms.alias.resolve(&room).await?;
		self.services
			.rooms
			.outlier
			.room_stream(&room_id)
			.filter(|(_event_id, pdu): &(OwnedEventId, PduEvent)| {
				let sender_match = sender.as_ref().is_none_or(|s| pdu.sender() == s);
				ready(sender_match)
			})
			.collect()
			.await
	} else {
		// Global stream: cap at 10k to avoid OOM on large outlier sets (300k+).
		// Room-specific queries above are uncapped for correct TS sorting.
		self.services
			.rooms
			.outlier
			.stream()
			.filter(|(_event_id, pdu): &(OwnedEventId, PduEvent)| {
				let sender_match = sender.as_ref().is_none_or(|s| pdu.sender() == s);
				ready(sender_match)
			})
			.take(10_000)
			.collect()
			.await
	};

	// Sort by origin_server_ts (or in reverse, if requested)
	outliers.sort_by(|(_, a), (_, b)| {
		if reverse {
			b.origin_server_ts.cmp(&a.origin_server_ts)
		} else {
			a.origin_server_ts.cmp(&b.origin_server_ts)
		}
	});

	let mut count = 0_usize;
	let mut cleared = 0_usize;
	let mut body = String::new();
	for (event_id, pdu) in outliers {
		if count >= limit {
			writeln!(body, "--- Stopped after {limit} entries ---")?;
			break;
		}

		let is_stuck = self
			.services
			.rooms
			.timeline
			.get_pdu_id(&event_id)
			.await
			.is_ok();
		let is_rejected = self
			.services
			.rooms
			.pdu_metadata
			.is_event_rejected(&event_id)
			.await;
		let is_soft_failed = self
			.services
			.rooms
			.pdu_metadata
			.is_event_soft_failed(&event_id)
			.await;

		let status =
			super::outlier_utils::OutlierStatus { is_stuck, is_rejected, is_soft_failed };

		let action = super::outlier_utils::classify_outlier(&status, rejected, clear);
		match action {
			| super::outlier_utils::OutlierAction::Skip => continue,
			| super::outlier_utils::OutlierAction::Show { should_clear } =>
				if should_clear {
					self.services
						.rooms
						.pdu_metadata
						.clear_pdu_markers(&event_id);
					cleared = cleared.saturating_add(1);
				},
		}

		let room_id_str = pdu.room_id().map_or("unknown", RoomId::as_str);
		let sender = pdu.sender();
		let kind = pdu.kind.to_string();
		let ts = pdu.origin_server_ts;
		let flags = super::outlier_utils::render_flags(&status);

		writeln!(
			body,
			"{event_id}\tTS: {ts}\tRoom: {room_id_str}\tSender: {sender}\tType: {kind}{flags}"
		)?;
		count = count.saturating_add(1);
	}

	if body.is_empty() {
		if rejected {
			return Err!("No rejected outliers found.");
		}
		return Err!("No outliers found.");
	}

	let header = super::outlier_utils::summary_header(rejected);
	self.write_str(&format!("{header} ({count} shown, {cleared} cleared):\n```\n{body}\n```"))
		.await
}

#[admin_command]
pub(super) async fn view_extremities(
	&self,
	room: Option<OwnedRoomOrAliasId>,
	all: bool,
	verbose: bool,
) -> Result {
	if all || room.is_none() {
		let mut fractured = Vec::new();
		let rooms: Vec<_> = self
			.services
			.rooms
			.metadata
			.iter_ids()
			.map(ToOwned::to_owned)
			.collect()
			.await;

		for room_id in &rooms {
			let count = self
				.services
				.rooms
				.state
				.get_forward_extremities(room_id)
				.count()
				.await;
			if count > 1 {
				fractured.push((room_id.clone(), count));
			}
		}

		fractured.sort_by(|a, b| b.1.cmp(&a.1));

		if fractured.is_empty() {
			return self
				.write_str(&format!("All {} rooms have exactly 1 extremity. ✓", rooms.len()))
				.await;
		}

		let mut body = String::new();
		for (room_id, count) in &fractured {
			writeln!(body, "{room_id}\t{count} extremities")?;
			if verbose {
				let extremities: Vec<OwnedEventId> = self
					.services
					.rooms
					.state
					.get_forward_extremities(room_id)
					.map(ToOwned::to_owned)
					.collect()
					.await;
				for eid in &extremities {
					let detail = match self.services.rooms.timeline.get_pdu(eid).await {
						| Ok(pdu) => {
							let ts = pdu.origin_server_ts;
							let kind = pdu.kind.to_string();
							let sender = pdu.sender();
							format!("  {eid}  {kind}  {sender}  TS:{ts}")
						},
						| Err(_) => format!("  {eid}  (PDU not found in timeline)"),
					};
					writeln!(body, "{detail}")?;
				}
				writeln!(body)?;
			}
		}

		return self
			.write_str(&format!(
				"{} of {} rooms have multiple extremities:\n```\n{body}\n```",
				fractured.len(),
				rooms.len()
			))
			.await;
	}

	let room = room.expect("room required when not --all");
	let room_id = self.services.rooms.alias.resolve(&room).await?;
	let extremities: Vec<OwnedEventId> = self
		.services
		.rooms
		.state
		.get_forward_extremities(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let num = extremities.len();
	let mut body = String::new();
	for event_id in extremities {
		let pdu = self.services.rooms.timeline.get_pdu(&event_id).await;
		match pdu {
			| Ok(pdu) => {
				let ts = pdu.origin_server_ts;
				let sender = pdu.sender();
				writeln!(body, "{event_id}\tTS: {ts}\tSender: {sender}")?;
			},
			| Err(_) => {
				writeln!(body, "{event_id}\tERROR: PDU not found in timeline")?;
			},
		}
	}

	self.write_str(&format!("Room {room_id} has {num} extremities:\n```\n{body}\n```"))
		.await
}

#[admin_command]
pub(super) async fn purge_outliers(
	&self,
	event_id: Option<OwnedEventId>,
	room_id: Option<OwnedRoomOrAliasId>,
	sender: Option<OwnedUserId>,
	all: bool,
	force: bool,
) -> Result {
	// Fast path: single event by ID
	if let Some(ref eid) = event_id {
		self.services.rooms.outlier.remove_outlier(eid, None).await;
		return self.write_str(&format!("Purged outlier {eid}")).await;
	}

	if room_id.is_none() && sender.is_none() && !all {
		return Err!(
			"You must specify --event-id, a room, a sender, or use --all to purge outliers."
		);
	}

	let outliers: Vec<OwnedEventId> = if let Some(room) = room_id {
		let room_id = self.services.rooms.alias.resolve(&room).await?;
		self.services
			.rooms
			.outlier
			.room_stream(&room_id)
			.filter(|(_event_id, pdu): &(OwnedEventId, PduEvent)| {
				let sender_match = sender.as_ref().is_none_or(|s| pdu.sender() == s);
				ready(sender_match)
			})
			.map(|(event_id, _)| event_id)
			.collect()
			.await
	} else {
		self.services
			.rooms
			.outlier
			.stream()
			.filter(|(_event_id, pdu): &(OwnedEventId, PduEvent)| {
				let sender_match = sender.as_ref().is_none_or(|s| pdu.sender() == s);
				ready(sender_match)
			})
			.map(|(event_id, _)| event_id)
			.collect()
			.await
	};

	let mut purged = 0_usize;
	let mut skipped = 0_usize;
	for event_id in &outliers {
		if force {
			// Force-remove: skip the timeline lookup entirely
			self.services
				.rooms
				.outlier
				.remove_outlier(event_id, None)
				.await;
			purged = purged.saturating_add(1);
		} else if self
			.services
			.rooms
			.timeline
			.get_pdu_id(event_id)
			.await
			.is_ok()
		{
			// Duplicate: exists in both outlier and timeline tables
			self.services
				.rooms
				.outlier
				.remove_outlier(event_id, None)
				.await;
			purged = purged.saturating_add(1);
		} else {
			skipped = skipped.saturating_add(1);
		}

		let total = purged.saturating_add(skipped);
		if total.is_multiple_of(10_000) && total > 0 {
			info!(
				"Purge progress: {purged} purged, {skipped} skipped of {} total",
				outliers.len()
			);
		}
	}

	self.write_str(&format!("Purged {purged} outliers, skipped {skipped} un-rescued outliers."))
		.await
}

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
) -> Result {
	self.bail_restricted()?;

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
			if Box::pin(self.rescue_room(room_id, force, nuclear, false, None, false))
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

	let mut outliers: HashMap<OwnedEventId, (PduEvent, CanonicalJsonObject)> = self
		.services
		.rooms
		.outlier
		.room_stream(&room_id)
		.broad_filter_map(|(event_id, pdu): (OwnedEventId, PduEvent)| async move {
			let json = self
				.services
				.rooms
				.timeline
				.get_pdu_json(&event_id)
				.await
				.ok()?;
			Some((event_id, (pdu, json)))
		})
		.collect()
		.await;

	if let Some(limit) = timeline_limit {
		self.write_str(&format!("Including last {limit} timeline PDUs for re-processing..."))
			.await?;
		let timeline_pdus: Vec<(OwnedEventId, PduEvent)> = self
			.services
			.rooms
			.timeline
			.all_pdus(&room_id)
			.collect::<Vec<_>>()
			.await
			.into_iter()
			.rev()
			.take(limit)
			.map(|(_, pdu)| (pdu.event_id().to_owned(), pdu))
			.collect();

		for (event_id, pdu) in timeline_pdus {
			if outliers.contains_key(&event_id) {
				continue;
			}
			if let Ok(json) = self.services.rooms.timeline.get_pdu_json(&event_id).await {
				outliers.insert(event_id, (pdu, json));
			}
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
	for (event_id, (pdu, _)) in &outliers {
		let mut parents = HashSet::new();
		for prev_id in pdu.prev_events() {
			if outliers.contains_key(prev_id) {
				parents.insert(prev_id.to_owned());
			}
		}
		graph.insert(event_id.clone(), parents);
	}

	let event_fetch = |event_id: OwnedEventId| {
		let pdu = if let Some((p, _)) = outliers.get(&event_id) {
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
			.find(|(pdu, _)| pdu.kind == TimelineEventType::RoomCreate)
			.map(|(pdu, _)| pdu.clone());
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
	for event_id in sorted {
		let (pdu, pdu_json) = outliers.get(&event_id).expect("in sorted list");

		// Skip superseded state events (only when not force-rescuing).
		if !force {
			if let Some(state_key) = &pdu.state_key {
				let key = (pdu.kind.to_string(), state_key.to_string());
				if let Some((curr_ts, curr_depth, curr_eid)) = current_state.get(&key) {
					let dominated = (pdu.origin_server_ts, pdu.depth, &pdu.event_id)
						< (*curr_ts, *curr_depth, curr_eid);
					if dominated {
						skipped = skipped.saturating_add(1);
						continue;
					}
				}
			}
		}

		// --force fast path: bypass the network and auth checks entirely and
		// directly promote the outlier to the timeline. Required for dead events
		// where the network returns 404 for /state_ids (pruned remote databases)
		// or the origin server is gone entirely.
		if force {
			self.services
				.rooms
				.pdu_metadata
				.clear_pdu_markers(&event_id);
			if self
				.services
				.rooms
				.timeline
				.promote_outlier(&room_id, &event_id)
				.await
				.is_ok()
			{
				count = count.saturating_add(1);
			}
			if count.is_multiple_of(500) && count > 0 {
				info!("rescue_room (force): promoted {count} events...");
				tokio::task::yield_now().await;
			}
			continue;
		}

		let origin = pdu
			.origin
			.clone()
			.unwrap_or_else(|| pdu.sender.server_name().to_owned());

		// Always clear rejection/soft-fail markers before rescue attempt.
		// An admin explicitly rescuing events wants them in the timeline.
		// Without this, previously soft-failed events bounce off the early
		// check in upgrade_outlier_to_timeline_pdu and nothing happens.
		self.services
			.rooms
			.pdu_metadata
			.clear_pdu_markers(&event_id);

		match Box::pin(
			self.services
				.rooms
				.event_handler
				.upgrade_outlier_to_timeline_pdu(
					pdu.clone(),
					pdu_json.clone(),
					&create_event,
					&origin,
					&room_id,
					// Always skip soft-fail for admin rescue (matches rescue-pdu)
					true,
					// is_forward_extremity
					true,
				),
		)
		.await
		{
			| Ok(Some(_)) => {
				count = count.saturating_add(1);
				// Update current_state so subsequent events can compare against
				// the just-rescued event
				if let Some(state_key) = &pdu.state_key {
					let key = (pdu.kind.to_string(), state_key.to_string());
					current_state
						.insert(key, (pdu.origin_server_ts, pdu.depth, pdu.event_id.clone()));
				}
			},
			| Ok(None) => {
				// Event was acknowledged but NOT added to the timeline
				// (e.g., soft-failed acknowledgment). Don't count as rescued.
				skipped = skipped.saturating_add(1);
			},
			| Err(e) => {
				failed = failed.saturating_add(1);
				warn!(
					event_id = %event_id,
					sender = %pdu.sender(),
					kind = %pdu.kind,
					"rescue-room: failed to upgrade outlier: {e}"
				);
			},
		}

		// Yield every 10 events to prevent blocking the executor too long
		if count.is_multiple_of(10) {
			tokio::task::yield_now().await;
		}
	}

	let msg = match (skipped > 0, failed > 0) {
		| (true, true) => format!(
			"Rescued {count} PDUs in room {room_id} (skipped {skipped} superseded, {failed} \
			 failed)."
		),
		| (true, false) =>
			format!("Rescued {count} PDUs in room {room_id} (skipped {skipped} superseded)."),
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
				.reorder_timeline(&room_id, None),
		)
		.await?;
		self.write_str(&format!("Reordered {n} events. Clients should re-sync."))
			.await?;
	}

	Ok(())
}

#[admin_command]
pub(super) async fn reorder_timeline(
	&self,
	room_id: OwnedRoomId,
	all: bool,
	tail: Option<usize>,
) -> Result {
	self.bail_restricted()?;

	if all {
		let mut room_ids: Vec<OwnedRoomId> = Vec::new();
		let mut rooms = self.services.rooms.metadata.iter_ids();
		while let Some(room_id) = rooms.next().await {
			room_ids.push(room_id.to_owned());
		}
		drop(rooms);

		self.write_str(&format!("Reordering timeline for {} rooms...", room_ids.len()))
			.await?;

		let mut count = 0_usize;
		for room_id in room_ids {
			if Box::pin(
				self.services
					.rooms
					.timeline
					.reorder_timeline(&room_id, None),
			)
			.await
			.is_ok()
			{
				count = count.saturating_add(1);
			}
		}

		return self
			.write_str(&format!("Reordered timeline for {count} rooms. Clients should re-sync."))
			.await;
	}

	if let Some(n) = tail {
		self.write_str(&format!(
			"Reordering last {n} events in {room_id} by origin_server_ts (tail fast-path)..."
		))
		.await?;
		let count = Box::pin(
			self.services
				.rooms
				.timeline
				.reorder_timeline(&room_id, Some(n)),
		)
		.await?;
		return self
			.write_str(&format!(
				"Reordered {count} events in room {room_id}. Clients should re-sync this room."
			))
			.await;
	}

	self.write_str(&format!("Reordering timeline for {room_id} by origin_server_ts..."))
		.await?;

	let count = Box::pin(
		self.services
			.rooms
			.timeline
			.reorder_timeline(&room_id, None),
	)
	.await?;

	self.write_str(&format!(
		"Reordered {count} PDUs in room {room_id}. Clients should re-sync this room."
	))
	.await
}

#[admin_command]
pub(super) async fn promote_outliers(&self, room_id: OwnedRoomId) -> Result {
	self.bail_restricted()?;

	let outlier_ids: Vec<_> = self
		.services
		.rooms
		.outlier
		.room_stream(&room_id)
		.map(|(event_id, _pdu)| event_id)
		.collect()
		.await;

	let total = outlier_ids.len();
	self.write_str(&format!("Promoting {total} outliers to timeline for {room_id}..."))
		.await?;

	let mut promoted = 0_usize;
	let mut failed = 0_usize;
	for event_id in &outlier_ids {
		// Clear soft-fail/rejected markers so promoted events are visible
		// to clients via sync. Without this, events are ghost timeline entries.
		self.services.rooms.pdu_metadata.clear_pdu_markers(event_id);

		match self
			.services
			.rooms
			.timeline
			.promote_outlier(&room_id, event_id)
			.await
		{
			| Ok(()) => {
				promoted = promoted.saturating_add(1);
			},
			| Err(e) => {
				info!("Failed to promote outlier {event_id}: {e:?}");
				failed = failed.saturating_add(1);
			},
		}

		let done = promoted.saturating_add(failed);
		if done.is_multiple_of(10000) {
			info!(target: "promote_outliers", "Progress: {done}/{total} ({promoted} ok, {failed} err)");
		}
	}

	self.write_str(&format!(
		"Promoted {promoted} outliers, {failed} failed out of {total} total for {room_id}. \
		 Clients should re-sync."
	))
	.await
}

#[admin_command]
pub(super) async fn purge_timeline_pdu(&self, event_id: OwnedEventId) -> Result {
	self.bail_restricted()?;

	let in_timeline = self
		.services
		.rooms
		.timeline
		.non_outlier_pdu_exists(&event_id)
		.await;

	let mut room_id_opt = None;
	if in_timeline {
		if let Ok(pdu) = self.services.rooms.timeline.get_pdu(&event_id).await {
			room_id_opt = pdu.room_id().map(ToOwned::to_owned);
		}
	}

	// Remove from timeline tables (pduid_pdu + eventid_pduid)
	self.services
		.rooms
		.timeline
		.remove_from_timeline(&event_id)
		.await;

	// Also remove from outlier tables
	self.services
		.rooms
		.outlier
		.remove_outlier(&event_id, None)
		.await;

	if in_timeline {
		if let Some(room_id) = room_id_opt {
			self.services
				.rooms
				.timeline
				.recalculate_extremities(&room_id, 100, true)
				.await?;
		}
		self.write_str(&format!(
			"Purged {event_id} from timeline and outlier tables. DAG Extremities automatically \
			 recalculated."
		))
		.await
	} else {
		self.write_str(&format!(
			"Event {event_id} was not in the timeline (purged outlier only)."
		))
		.await
	}
}

#[admin_command]
pub(super) async fn get_room_dag(
	&self,
	room_id: OwnedRoomOrAliasId,
	start: i64,
	end: i64,
	print: bool,
) -> Result {
	let room_id = self.services.rooms.alias.resolve(&room_id).await?;
	let pdu_ids: Vec<OwnedEventId> = self
		.services
		.rooms
		.timeline
		.all_pdus(&room_id)
		.map(|(_, pdu)| pdu.event_id().to_owned())
		.collect()
		.await;

	let actual_start = if start < 0 {
		let offset = usize::try_from(start.unsigned_abs()).unwrap_or(usize::MAX);
		u64::try_from(pdu_ids.len().saturating_sub(offset)).unwrap_or(u64::MAX)
	} else {
		start.unsigned_abs()
	};

	let mut i = 0_u64;
	let mut count = 0_u64;
	let mut total_prev_events = 0_u64;
	let mut state_events = 0_u64;
	let mut missing_hash = 0_u64;
	let mut unique_hashes = HashSet::<u64>::new();
	let mut last_ssh: Option<u64> = None;
	let mut max_depth = 0_u64;
	let mut min_depth = u64::MAX;
	let server = self.services.globals.server_name();
	let room_version_str = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await
		.map_or_else(|_| "unknown".to_owned(), |v| v.to_string());
	let safe_room_id = room_id.to_string().replace('!', "").replace(':', "_");
	let path = format!("/tmp/local-dag-{safe_room_id}-v{room_version_str}-{server}.jsonl");
	let mut file = tokio::fs::File::create(&path)
		.await
		.map_err(|e| err!(Database("Failed to create file {path}: {e:?}")))?;

	let mut all_event_ids = HashSet::<OwnedEventId>::new();
	let mut referenced_as_prev = HashSet::<OwnedEventId>::new();

	for event_id in pdu_ids {
		if let Ok(end) = u64::try_from(end) {
			if i > end {
				break;
			}
		}
		if i >= actual_start {
			if let Ok(pdu_json) = self.services.rooms.timeline.get_pdu_json(&event_id).await {
				let mut obj: serde_json::Map<String, JsonValue> =
					serde_json::from_value(serde_json::to_value(&pdu_json)?)?;

				// Inject event_id into the raw JSON (required for v3+ rooms where it's missing)
				obj.insert(
					"event_id".to_owned(),
					JsonValue::String(event_id.as_str().to_owned()),
				);

				// V11+: strip room_id per MSC3820/MSC4291
				let is_v11 = room_version_str == "11";
				let is_v12_or_later = !matches!(
					room_version_str.as_str(),
					"1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "10" | "11"
				);
				let is_create = obj.get("type").and_then(|v| v.as_str()) == Some("m.room.create");
				if is_v12_or_later || (is_v11 && !is_create) {
					obj.remove("room_id");
				}

				let pdu_result = self.services.rooms.timeline.get_pdu(&event_id).await;
				if let Ok(pdu) = &pdu_result {
					if let Ok(ssh) = self
						.services
						.rooms
						.state_accessor
						.pdu_shortstatehash(pdu.event_id())
						.await
					{
						obj.insert("__shortstatehash".to_owned(), JsonValue::from(ssh));
						unique_hashes.insert(ssh);
						last_ssh = Some(ssh);
					} else {
						missing_hash = missing_hash.saturating_add(1);
					}

					if pdu.state_key.is_some() {
						state_events = state_events.saturating_add(1);
					}

					let eid = pdu.event_id().to_owned();
					all_event_ids.insert(eid);
					for prev in pdu.prev_events() {
						referenced_as_prev.insert(prev.to_owned());
					}
					let d: u64 = pdu.depth.into();
					max_depth = max_depth.max(d);
					min_depth = min_depth.min(d);
				}

				let json = serde_json::to_string(&obj)?;
				file.write_all(json.as_bytes()).await?;
				file.write_all(b"\n").await?;
				if print {
					self.write_str(&format!("{json}\n")).await?;
				}
				if let Ok(pdu) = &pdu_result {
					total_prev_events = total_prev_events
						.saturating_add(u64::try_from(pdu.prev_events().count()).unwrap_or(0));
				}
				count = count.saturating_add(1);
			}
		}
		i = i.saturating_add(1);
	}

	// Forward extremities: events not referenced as prev_events by any other event
	let heads_count = all_event_ids.difference(&referenced_as_prev).count();

	let (bf_whole, bf_frac) = if count > 0 {
		let scaled = total_prev_events
			.saturating_mul(1000)
			.checked_div(count)
			.unwrap_or(0);
		(scaled.checked_div(1000).unwrap_or(0), scaled % 1000)
	} else {
		(0, 0)
	};

	let room_ssh = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await
		.ok();

	let tip_match = match (last_ssh, room_ssh) {
		| (Some(tip), Some(room)) if tip == room => "✓ tip matches room state",
		| (Some(tip), Some(room)) => {
			let _ = (tip, room);
			"✗ tip DIVERGES from room state"
		},
		| _ => "? unknown",
	};

	// Rename to include depth range so successive runs don't overwrite
	let min_d = if min_depth == u64::MAX { 0 } else { min_depth };
	let final_path = format!(
		"/tmp/local-dag-{safe_room_id}-v{room_version_str}-{server}-d{min_d}-{max_depth}.jsonl"
	);
	if let Err(e) = tokio::fs::rename(&path, &final_path).await {
		warn!("Failed to rename {path} -> {final_path}: {e}");
	}
	let display_path = if tokio::fs::metadata(&final_path).await.is_ok() {
		&final_path
	} else {
		&path
	};

	let mut out = format!("Wrote {count} PDUs to {display_path}\n");
	writeln!(out, "```").expect("fmt");
	writeln!(out, "PDUs:           {count}").expect("fmt");
	writeln!(out, "State events:   {state_events}").expect("fmt");
	writeln!(out, "Branching:      {bf_whole}.{bf_frac:03} avg prev_events/PDU").expect("fmt");
	let (frag_whole, frag_frac) = if max_depth > 0 {
		let scaled = count
			.saturating_mul(1000)
			.checked_div(max_depth)
			.unwrap_or(0);
		(scaled.checked_div(1000).unwrap_or(0), scaled % 1000)
	} else {
		(0, 0)
	};
	writeln!(
		out,
		"Frag factor:    {frag_whole}.{frag_frac:03} ({count} events / {max_depth} depth, \
		 {heads_count} heads)"
	)
	.expect("fmt");
	writeln!(out, "Unique states:  {}", unique_hashes.len()).expect("fmt");
	writeln!(out, "Missing hash:   {missing_hash}").expect("fmt");
	if let Some(tip) = last_ssh {
		writeln!(out, "Tip SSH:        {tip}").expect("fmt");
	}
	if let Some(room) = room_ssh {
		writeln!(out, "Room SSH:       {room}").expect("fmt");
	}
	writeln!(out, "Status:         {tip_match}").expect("fmt");
	writeln!(out, "```").expect("fmt");

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn get_remote_dag(
	&self,
	room_id: OwnedRoomId,
	server: OwnedServerName,
	limit: i64,
	from: Option<OwnedEventId>,
	print: bool,
	verbose: bool,
) -> Result {
	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.");
	}

	if server == self.services.globals.server_name() {
		return Err!("Cannot fetch from ourselves. Use get-room-dag instead.");
	}

	let room_version = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await?;

	// Start from explicit event ID or latest local event
	let start_event_id = match from {
		| Some(eid) => eid,
		| None => self
			.services
			.rooms
			.timeline
			.latest_pdu_in_room(&room_id)
			.await?
			.event_id()
			.to_owned(),
	};

	let safe_room_id = room_id.to_string().replace('!', "").replace(':', "_");
	let path = format!("/tmp/remote-dag-{safe_room_id}-v{room_version}-{server}.jsonl");
	let mut file = tokio::fs::File::create(&path)
		.await
		.map_err(|e| err!(Database("Failed to create file {path}: {e:?}")))?;

	let mut seen = HashSet::<OwnedEventId>::new();
	let mut queue = VecDeque::from(vec![start_event_id]);
	let mut total = 0_usize;
	let mut total_prev_events = 0_u64;
	let mut batches = 0_usize;
	let mut min_depth = u64::MAX;
	let mut max_depth = 0_u64;
	let mut consecutive_errors = 0_usize;
	let batch_size = ruma::uint!(100);
	let start_time = tokio::time::Instant::now();

	info!("get-remote-dag: starting crawl from {server} for {room_id} (limit: {limit})");
	self.write_str(&format!("Fetching DAG from {server} for {room_id} (limit: {limit})...\n"))
		.await?;

	let unlimited = limit < 0;
	let limit = if unlimited {
		usize::MAX
	} else {
		usize::try_from(limit).unwrap_or(usize::MAX)
	};

	while !queue.is_empty() && total < limit {
		// Cap request queue to avoid 414 URI Too Long from reverse proxies.
		// Drain items from front so we don't lose unsent frontier items.
		let current_batch_size = 50.min(queue.len());
		let request_v: Vec<_> = queue.drain(..current_batch_size).collect();
		let request = ruma::api::federation::backfill::get_backfill::v1::Request {
			room_id: room_id.clone(),
			v: request_v.clone(),
			limit: batch_size,
		};

		batches = batches.saturating_add(1);
		let response = match self
			.services
			.sending
			.send_federation_request(&server, request)
			.await
		{
			| Ok(r) => {
				consecutive_errors = 0;
				r
			},
			| Err(e) => {
				let err_str = e.to_string();

				// 414 URI Too Long -- re-add only half the items to shrink next request
				if err_str.contains("414") {
					let keep = request_v.len() / 2;
					for id in request_v.into_iter().take(keep) {
						queue.push_front(id);
					}
					continue;
				}

				// Other errors: re-add all items to retry
				for id in request_v.into_iter().rev() {
					queue.push_front(id);
				}

				consecutive_errors = consecutive_errors.saturating_add(1);
				info!(
					"get-remote-dag: federation request failed after {total} PDUs in {batches} \
					 batches (attempt {consecutive_errors}/3): {e}"
				);
				if verbose {
					self.write_str(&format!(
						"Federation request failed (batch {batches}, queue={}, attempt \
						 {consecutive_errors}/3):\n```\n{e:?}\n```\n",
						queue.len()
					))
					.await?;
				} else {
					self.write_str(&format!(
						"Federation request failed (attempt {consecutive_errors}/3): {e}"
					))
					.await?;
				}
				if consecutive_errors >= 3 {
					self.write_str("Giving up after 3 consecutive failures.\n")
						.await?;
					break;
				}
				continue;
			},
		};

		if response.pdus.is_empty() {
			info!("get-remote-dag: server returned empty response after {total} PDUs");
			break;
		}

		// Response PDUs will add their prev_events to the queue below
		for raw_pdu in &response.pdus {
			let (event_id, value) = match self
				.services
				.server_keys
				.validate_and_add_event_id(raw_pdu, &room_version)
				.await
			{
				| Ok(r) => r,
				| Err(e) => {
					match conduwuit::matrix::event::gen_event_id_canonical_json(
						raw_pdu,
						&room_version,
					) {
						| Ok((eid, val)) => {
							warn!(
								"get_remote_dag: PDU {eid} failed sig verify, storing as \
								 rejected outlier: {e}"
							);
							self.services.rooms.outlier.add_pdu_outlier(
								&eid,
								&val,
								Some(&room_id),
							);
							self.services
								.rooms
								.pdu_metadata
								.mark_event_soft_failed(&eid);
							// Fall through so prev_events are still queued
							(eid, val)
						},
						| Err(err) => {
							warn!("get_remote_dag: Failed to canonicalize PDU: {err}");
							continue;
						},
					}
				},
			};

			if seen.contains(&event_id) {
				continue;
			}
			seen.insert(event_id.clone());

			let Ok(pdu) = PduEvent::from_id_val(&event_id, value.clone(), Some(room_id.as_ref()))
			else {
				continue;
			};

			// Write the original un-canonicalized raw JSON from the remote server
			// so signatures and unsigned data are perfectly preserved for imports.
			let mut export_val: serde_json::Map<String, serde_json::Value> =
				serde_json::from_str(raw_pdu.get())?;
			export_val
				.insert("event_id".to_owned(), serde_json::Value::String(event_id.to_string()));

			let room_features = conduwuit_core::RoomVersion::new(&room_version)
				.unwrap_or(conduwuit_core::RoomVersion::V1);
			let is_create =
				export_val.get("type").and_then(|v| v.as_str()) == Some("m.room.create");

			if room_features.strips_room_id(is_create) {
				export_val.remove("room_id");
			}
			let json = serde_json::to_string(&export_val)?;
			file.write_all(json.as_bytes()).await?;
			file.write_all(b"\n").await?;
			if print {
				self.write_str(&format!("{json}\n")).await?;
			}
			total_prev_events = total_prev_events
				.saturating_add(u64::try_from(pdu.prev_events().count()).unwrap_or(0));
			let depth: u64 = pdu.depth.into();
			min_depth = min_depth.min(depth);
			max_depth = max_depth.max(depth);
			total = total.saturating_add(1);

			if total.is_multiple_of(1000) {
				let elapsed = start_time.elapsed();
				info!(
					"get-remote-dag: {total} PDUs fetched from {server} in {elapsed:?} \
					 ({batches} batches, queue={})",
					queue.len()
				);
			}

			// Add prev_events to the queue for next iteration
			for prev in pdu.prev_events() {
				if !seen.contains(prev) {
					queue.push_back(prev.to_owned());
				}
			}

			if total >= limit {
				break;
			}
		}

		// Yield to avoid blocking
		tokio::task::yield_now().await;
	}

	let elapsed = start_time.elapsed();
	let (bf_whole, bf_frac) = if total > 0 {
		let divisor = u64::try_from(total).unwrap_or(1);
		let scaled = total_prev_events
			.saturating_mul(1000)
			.checked_div(divisor)
			.unwrap_or(0);
		(scaled.checked_div(1000).unwrap_or(0), scaled % 1000)
	} else {
		(0, 0)
	};

	info!(
		"get-remote-dag: complete — {total} PDUs from {server} in {elapsed:?} ({batches} \
		 batches, bf={bf_whole}.{bf_frac:03}, depth={min_depth}..{max_depth})"
	);

	// Rename to include depth range so successive runs don't overwrite
	let final_path = format!(
		"/tmp/remote-dag-{safe_room_id}-v{room_version}-{server}-d{min_depth}-{max_depth}.jsonl"
	);
	if let Err(e) = tokio::fs::rename(&path, &final_path).await {
		warn!("Failed to rename {path} -> {final_path}: {e}");
	}
	let display_path = if tokio::fs::metadata(&final_path).await.is_ok() {
		&final_path
	} else {
		&path
	};

	self.write_str(&format!(
		"\nSuccessfully fetched {total} PDUs from {server} to {display_path} (depth: \
		 {min_depth}..{max_depth}, branching factor: {bf_whole}.{bf_frac:03})"
	))
	.await
}

#[admin_command]
pub(super) async fn fetch_pdu(
	&self,
	room_id: OwnedRoomId,
	event_id: OwnedEventId,
	server: OwnedServerName,
	skip_auth: bool,
) -> Result {
	self.bail_restricted()?;

	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.");
	}

	if server == self.services.globals.server_name() {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs.",
		);
	}

	let mut room_version = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await?;

	let response = self
		.services
		.sending
		.send_federation_request(&server, get_event::v1::Request::new(event_id, None))
		.await?;

	// If the room's state is completely missing (falling back to V11) and we happen
	// to be fetching the `m.room.create` event to rescue it, we MUST extract the
	// real version from the PDU itself. Otherwise, canonicalization uses the
	// fallback rules, resulting in an entirely incorrect event ID.
	if let Ok(val) = serde_json::from_str::<serde_json::Value>(response.pdu.get()) {
		if val.get("type").and_then(|t| t.as_str()) == Some("m.room.create") {
			if let Some(v_str) = val
				.get("content")
				.and_then(|c| c.get("room_version"))
				.and_then(|v| v.as_str())
			{
				if let Ok(v) = RoomVersionId::try_from(v_str) {
					room_version = v;
				}
			} else {
				// Matrix spec: If room_version is omitted in m.room.create, it defaults to V1.
				room_version = RoomVersionId::V1;
			}
		}
	}

	let (event_id, value) = if skip_auth {
		let (eid, mut val) = conduwuit_core::matrix::event::gen_event_id_canonical_json(
			&response.pdu,
			&room_version,
		)?;
		val.insert("event_id".into(), ruma::CanonicalJsonValue::String(eid.as_str().into()));
		(eid, val)
	} else {
		self.services
			.server_keys
			.validate_and_add_event_id(&response.pdu, &room_version)
			.await?
	};

	let pdu = PduEvent::from_id_val(&event_id, value.clone(), Some(room_id.as_ref()))
		.map_err(|e| err!(Database("Invalid PDU: {e:?}")))?;

	if skip_auth {
		// Direct insert into timeline, bypassing all auth checks.
		let msg = match self
			.services
			.rooms
			.timeline
			.force_insert_pdu(&room_id, &event_id, &pdu, &value, true)
			.await
		{
			| Ok(pdu_id) => {
				format!("Force-inserted PDU {event_id} into timeline (skipped auth): {pdu_id:?}")
			},
			| Err(e) => format!("PDU {event_id}: {e}"),
		};
		return self.write_str(&msg).await;
	}

	let create_event = self
		.services
		.rooms
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await?;

	let result = Box::pin(
		self.services
			.rooms
			.event_handler
			.upgrade_outlier_to_timeline_pdu(
				pdu,
				value,
				&create_event,
				&server,
				&room_id,
				false,
				true,
			),
	)
	.await?;

	match result {
		| Some(id) => write!(self, "Successfully fetched and rescued PDU: {id:?}"),
		| None => write!(self, "PDU was already present or promoted successfully."),
	}
	.await
}

#[admin_command]
pub(super) async fn resend_receipts(
	&self,
	room_id: OwnedRoomId,
	server: Option<OwnedServerName>,
) -> Result {
	use std::collections::BTreeMap;

	use ruma::{
		OwnedEventId,
		api::federation::transactions::edu::{Edu, ReceiptContent, ReceiptData, ReceiptMap},
		events::{AnySyncEphemeralRoomEvent, receipt::ReceiptType},
	};

	// Collect latest receipt per local user in this room
	let mut latest_receipts: BTreeMap<
		OwnedUserId,
		(OwnedEventId, ruma::events::receipt::Receipt),
	> = BTreeMap::new();

	let receipts = self
		.services
		.rooms
		.read_receipt
		.readreceipts_since(&room_id, None);

	pin_mut!(receipts);
	while let Some((user_id, _count, raw_receipt)) = receipts.next().await {
		// Only resend our local users' receipts
		if !self.services.globals.server_is_ours(user_id.server_name()) {
			continue;
		}

		let Ok(event) =
			serde_json::from_str::<AnySyncEphemeralRoomEvent>(raw_receipt.json().get())
		else {
			continue;
		};

		let AnySyncEphemeralRoomEvent::Receipt(r) = event else {
			continue;
		};

		let Some((event_id, mut receipt_types)) = r.content.0.into_iter().next() else {
			continue;
		};

		let Some(users) = receipt_types.remove(&ReceiptType::Read) else {
			continue;
		};

		let Some(receipt) = users.into_iter().next().map(|(_, r)| r) else {
			continue;
		};

		// Keep only the latest per user (stream is ordered by count ascending)
		latest_receipts.insert(user_id.clone(), (event_id, receipt));
	}

	if latest_receipts.is_empty() {
		return self
			.write_str("No local user receipts found for this room.")
			.await;
	}

	// Build the receipt EDU
	let mut read = BTreeMap::new();
	for (user_id, (event_id, receipt)) in &latest_receipts {
		read.insert(user_id.clone(), ReceiptData {
			data: receipt.clone(),
			event_ids: vec![event_id.clone()],
		});
	}

	let receipt_map = ReceiptMap { read };
	let receipts_content = BTreeMap::from([(room_id.clone(), receipt_map)]);
	let edu = Edu::Receipt(ReceiptContent { receipts: receipts_content });

	let mut buf = conduwuit_service::sending::EduBuf::new();
	serde_json::to_writer(&mut buf, &edu)
		.map_err(|e| err!("Failed to serialize receipt EDU: {e}"))?;

	// Send to specific server or all participating servers
	if let Some(ref target_server) = server {
		self.services.sending.send_edu_server(target_server, buf)?;
		self.write_str(&format!(
			"Resent {} receipt(s) for room {} to server {}.",
			latest_receipts.len(),
			room_id,
			target_server
		))
		.await?;
	} else {
		self.services.sending.send_edu_room(&room_id, buf).await?;
		self.write_str(&format!(
			"Resent {} receipt(s) for room {} to all participating servers.",
			latest_receipts.len(),
			room_id
		))
		.await?;
	}

	Ok(())
}

#[admin_command]
pub(super) async fn repair_unsigned(&self, room_id: OwnedRoomId) -> Result {
	use conduwuit::PduCount;

	self.bail_restricted()?;

	let pdus_stream = self
		.services
		.rooms
		.timeline
		.pdus(&room_id, Some(PduCount::min()))
		.filter_map(|r| ready(r.ok()))
		.filter(|(_count, pdu)| ready(pdu.state_key().is_some()))
		.map(|(_count, pdu)| {
			let event_id = pdu.event_id().to_owned();
			let kind = pdu.kind().to_string();
			let state_key = pdu.state_key().unwrap_or_default().to_owned();
			async move {
				// Get the stored JSON
				let pdu_json = self.services.rooms.timeline.get_pdu_json(&event_id).await;

				// Try state snapshot lookup
				let prev_state = if let Ok(ssh) = self
					.services
					.rooms
					.state_accessor
					.pdu_shortstatehash(&event_id)
					.await
				{
					self.services
						.rooms
						.state_accessor
						.state_get(ssh, &kind.clone().into(), &state_key)
						.await
						.ok()
						.filter(|prev| prev.event_id() != event_id)
				} else {
					None
				};

				(event_id, kind, state_key, pdu_json, prev_state)
			}
		})
		.buffer_unordered(100);

	pin_mut!(pdus_stream);

	info!("repair_unsigned: starting streaming state event repair in {room_id}");

	let mut repaired = 0_usize;
	let mut skipped = 0_usize;
	let mut errors = 0_usize;

	while let Some((event_id, _kind, _state_key, pdu_json, prev_state)) = pdus_stream.next().await
	{
		let Ok(mut pdu_json) = pdu_json else {
			errors = errors.saturating_add(1);
			continue;
		};

		let unsigned = pdu_json.entry("unsigned".to_owned()).or_insert_with(|| {
			ruma::CanonicalJsonValue::Object(std::collections::BTreeMap::new())
		});

		let ruma::CanonicalJsonValue::Object(unsigned) = unsigned else {
			errors = errors.saturating_add(1);
			continue;
		};

		// If no state snapshot, try replaces_state fallback
		let prev_state = match prev_state {
			| Some(_) => prev_state,
			| None => {
				let replaces = unsigned
					.get("replaces_state")
					.and_then(|v| v.as_str())
					.and_then(|s| <&EventId>::try_from(s).ok())
					.filter(|eid| *eid != event_id);

				match replaces {
					| Some(prev_eid) => self.services.rooms.timeline.get_pdu(prev_eid).await.ok(),
					| None => {
						skipped = skipped.saturating_add(1);
						continue;
					},
				}
			},
		};

		// Populate from the previous state event
		if let Some(prev_state) = prev_state {
			if let Err(e) = conduwuit_service::rooms::timeline::update_unsigned_prev_content(
				&mut pdu_json,
				&prev_state,
			) {
				warn!("repair_unsigned: failed to update unsigned for {event_id}: {e}");
				errors = errors.saturating_add(1);
				continue;
			}
		}

		// Write back
		let Ok(pdu_id) = self.services.rooms.timeline.get_pdu_id(&event_id).await else {
			errors = errors.saturating_add(1);
			continue;
		};

		if let Err(e) = self
			.services
			.rooms
			.timeline
			.replace_pdu(&pdu_id, &pdu_json)
			.await
		{
			warn!("Failed to replace PDU {event_id}: {e}");
			errors = errors.saturating_add(1);
			continue;
		}

		repaired = repaired.saturating_add(1);

		let processed = repaired.saturating_add(skipped).saturating_add(errors);
		if processed.is_multiple_of(1000) {
			info!(
				"repair_unsigned: {processed} processed ({repaired} repaired, {skipped} skipped)"
			);
		}
	}

	self.write_str(&format!(
		"Repair complete for room {room_id}: {repaired} state events repaired, {skipped} \
		 skipped (no state snapshot), {errors} errors"
	))
	.await
}

#[admin_command]
pub(super) async fn compare_room_state(
	&self,
	room_id: OwnedRoomId,
	servers: Vec<OwnedServerName>,
	at_event: Option<OwnedEventId>,
	conflict: Option<OwnedUserId>,
	summary: bool,
	skip_sig_verify: bool,
) -> Result {
	use std::fmt::Write;

	if servers.is_empty() {
		return Err!(Request(InvalidParam("Provide at least one server to compare against.")));
	}
	let server = &servers[0];
	let room_version = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await?;
	let at_event_id = match at_event {
		| Some(event_id) => event_id,
		| None => self
			.services
			.rooms
			.timeline
			.latest_pdu_in_room(&room_id)
			.await?
			.event_id()
			.to_owned(),
	};

	// Fetch tip once — used for state-event detection and injection
	let tip_pdu_opt = self
		.services
		.rooms
		.timeline
		.get_pdu(&at_event_id)
		.await
		.ok();
	let tip_is_state_event = tip_pdu_opt
		.as_ref()
		.is_some_and(|pdu| pdu.state_key.is_some());

	let response = match self
		.services
		.sending
		.send_federation_request(server, get_room_state::v1::Request {
			room_id: room_id.clone(),
			event_id: at_event_id.clone(),
		})
		.await
	{
		| Ok(r) => r,
		| Err(e) => {
			return self
				.write_str(&format!(
					"Failed to fetch state from {server} at event {at_event_id}: {e}\n\nThe \
					 remote server may not have this event. Try specifying a known-shared event \
					 with --at-event, or compare with a different server.",
				))
				.await;
		},
	};

	// Single pass: build remote state map + count remote members
	let mut remote_state = HashMap::new();
	let mut event_timestamps: HashMap<OwnedEventId, u64> = HashMap::new();
	// (membership_or_empty, sender)
	let mut event_meta: HashMap<OwnedEventId, (String, String)> = HashMap::new();
	let mut skipped = 0_usize;
	let mut remote_joined: HashSet<String> = HashSet::new();
	let mut remote_invited: HashSet<String> = HashSet::new();

	// Conflict tracking: (server_label, event_id, ts, membership, displayname,
	// avatar_url)
	let conflict_key = conflict
		.as_ref()
		.map(|u| ("m.room.member".to_owned(), u.to_string()));
	let mut conflict_entries: Vec<(String, String, u64, String, String, String)> = Vec::new();

	for pdu_raw in &response.pdus {
		let (event_id, value) = if skip_sig_verify {
			match conduwuit::matrix::event::gen_event_id_canonical_json(pdu_raw, &room_version) {
				| Ok((eid, val)) => (eid, val),
				| Err(e) => {
					warn!("Skipping PDU, canonicalization failed: {e}");
					skipped = skipped.saturating_add(1);
					continue;
				},
			}
		} else {
			match self
				.services
				.server_keys
				.validate_and_add_event_id(pdu_raw, &room_version)
				.await
			{
				| Ok(r) => r,
				| Err(e) => {
					// Persist as rejected outlier so the event is available for
					// auth chain lookups and state resolution context
					match conduwuit::matrix::event::gen_event_id_canonical_json(
						pdu_raw,
						&room_version,
					) {
						| Ok((eid, val)) => {
							warn!(
								"PDU {eid} failed signature verification, storing as rejected \
								 outlier: {e}"
							);
							self.services.rooms.outlier.add_pdu_outlier(
								&eid,
								&val,
								Some(&room_id),
							);
							self.services
								.rooms
								.pdu_metadata
								.mark_event_soft_failed(&eid);
							// Still count membership for the remote's totals —
							// the remote sent this as part of their state.
							if let Ok(pdu) =
								PduEvent::from_id_val(&eid, val, Some(room_id.as_ref()))
							{
								if pdu.kind == TimelineEventType::RoomMember {
									if let Some(state_key) = &pdu.state_key {
										let content: JsonValue = pdu.get_content_as_value();
										let membership = content
											.get("membership")
											.and_then(|v| v.as_str())
											.unwrap_or("unknown");
										match membership {
											| "join" => {
												remote_joined.insert(state_key.to_string());
												remote_invited.remove(state_key.as_str());
											},
											| "invite" => {
												remote_invited.insert(state_key.to_string());
												remote_joined.remove(state_key.as_str());
											},
											| _ => {
												remote_joined.remove(state_key.as_str());
												remote_invited.remove(state_key.as_str());
											},
										}
									}
								}
								if let Some(state_key) = &pdu.state_key {
									remote_state.insert(
										(pdu.kind.to_string(), state_key.to_string()),
										eid.clone(),
									);
								}
								event_timestamps
									.insert(eid.clone(), u64::from(pdu.origin_server_ts));
								let content: JsonValue = pdu.get_content_as_value();
								let membership = content
									.get("membership")
									.and_then(|v| v.as_str())
									.unwrap_or("")
									.to_owned();
								event_meta
									.insert(eid.clone(), (membership, pdu.sender().to_string()));
							}
							skipped = skipped.saturating_add(1);
							continue;
						},
						| Err(e2) => {
							warn!("Skipping PDU, canonicalization failed: {e2}");
							skipped = skipped.saturating_add(1);
							continue;
						},
					}
				},
			}
		};

		let pdu = PduEvent::from_id_val(&event_id, value, Some(room_id.as_ref()))?;
		event_timestamps.insert(event_id.clone(), u64::from(pdu.origin_server_ts));
		if let Some(state_key) = &pdu.state_key {
			remote_state.insert((pdu.kind.to_string(), state_key.to_string()), event_id.clone());
		}
		// Store metadata for richer diff output
		{
			let content: JsonValue = pdu.get_content_as_value();
			let membership = content
				.get("membership")
				.and_then(|v| v.as_str())
				.unwrap_or("")
				.to_owned();
			event_meta.insert(event_id.clone(), (membership, pdu.sender().to_string()))
		};

		if pdu.kind == TimelineEventType::RoomMember {
			if let Some(state_key) = &pdu.state_key {
				let content: JsonValue = pdu.get_content_as_value();
				let membership = content
					.get("membership")
					.and_then(|v| v.as_str())
					.unwrap_or("unknown");
				match membership {
					| "join" => {
						remote_joined.insert(state_key.to_string());
						remote_invited.remove(state_key.as_str());
					},
					| "invite" => {
						remote_invited.insert(state_key.to_string());
						remote_joined.remove(state_key.as_str());
					},
					| _ => {
						remote_joined.remove(state_key.as_str());
						remote_invited.remove(state_key.as_str());
					},
				}

				if let Some(ref ck) = conflict_key {
					if *state_key == ck.1 {
						let displayname = content
							.get("displayname")
							.and_then(|v| v.as_str())
							.unwrap_or("(none)")
							.to_owned();
						let avatar = content
							.get("avatar_url")
							.and_then(|v| v.as_str())
							.unwrap_or("(none)")
							.to_owned();
						let ts = u64::from(pdu.origin_server_ts);
						conflict_entries.push((
							server.to_string(),
							event_id.to_string(),
							ts,
							membership.to_owned(),
							displayname,
							avatar,
						));
					}
				}
			}
		}
	}

	let local_state_hash = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	// Inject tip event into remote state (uses cached tip_pdu_opt)
	if tip_is_state_event {
		if let Some(ref tip_pdu) = tip_pdu_opt {
			if let Some(state_key) = &tip_pdu.state_key {
				remote_state.insert(
					(tip_pdu.kind.to_string(), state_key.to_string()),
					at_event_id.clone(),
				);

				if tip_pdu.kind == TimelineEventType::RoomMember {
					let content: JsonValue = tip_pdu.get_content_as_value();
					match content.get("membership").and_then(|v| v.as_str()) {
						| Some("join") => {
							remote_joined.insert(state_key.to_string());
							remote_invited.remove(state_key.as_str());
						},
						| Some("invite") => {
							remote_invited.insert(state_key.to_string());
							remote_joined.remove(state_key.as_str());
						},
						| _ => {
							remote_joined.remove(state_key.as_str());
							remote_invited.remove(state_key.as_str());
						},
					}
				}
			}
		}
	}

	// Single pass: build local state map + count local members
	let mut local_state: HashMap<(String, String), OwnedEventId> = HashMap::new();
	let mut local_state_joined = 0_usize;
	let mut local_state_invited = 0_usize;
	{
		let state_full = self
			.services
			.rooms
			.state_accessor
			.state_full(local_state_hash);
		pin_mut!(state_full);
		while let Some(((event_type, state_key), pdu)) = state_full.next().await {
			let eid = pdu.event_id().to_owned();
			event_timestamps.insert(eid.clone(), pdu.origin_server_ts().0.into());
			local_state.insert((event_type.to_string(), state_key.to_string()), eid.clone());
			// Store metadata for richer diff output
			{
				let content: JsonValue = pdu.get_content_as_value();
				let membership = content
					.get("membership")
					.and_then(|v| v.as_str())
					.unwrap_or("")
					.to_owned();
				event_meta.insert(eid.clone(), (membership, pdu.sender().to_string()))
			};

			if event_type == StateEventType::RoomMember {
				let content: JsonValue = pdu.get_content_as_value();
				let membership = content
					.get("membership")
					.and_then(|v| v.as_str())
					.unwrap_or("unknown");
				match membership {
					| "join" => local_state_joined = local_state_joined.saturating_add(1),
					| "invite" => {
						local_state_invited = local_state_invited.saturating_add(1);
					},
					| _ => {},
				}

				if let Some(ref ck) = conflict_key {
					if &*state_key == ck.1.as_str() {
						let displayname = content
							.get("displayname")
							.and_then(|v| v.as_str())
							.unwrap_or("(none)")
							.to_owned();
						let avatar = content
							.get("avatar_url")
							.and_then(|v| v.as_str())
							.unwrap_or("(none)")
							.to_owned();
						let ts: u64 = pdu.origin_server_ts().0.into();
						conflict_entries.push((
							"local".to_owned(),
							eid.to_string(),
							ts,
							membership.to_owned(),
							displayname,
							avatar,
						));
					}
				}
			}
		}
	}

	let mut missing_locally = Vec::new();
	for (key, event_id) in &remote_state {
		if local_state.get(key) != Some(event_id) {
			let ts = event_timestamps.get(event_id).copied().unwrap_or_else(|| {
				tip_pdu_opt
					.as_ref()
					.filter(|tip| tip.event_id() == event_id)
					.map_or(0, |tip| u64::from(tip.origin_server_ts))
			});
			let extra = fmt_event_meta(&key.0, event_id, &event_meta);
			missing_locally
				.push((ts, format!("{event_id} ({} {}) {}{extra}", key.0, key.1, format_ts(ts))));
		}
	}
	missing_locally.sort_by_key(|(ts, _)| *ts);

	let mut extra_locally = Vec::new();
	for (key, event_id) in &local_state {
		if remote_state.get(key) != Some(event_id) {
			let ts = event_timestamps.get(event_id).copied().unwrap_or_else(|| {
				tip_pdu_opt
					.as_ref()
					.filter(|tip| tip.event_id() == event_id)
					.map_or(0, |tip| u64::from(tip.origin_server_ts))
			});
			let extra = fmt_event_meta(&key.0, event_id, &event_meta);
			extra_locally
				.push((ts, format!("{event_id} ({} {}) {}{extra}", key.0, key.1, format_ts(ts))));
		}
	}
	extra_locally.sort_by_key(|(ts, _)| *ts);

	let cached_joined = self
		.services
		.rooms
		.state_cache
		.room_joined_count(&room_id)
		.await
		.unwrap_or(0);

	let latest_local = self
		.services
		.rooms
		.timeline
		.latest_pdu_in_room(&room_id)
		.await?;
	let latest_local_id = latest_local.event_id().to_owned();

	let extremity_count = self
		.services
		.rooms
		.state
		.get_forward_extremities(&room_id)
		.count()
		.await;

	let cache_status = if u64::try_from(local_state_joined).unwrap_or(0) == cached_joined {
		"✓"
	} else {
		"✗ MISMATCH"
	};

	let mut out = String::from("```\n");
	writeln!(
		out,
		"Room State Comparison for {room_id} vs {server}\nat_event (sent to remote): \
		 {at_event_id}\nlocal tip: {latest_local_id}\nMissing locally: {}\nExtra locally: \
		 {}\nSkipped (bad sig): {skipped}",
		missing_locally.len(),
		extra_locally.len()
	)?;
	writeln!(out)?;
	writeln!(out, "Room SSH:        {local_state_hash}")?;
	writeln!(out, "Extremities:     {extremity_count}")?;
	writeln!(
		out,
		"Local joined:    state={local_state_joined}, cache={cached_joined} {cache_status}"
	)?;
	writeln!(out, "Local invited:   state={local_state_invited}")?;
	writeln!(out, "Remote joined:   {}", remote_joined.len())?;
	writeln!(out, "Remote invited:  {}", remote_invited.len())?;
	if tip_is_state_event {
		writeln!(
			out,
			"NOTE: Tip is a state event — injected into remote state for state-after comparison"
		)?;
	}
	if !summary {
		writeln!(out)?;
		fmt_list(&mut out, "Missing IDs", &missing_locally)?;
		fmt_list(&mut out, "Extra IDs", &extra_locally)?;
	}
	writeln!(out, "```")?;
	self.write_str(&out).await?;

	// If additional servers provided, compare first server against each
	if servers.len() > 1 {
		let tip_key: Option<(String, String)> = tip_pdu_opt.as_ref().and_then(|pdu| {
			pdu.state_key
				.as_ref()
				.map(|sk| (pdu.kind.to_string(), sk.to_string()))
		});

		for cmp_server in &servers[1..] {
			let response = match self
				.services
				.sending
				.send_federation_request(cmp_server, get_room_state::v1::Request {
					room_id: room_id.clone(),
					event_id: at_event_id.clone(),
				})
				.await
			{
				| Ok(r) => r,
				| Err(e) => {
					self.write_str(&format!("\n--- vs {cmp_server}: ERROR: {e}\n"))
						.await?;
					continue;
				},
			};

			let mut server_state = HashMap::new();
			let mut verify_errors = 0_usize;
			let mut cmp_joined = 0_usize;
			let mut cmp_invited = 0_usize;
			for pdu_raw in &response.pdus {
				let (event_id, value) = match if skip_sig_verify {
					conduwuit::matrix::event::gen_event_id_canonical_json(pdu_raw, &room_version)
				} else {
					self.services
						.server_keys
						.validate_and_add_event_id(pdu_raw, &room_version)
						.await
				} {
					| Ok(r) => r,
					| Err(e) => {
						if let Ok((eid, val)) =
							conduwuit::matrix::event::gen_event_id_canonical_json(
								pdu_raw,
								&room_version,
							) {
							warn!(
								"compare_room_state: PDU {eid} failed verification, storing as \
								 rejected outlier: {e}"
							);
							self.services.rooms.outlier.add_pdu_outlier(
								&eid,
								&val,
								Some(&room_id),
							);
							self.services
								.rooms
								.pdu_metadata
								.mark_event_soft_failed(&eid);
							// Still count membership — remote sent this as
							// part of their state.
							if let Ok(pdu) =
								PduEvent::from_id_val(&eid, val, Some(room_id.as_ref()))
							{
								event_timestamps
									.insert(eid.clone(), u64::from(pdu.origin_server_ts));
								if let Some(state_key) = &pdu.state_key {
									server_state.insert(
										(pdu.kind.to_string(), state_key.to_string()),
										eid.clone(),
									);
									if !event_meta.contains_key(&eid) {
										let content: JsonValue = pdu.get_content_as_value();
										let membership = content
											.get("membership")
											.and_then(|v| v.as_str())
											.unwrap_or("")
											.to_owned();
										event_meta.insert(
											eid.clone(),
											(membership, pdu.sender().to_string()),
										);
									}
								}
								if pdu.kind == TimelineEventType::RoomMember {
									if pdu.state_key.is_some() {
										let content: JsonValue = pdu.get_content_as_value();
										let membership = content
											.get("membership")
											.and_then(|v| v.as_str())
											.unwrap_or("unknown");
										match membership {
											| "join" => {
												cmp_joined = cmp_joined.saturating_add(1);
											},
											| "invite" => {
												cmp_invited = cmp_invited.saturating_add(1);
											},
											| _ => {},
										}
									}
								}
							}
						}
						verify_errors = verify_errors.saturating_add(1);
						continue;
					},
				};
				let Ok(pdu) = PduEvent::from_id_val(&event_id, value, Some(room_id.as_ref()))
				else {
					continue;
				};
				event_timestamps.insert(event_id.clone(), u64::from(pdu.origin_server_ts));
				if let Some(state_key) = &pdu.state_key {
					server_state
						.insert((pdu.kind.to_string(), state_key.to_string()), event_id.clone());

					// Store metadata for richer diff output
					if !event_meta.contains_key(&event_id) {
						let content: JsonValue = pdu.get_content_as_value();
						let membership = content
							.get("membership")
							.and_then(|v| v.as_str())
							.unwrap_or("")
							.to_owned();
						event_meta
							.insert(event_id.clone(), (membership, pdu.sender().to_string()));
					}

					if pdu.kind == TimelineEventType::RoomMember {
						let content: JsonValue = pdu.get_content_as_value();
						let membership = content
							.get("membership")
							.and_then(|v| v.as_str())
							.unwrap_or("unknown");
						match membership {
							| "join" => {
								cmp_joined = cmp_joined.saturating_add(1);
							},
							| "invite" => {
								cmp_invited = cmp_invited.saturating_add(1);
							},
							| _ => {},
						}

						if let Some(ref ck) = conflict_key {
							if *state_key == ck.1 {
								let displayname = content
									.get("displayname")
									.and_then(|v| v.as_str())
									.unwrap_or("(none)")
									.to_owned();
								let avatar = content
									.get("avatar_url")
									.and_then(|v| v.as_str())
									.unwrap_or("(none)")
									.to_owned();
								let ts = u64::from(pdu.origin_server_ts);
								conflict_entries.push((
									cmp_server.to_string(),
									event_id.to_string(),
									ts,
									membership.to_owned(),
									displayname,
									avatar,
								));
							}
						}
					}
				}
			}

			if let Some(ref key) = tip_key {
				server_state.insert(key.clone(), at_event_id.clone());
			}

			let mut only_on_first = Vec::new();
			for (key, event_id) in &remote_state {
				if server_state.get(key) != Some(event_id) {
					let ts = event_timestamps.get(event_id).copied().unwrap_or_else(|| {
						tip_pdu_opt
							.as_ref()
							.filter(|tip| tip.event_id() == event_id)
							.map_or(0, |tip| u64::from(tip.origin_server_ts))
					});
					let extra = fmt_event_meta(&key.0, event_id, &event_meta);
					only_on_first.push((
						ts,
						format!("{event_id} ({} {}) {}{extra}", key.0, key.1, format_ts(ts)),
					));
				}
			}
			only_on_first.sort_by_key(|(ts, _)| *ts);

			let mut only_on_cmp = Vec::new();
			for (key, event_id) in &server_state {
				if remote_state.get(key) != Some(event_id) {
					let ts = event_timestamps.get(event_id).copied().unwrap_or_else(|| {
						tip_pdu_opt
							.as_ref()
							.filter(|tip| tip.event_id() == event_id)
							.map_or(0, |tip| u64::from(tip.origin_server_ts))
					});
					let extra = fmt_event_meta(&key.0, event_id, &event_meta);
					only_on_cmp.push((
						ts,
						format!("{event_id} ({} {}) {}{extra}", key.0, key.1, format_ts(ts)),
					));
				}
			}
			only_on_cmp.sort_by_key(|(ts, _)| *ts);

			let mut section = format!(
				"```\n--- {server} vs {cmp_server}:\nOnly on {server}: {}  Only on \
				 {cmp_server}: {}\n{cmp_server} joined: {cmp_joined}, invited: {cmp_invited}\n",
				only_on_first.len(),
				only_on_cmp.len()
			);
			if verify_errors > 0 {
				writeln!(section, "Skipped (bad sig): {verify_errors}")?;
			}
			if !summary {
				fmt_list(&mut section, &format!("IDs only on {server}"), &only_on_first)?;
				fmt_list(&mut section, &format!("IDs only on {cmp_server}"), &only_on_cmp)?;
			}
			writeln!(section, "```")?;
			self.write_str(&section).await?;
		}
	}
	// Output conflict summary if requested
	if let Some(ref user) = conflict {
		if !conflict_entries.is_empty() {
			use std::fmt::Write;

			let mut out = format!("\n--- Conflict detail for {user}:\n```\n");
			for (srv, eid, ts, membership, displayname, avatar) in &conflict_entries {
				writeln!(out, "{srv}:")?;
				writeln!(out, "  event:       {eid}")?;
				writeln!(out, "  timestamp:   {}", format_ts(*ts))?;
				writeln!(out, "  membership:  {membership}")?;
				writeln!(out, "  displayname: {displayname}")?;
				writeln!(out, "  avatar_url:  {avatar}")?;
			}
			writeln!(out, "```")?;
			self.write_str(&out).await?;
		} else {
			self.write_str(&format!("\n--- Conflict: {user} not found in any server state\n"))
				.await?;
		}
	}

	Ok(())
}

fn fmt_list(out: &mut String, label: &str, items: &[(u64, String)]) -> std::fmt::Result {
	use std::fmt::Write;

	write!(out, "{label}: [")?;
	for (_, item) in items {
		write!(out, "\n  {item}")?;
	}
	writeln!(out, "{}]", if items.is_empty() { "" } else { "\n" })
}

/// Returns a suffix like " [join]" or " [by @admin:hs]" for richer diff output.
fn fmt_event_meta(
	event_type: &str,
	event_id: &OwnedEventId,
	meta: &HashMap<OwnedEventId, (String, String)>,
) -> String {
	let Some((membership, sender)) = meta.get(event_id) else {
		return String::new();
	};
	match event_type {
		| "m.room.member" if !membership.is_empty() => format!(" [{membership}]"),
		| "m.room.power_levels" => format!(" [by {sender}]"),
		| _ => String::new(),
	}
}

#[admin_command]
#[allow(clippy::fn_params_excessive_bools)]
pub(super) async fn heal_room(
	&self,
	room_id: OwnedRoomId,
	server: OwnedServerName,
	nuclear: bool,
	execute: bool,
	resync_state: bool,
	purge_after: bool,
) -> Result {
	self.bail_restricted()?;

	let dry_run = !execute;

	// Phase 1: Rescue existing local outliers first (no network)
	// Only pass force=true when nuclear is set; otherwise respect auth checks
	if !dry_run {
		self.write_str(&format!("Phase 1: Rescuing local outliers in {room_id}...\n"))
			.await?;
		Box::pin(self.rescue_room(room_id.clone(), nuclear, nuclear, false, None, false)).await?;
	} else {
		self.write_str(&format!("Phase 1: [dry-run] Would rescue local outliers in {room_id}\n"))
			.await?;
	}

	// Phase 2: Walk the DAG to find genuinely missing events
	self.write_str("Phase 2: Scanning DAG for gaps...\n")
		.await?;
	let room_version = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await?;
	let latest_event_id = self
		.services
		.rooms
		.timeline
		.latest_pdu_in_room(&room_id)
		.await?
		.event_id()
		.to_owned();

	let latest = self
		.services
		.rooms
		.timeline
		.get_pdu(&latest_event_id)
		.await?;

	let mut queue: VecDeque<OwnedEventId> = latest.prev_events().map(ToOwned::to_owned).collect();
	queue.extend(latest.auth_events().map(ToOwned::to_owned));
	let mut seen = HashSet::<OwnedEventId>::new();
	let mut fetched = 0_usize;
	let mut local_found = 0_usize;
	drop(latest);

	while let Some(event_id) = queue.pop_front() {
		if seen.contains(&event_id) {
			continue;
		}
		seen.insert(event_id.clone());

		// Check local sources: timeline first, then outlier table
		if let Ok(pdu) = self.services.rooms.timeline.get_pdu(&event_id).await {
			// Already in timeline — just walk its parents (no fetch needed)
			local_found = local_found.saturating_add(1);
			if nuclear {
				queue.extend(pdu.prev_events().map(ToOwned::to_owned));
				queue.extend(pdu.auth_events().map(ToOwned::to_owned));
			}
			continue;
		}

		// Check outlier table
		if let Ok(pdu) = self.services.rooms.outlier.get_pdu_outlier(&event_id).await {
			// Present locally as outlier — walk parents, rescue will handle it
			local_found = local_found.saturating_add(1);
			queue.extend(pdu.prev_events().map(ToOwned::to_owned));
			queue.extend(pdu.auth_events().map(ToOwned::to_owned));
			continue;
		}

		if dry_run {
			fetched = fetched.saturating_add(1);
			continue;
		}

		// Genuinely missing — fetch from federation
		let Ok(response) = self
			.services
			.sending
			.send_federation_request(&server, get_event::v1::Request::new(event_id.clone(), None))
			.await
		else {
			continue;
		};

		let Ok((eid, value)) = self
			.services
			.server_keys
			.validate_and_add_event_id(&response.pdu, &room_version)
			.await
		else {
			continue;
		};

		let Ok(pdu) = PduEvent::from_id_val(&eid, value.clone(), Some(room_id.as_ref())) else {
			continue;
		};

		self.services
			.rooms
			.outlier
			.add_pdu_outlier(&eid, &value, Some(&room_id));
		// Clear rejection/soft-fail markers for forcefully imported state
		self.services.rooms.pdu_metadata.clear_pdu_markers(&eid);
		queue.extend(pdu.prev_events().map(ToOwned::to_owned));
		queue.extend(pdu.auth_events().map(ToOwned::to_owned));
		fetched = fetched.saturating_add(1);

		// Yield periodically to avoid blocking the executor
		if fetched.is_multiple_of(10) {
			tokio::task::yield_now().await;
		}
	}

	self.write_str(&format!(
		"Phase 2: Scanned {seen} events ({local_found} local, {fetched} {action})\n",
		seen = seen.len(),
		action = if dry_run { "would fetch" } else { "fetched" },
	))
	.await?;

	if dry_run {
		return self
			.write_str("Dry run complete. No changes made. Pass --execute to apply.")
			.await;
	}

	// Phase 3: Rescue any newly-fetched outliers
	if fetched > 0 {
		self.write_str(&format!("Phase 3: Fetched {fetched} missing events, rescuing..."))
			.await?;
		Box::pin(self.rescue_room(room_id.clone(), nuclear, nuclear, false, None, false)).await?;
	} else {
		self.write_str("Phase 3: No missing events found (DAG is complete locally).")
			.await?;
	}

	// Phase 4: Reorder timeline by origin_server_ts so auth checks work correctly
	self.write_str("Phase 4: Reordering timeline by timestamp...")
		.await?;
	Box::pin(self.reorder_timeline(room_id.clone(), false, None)).await?;
	self.write_str("Phase 4: Reordered timeline.").await?;

	// Phase 5: Resync state from the remote server (opt-in)
	if resync_state {
		self.write_str("Phase 5: Resyncing room state from server...")
			.await?;
		Box::pin(self.force_set_state(
			room_id.clone(),
			vec![server],
			None,
			nuclear,
			false,
			false,
			None,
			None,
			false,
			false, // skip_membership_rebuild
		))
		.await?;

		// Phase 5b: Rescue any outliers created by Phase 5's state resync
		self.write_str("Phase 5b: Rescuing state outliers from resync...")
			.await?;
		Box::pin(self.rescue_room(room_id.clone(), nuclear, nuclear, false, None, false)).await?;
	} else {
		self.write_str("Phase 5: Skipped state resync (pass --resync-state to enable).")
			.await?;
	}

	// Phase 6: Purge stuck outliers (events that now exist in both tables)
	if purge_after {
		self.write_str("Phase 6: Purging stuck outliers...").await?;
		let room_alias = OwnedRoomOrAliasId::from(room_id);
		Box::pin(self.purge_outliers(None, Some(room_alias), None, false, false)).await?;
	}

	Ok(())
}

#[admin_command]
pub(super) async fn import_outliers(&self, jsonl: String) -> Result {
	self.bail_restricted()?;
	let mut count = 0_usize;

	for line in jsonl.lines() {
		if line.trim().is_empty() {
			continue;
		}

		let pdu: CanonicalJsonObject = serde_json::from_str(line).map_err(|e| {
			err!(
				"Failed to parse PDU JSON: {e:?}. Make sure it's valid JSON on each line of the \
				 code block."
			)
		})?;

		let event_id = pdu
			.get("event_id")
			.and_then(ruma::CanonicalJsonValue::as_str)
			.and_then(|id| OwnedEventId::parse(id).ok())
			.ok_or_else(|| err!("Missing or invalid event_id in PDU JSON"))?;

		self.services
			.rooms
			.outlier
			.add_pdu_outlier(&event_id, &pdu, None);
		count = count.saturating_add(1);
	}

	self.write_str(&format!("Successfully imported {count} outliers."))
		.await
}

#[admin_command]
pub(super) async fn import_pdus(
	&self,
	room_id: OwnedRoomId,
	path: String,
	skip_auth: bool,
	skip_sig_verify: bool,
) -> Result {
	use tokio::io::{AsyncBufReadExt, BufReader};

	self.bail_restricted()?;

	let file = tokio::fs::File::open(&path)
		.await
		.map_err(|e| err!("Failed to open file {path}: {e:?}"))?;
	let mut lines = BufReader::new(file).lines();
	let room_version = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await?;
	let origin = room_id
		.server_name()
		.filter(|s| !self.services.globals.server_is_ours(s))
		.unwrap_or_else(|| self.services.globals.server_name());

	let mut inserted = 0_usize;
	let mut rejected = 0_usize;
	let mut failed = 0_usize;
	let mut total = 0_usize;

	let mode = match (skip_auth, skip_sig_verify) {
		| (true, _) => "force-insert (skip-auth)",
		| (_, true) => "auth-checked (skip-sig-verify)",
		| _ => "full pipeline",
	};

	self.write_str(&format!(
		"Importing PDUs from {path} into {room_id} [{mode}] (streaming)...\n"
	))
	.await?;

	// Helper: extract event_id from raw JSON value
	let extract_event_id = |value: &CanonicalJsonObject| -> Option<OwnedEventId> {
		value
			.get("event_id")
			.and_then(ruma::CanonicalJsonValue::as_str)
			.and_then(|id| OwnedEventId::parse(id).ok())
	};

	// Fetch the room's create event for auth checking (needed by
	// handle_outlier_pdu)
	let create_event = self
		.services
		.rooms
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await
		.ok();

	while let Ok(Some(line)) = lines.next_line().await {
		if line.trim().is_empty() {
			continue;
		}
		total = total.saturating_add(1);

		let result: Result<bool> = async {
			let mut value: CanonicalJsonObject = serde_json::from_str(&line)
				.map_err(|e| err!("Failed to parse JSON line: {e}"))?;

			// Strip diagnostic/internal fields that were injected during export.
			value.remove("__shortstatehash");
			value.remove("prev_state_events");
			value.remove("state_jump_pointers");

			let room_features = conduwuit_core::RoomVersion::new(&room_version)
				.unwrap_or(conduwuit_core::RoomVersion::V1);
			let is_create = value.get("type").and_then(ruma::CanonicalJsonValue::as_str)
				== Some("m.room.create");

			if room_features.strips_room_id(is_create) {
				value.remove("room_id");
			}

			if skip_auth {
				let eid = extract_event_id(&value).ok_or_else(|| err!("missing event_id"))?;
				let pdu = PduEvent::from_id_val(&eid, value.clone(), Some(room_id.as_ref()))?;
				self.services
					.rooms
					.timeline
					.force_insert_pdu(&room_id, &eid, &pdu, &value, true)
					.await
					.map(|_| true)
			} else {
				let (eid, val) = if skip_sig_verify {
					(extract_event_id(&value).ok_or_else(|| err!("missing event_id"))?, value)
				} else {
					// Build RawValue for sig verification from the canonical object.
					// Strip event_id for v3+ rooms (not part of signed content).
					// V1/V2 rooms require event_id for sig verification.
					let mut raw_val = value.clone();
					if room_version != RoomVersionId::V1 && room_version != RoomVersionId::V2 {
						raw_val.remove("event_id");
					}
					let raw = serde_json::value::RawValue::from_string(serde_json::to_string(
						&raw_val,
					)?)
					.map_err(|e| err!("raw value: {e}"))?;

					match self
						.services
						.server_keys
						.validate_and_add_event_id(&raw, &room_version)
						.await
					{
						| Ok(result) => result,
						| Err(e) => {
							// Sig verification failed — persist as soft-failed outlier so the
							// event is available for auth chain lookups and state context
							let eid = extract_event_id(&value)
								.ok_or_else(|| err!("missing event_id"))?;

							warn!(
								"import_pdus: Event {eid} failed verification: {e}\n  PDU: {}",
								serde_json::to_string_pretty(&value).unwrap_or_default(),
							);

							// Store as outlier
							self.services.rooms.outlier.add_pdu_outlier(
								&eid,
								&value,
								Some(&room_id),
							);

							// Mark as soft-failed (unverifiable, not proven fraudulent)
							self.services
								.rooms
								.pdu_metadata
								.mark_event_soft_failed(&eid);

							return Ok(false);
						},
					}
				};

				// Local-only auth: handle_outlier_pdu checks auth_events from local DB,
				// runs auth_check, and persists as outlier. auth_events_known=true skips
				// federation fetches for missing auth events.
				let (pdu, _json) = self
					.services
					.rooms
					.event_handler
					.handle_outlier_pdu(
						origin,
						create_event.as_ref(),
						&eid,
						&room_id,
						val,
						true,
						true,
					)
					.await?;

				// Promote from outlier to timeline
				self.services
					.rooms
					.timeline
					.promote_outlier(&room_id, &eid)
					.await?;
				let _ = pdu; // used by handle_outlier_pdu internally
				Ok(true)
			}
		}
		.await;

		match result {
			| Ok(true) => inserted = inserted.saturating_add(1),
			| Ok(false) => rejected = rejected.saturating_add(1),
			| Err(e) => {
				warn!("import_pdus: {e}");
				failed = failed.saturating_add(1);
			},
		}

		let done = inserted.saturating_add(failed).saturating_add(rejected);
		if done.is_multiple_of(1000) {
			info!(
				"import_pdus: {done}/{total} ({inserted} ok, {rejected} rejected, {failed} err)"
			);
		}
	}

	self.services
		.rooms
		.timeline
		.recalculate_extremities(&room_id, 500, true)
		.await?;

	self.write_str(&format!(
		"\nImported {inserted} PDUs, {rejected} stored as rejected outliers, {failed} errors \
		 out of {total} total for {room_id}. DAG Extremities recalculated. Run \
		 `force-set-room-state` to finalize."
	))
	.await
}

#[admin_command]
pub(super) async fn federation_request(
	&self,
	server_name: OwnedServerName,
	url_path: String,
	output: Option<String>,
) -> Result {
	use conduwuit::info;

	// This command can write arbitrary files via the `output` parameter,
	// so it must remain restricted to the server console.
	self.bail_restricted()?;

	// Parse the URL path to determine which federation endpoint to call
	// Currently supports: /_matrix/federation/v1/state/{roomId}
	if let Some(rest) = url_path.strip_prefix("/_matrix/federation/v1/state/") {
		let (room_id_str, event_id_str) = if let Some((room_part, query)) = rest.split_once('?') {
			let event_id = query.strip_prefix("event_id=").unwrap_or(query);
			(room_part, Some(event_id))
		} else {
			(rest, None)
		};

		let room_id: OwnedRoomId = room_id_str
			.parse()
			.map_err(|e| err!("Invalid room ID: {e:?}"))?;

		let event_id: OwnedEventId = event_id_str
			.ok_or_else(|| err!("event_id query parameter is required"))?
			.parse()
			.map_err(|e| err!("Invalid event ID: {e:?}"))?;

		info!("Fetching federation state for {room_id} at {event_id} from {server_name}");

		let response = self
			.services
			.sending
			.send_federation_request(&server_name, get_room_state::v1::Request {
				room_id: room_id.clone(),
				event_id: event_id.clone(),
			})
			.await?;

		let dump = serde_json::json!({
			"room_id": room_id,
			"server_name": server_name,
			"event_id": event_id.to_string(),
			"pdus": response.pdus,
			"auth_chain": response.auth_chain,
		});

		let pretty = serde_json::to_string_pretty(&dump).unwrap_or_default();

		if let Some(ref path) = output {
			std::fs::write(path, &pretty)
				.map_err(|e| err!("Failed to write output file: {e:?}"))?;
			self.write_str(&format!(
				"Saved {} state PDUs and {} auth chain events to {path}",
				response.pdus.len(),
				response.auth_chain.len()
			))
			.await
		} else {
			let truncated = pretty.get(..4096).unwrap_or(&pretty);
			self.write_str(&format!(
				"Received {} state PDUs and {} auth chain events\n\n{}",
				response.pdus.len(),
				response.auth_chain.len(),
				truncated
			))
			.await
		}
	} else {
		Err!(
			"Unsupported federation endpoint: {url_path}\n\nSupported endpoints:\n  \
			 /_matrix/federation/v1/state/!room:server?event_id=$event"
		)
	}
}

#[admin_command]
pub(super) async fn dag_merge_base(
	&self,
	room_id: OwnedRoomId,
	server: Option<OwnedServerName>,
	event_a: Option<OwnedEventId>,
	event_b: Option<OwnedEventId>,
	max_depth: usize,
	federate: bool,
) -> Result {
	// Server is required when event_b is not provided (need to probe remote tip)
	if event_b.is_none() && server.is_none() {
		return Err!("--server is required unless both --event-a and --event-b are provided");
	}

	if let Some(ref server) = server {
		if !self.services.server.config.allow_federation {
			return Err!("Federation is disabled on this homeserver.");
		}
		if *server == self.services.globals.server_name()
			&& !self.services.server.config.federation_loopback
		{
			return Err!(
				"Cannot compare against ourselves (enable federation_loopback to allow)."
			);
		}
	}

	let room_version = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await?;

	/// Look up a PDU from timeline first, then outlier table.
	macro_rules! get_pdu_any {
		($event_id:expr) => {{
			let eid: &EventId = $event_id;
			if let Ok(pdu) = self.services.rooms.timeline.get_pdu(eid).await {
				Some(pdu)
			} else if let Ok(pdu) = self.services.rooms.outlier.get_pdu_outlier(eid).await {
				Some(pdu)
			} else {
				None
			}
		}};
	}

	// Fetch a PDU from the remote server and store as outlier.
	macro_rules! fed_fetch {
		($event_id:expr) => {{
			let eid: OwnedEventId = $event_id;
			let result: Option<PduEvent> = async {
				let server = server.as_ref()?;
				let response = self
					.services
					.sending
					.send_federation_request(
						server,
						get_event::v1::Request::new(eid.clone(), None),
					)
					.await
					.ok()?;
				let (validated_id, value) = self
					.services
					.server_keys
					.validate_and_add_event_id(&response.pdu, &room_version)
					.await
					.ok()?;
				let pdu =
					PduEvent::from_id_val(&validated_id, value.clone(), Some(room_id.as_ref()))
						.ok()?;
				self.services.rooms.outlier.add_pdu_outlier(
					&validated_id,
					&value,
					Some(room_id.as_ref()),
				);
				Some(pdu)
			}
			.await;
			result
		}};
	}

	// Resolve local tip (event A)
	let event_a = match event_a {
		| Some(id) => id,
		| None => {
			let latest = self
				.services
				.rooms
				.timeline
				.latest_pdu_in_room(&room_id)
				.await?;
			self.write_str(&format!("Local tip: {}\n", latest.event_id()))
				.await?;
			latest.event_id().to_owned()
		},
	};

	// Resolve remote tip (event B) — only needed when --event-b is not provided.
	// The early guard ensures server.is_some() in this path.
	let event_b = match event_b {
		| Some(id) => id,
		| None => {
			let server = server.as_ref().expect("guarded above");
			self.write_str(&format!("Probing {server} for remote tip via make_join...\n"))
				.await?;

			let user_id = self
				.services
				.rooms
				.state_cache
				.active_local_users_in_room(&room_id)
				.boxed()
				.next()
				.await
				.ok_or_else(|| err!("No active local users in room {room_id}"))?
				.to_owned();

			let make_join_request =
				ruma::api::federation::membership::prepare_join_event::v1::Request {
					room_id: room_id.clone(),
					user_id,
					ver: self.services.server.supported_room_versions().collect(),
				};

			let response = self
				.services
				.sending
				.send_federation_request(server, make_join_request)
				.await
				.map_err(|e| err!("make_join to {server} failed: {e}"))?;

			let event_stub_raw = response.event;

			let event_stub: CanonicalJsonObject = serde_json::from_str(event_stub_raw.get())
				.map_err(|e| err!("Invalid make_join template from {server}: {e}"))?;

			let remote_tips: Vec<OwnedEventId> = event_stub
				.get("prev_events")
				.and_then(|v| v.as_array())
				.map(|arr| {
					arr.iter()
						.filter_map(|v| {
							v.as_str()
								.and_then(|s| <&EventId>::try_from(s).ok().map(ToOwned::to_owned))
						})
						.collect()
				})
				.unwrap_or_default();

			if remote_tips.is_empty() {
				return Err!(
					"make_join from {server} returned no prev_events (forward extremities)"
				);
			}

			let remote_tip_id = remote_tips.into_iter().next().expect("checked non-empty");
			self.write_str(&format!("Remote tip (via make_join): {remote_tip_id}\n"))
				.await?;
			remote_tip_id
		},
	};

	let pdu_a = match get_pdu_any!(&event_a) {
		| Some(pdu) => pdu,
		| None => fed_fetch!(event_a.clone())
			.ok_or_else(|| err!("Event A not found locally or via federation: {event_a}"))?,
	};
	let pdu_b = match get_pdu_any!(&event_b) {
		| Some(pdu) => pdu,
		| None => fed_fetch!(event_b.clone())
			.ok_or_else(|| err!("Event B not found locally or via federation: {event_b}"))?,
	};

	self.write_str(&format!(
		"Walking DAG backwards from:\n  A (local):  {event_a} (ts {ta_ts}, type {ta})\n  B \
		 (remote): {event_b} (ts {tb_ts}, type {tb})\n\nMax depth: {max_depth}\n",
		ta_ts = pdu_a.origin_server_ts,
		ta = pdu_a.kind,
		tb_ts = pdu_b.origin_server_ts,
		tb = pdu_b.kind,
	))
	.await?;

	// Bidirectional BFS via prev_events
	// ancestors_a/b: event_id -> (depth_from_start, parent_event_id)
	let mut ancestors_a: HashMap<OwnedEventId, (usize, Option<OwnedEventId>)> = HashMap::new();
	let mut ancestors_b: HashMap<OwnedEventId, (usize, Option<OwnedEventId>)> = HashMap::new();
	let mut queue_a: VecDeque<(OwnedEventId, usize)> = VecDeque::new();
	let mut queue_b: VecDeque<(OwnedEventId, usize)> = VecDeque::new();

	ancestors_a.insert(event_a.clone(), (0, None));
	ancestors_b.insert(event_b.clone(), (0, None));
	queue_a.push_back((event_a.clone(), 0));
	queue_b.push_back((event_b.clone(), 0));

	let mut merge_bases: Vec<OwnedEventId> = Vec::new();
	let mut steps = 0_usize;
	let mut missing_events = 0_usize;
	let mut fetched_events = 0_usize;

	while (!queue_a.is_empty() || !queue_b.is_empty()) && steps < max_depth {
		// Expand one step from A
		if let Some((current, depth)) = queue_a.pop_front() {
			if ancestors_b.contains_key(&current) {
				if !merge_bases.contains(&current) {
					merge_bases.push(current.clone());
				}
				// Don't stop — find all merge bases at this depth
			}

			let pdu = match get_pdu_any!(&current) {
				| Some(p) => Some(p),
				| None if federate => {
					fetched_events = fetched_events.saturating_add(1);
					let srv = server.as_deref().map_or("local", |s| s.as_str());
					info!(
						"dag-merge-base: fetching {current} from {srv} (A-side, \
						 #{fetched_events})"
					);
					fed_fetch!(current.clone())
				},
				| None => None,
			};
			if let Some(pdu) = pdu {
				for prev in pdu.prev_events() {
					let prev = prev.to_owned();
					if !ancestors_a.contains_key(&prev) {
						let next_depth = depth.saturating_add(1);
						ancestors_a.insert(prev.clone(), (next_depth, Some(current.clone())));
						if next_depth < max_depth {
							queue_a.push_back((prev, next_depth));
						}
					}
				}
			} else {
				missing_events = missing_events.saturating_add(1);
			}
		}

		// Expand one step from B
		if let Some((current, depth)) = queue_b.pop_front() {
			if ancestors_a.contains_key(&current) {
				if !merge_bases.contains(&current) {
					merge_bases.push(current.clone());
				}
			}

			let pdu = match get_pdu_any!(&current) {
				| Some(p) => Some(p),
				| None if federate => {
					fetched_events = fetched_events.saturating_add(1);
					let srv = server.as_deref().map_or("local", |s| s.as_str());
					info!(
						"dag-merge-base: fetching {current} from {srv} (B-side, \
						 #{fetched_events})"
					);
					fed_fetch!(current.clone())
				},
				| None => None,
			};
			if let Some(pdu) = pdu {
				for prev in pdu.prev_events() {
					let prev = prev.to_owned();
					if !ancestors_b.contains_key(&prev) {
						let next_depth = depth.saturating_add(1);
						ancestors_b.insert(prev.clone(), (next_depth, Some(current.clone())));
						if next_depth < max_depth {
							queue_b.push_back((prev, next_depth));
						}
					}
				}
			} else {
				missing_events = missing_events.saturating_add(1);
			}
		}

		steps = steps.saturating_add(1);

		// If we found merge bases and both queues are past the merge base depth, stop
		if !merge_bases.is_empty() && queue_a.is_empty() && queue_b.is_empty() {
			break;
		}
		// Early stop if we found merge bases and current depth exceeds merge base depth
		// by a margin
		if let Some(first_mb) = merge_bases.first() {
			let mb_depth_a = ancestors_a.get(first_mb).map_or(0, |(d, _)| *d);
			let mb_depth_b = ancestors_b.get(first_mb).map_or(0, |(d, _)| *d);
			let mb_max = mb_depth_a.max(mb_depth_b);
			let current_min_a = queue_a.front().map_or(usize::MAX, |(_, d)| *d);
			let current_min_b = queue_b.front().map_or(usize::MAX, |(_, d)| *d);
			if current_min_a > mb_max && current_min_b > mb_max {
				break;
			}
		}
	}

	self.write_str(&format!(
		"Walked {} steps, visited {} (A) + {} (B) events, {} missing, {} fetched\n",
		steps,
		ancestors_a.len(),
		ancestors_b.len(),
		missing_events,
		fetched_events,
	))
	.await?;

	if merge_bases.is_empty() {
		return self
			.write_str(&format!(
				"**No common ancestor found** within {max_depth} steps.\n\nThe events may be on \
				 completely disjoint DAG branches, or the common ancestor is deeper than the \
				 limit."
			))
			.await;
	}

	// For each merge base, trace back the path from both events
	for mb in &merge_bases {
		let mb_pdu = get_pdu_any!(mb);
		let mb_info = mb_pdu.as_ref().map_or_else(
			|| "unknown".to_owned(),
			|p| format!("ts {}, type {}", p.origin_server_ts, p.kind),
		);

		// BFS distances from each starting event
		let dist_a = ancestors_a
			.get(mb)
			.map_or_else(|| "?".to_owned(), |(d, _)| d.to_string());
		let dist_b = ancestors_b
			.get(mb)
			.map_or_else(|| "?".to_owned(), |(d, _)| d.to_string());

		self.write_str(&format!(
			"\n### Merge base: `{mb}` ({mb_info})\nA is {dist_a} step(s) away, B is {dist_b} \
			 step(s) away\n"
		))
		.await?;

		// Trace path A -> merge base
		let path_a = trace_path(&ancestors_a, &event_a, mb);
		let path_b = trace_path(&ancestors_b, &event_b, mb);

		// Render ASCII DAG
		let short = |id: &EventId| -> String {
			let s = id.as_str();
			let truncated: String = s.chars().take(12).collect();
			if s.len() > 12 {
				format!("{truncated}…")
			} else {
				s.to_owned()
			}
		};

		let mut graph = String::new();

		// Header
		writeln!(graph, "```").ok();
		writeln!(
			graph,
			"  A ({} steps)          B ({} steps)",
			path_a.len().saturating_sub(1),
			path_b.len().saturating_sub(1)
		)
		.ok();
		writeln!(graph, "  │                     │").ok();

		let max_len = path_a.len().max(path_b.len());
		for i in 0..max_len {
			let left = path_a.get(i).map(|id| short(id)).unwrap_or_default();
			let right = path_b.get(i).map(|id| short(id)).unwrap_or_default();

			// Check if this is the merge base
			let is_mb_left = path_a.get(i).is_some_and(|id| id == mb);
			let is_mb_right = path_b.get(i).is_some_and(|id| id == mb);

			if is_mb_left || is_mb_right {
				writeln!(graph, "  └──────────┬──────────┘").ok();
				writeln!(graph, "             │").ok();
				writeln!(graph, "      {left} ◄── MERGE BASE").ok();

				// Get the merge base PDU info
				if let Some(ref p) = mb_pdu {
					writeln!(graph, "      ts={}", p.origin_server_ts).ok();
				}
				break;
			}

			if !left.is_empty() && !right.is_empty() {
				writeln!(graph, "  {left:<20}  {right}").ok();
				writeln!(graph, "  │                     │").ok();
			} else if !left.is_empty() {
				writeln!(graph, "  {left:<20}  ·").ok();
				writeln!(graph, "  │                     ·").ok();
			} else {
				writeln!(graph, "  ·                     {right}").ok();
				writeln!(graph, "  ·                     │").ok();
			}
		}

		// If we never printed a merge base line (both paths end at merge base on
		// different iterations)
		if max_len == 0 {
			writeln!(graph, "  (same event)").ok();
		}

		writeln!(graph, "```").ok();
		self.write_str(&graph).await?;
	}

	Ok(())
}

/// Trace the path from a starting event back to the target event using the
/// ancestor map.
fn trace_path(
	ancestors: &HashMap<OwnedEventId, (usize, Option<OwnedEventId>)>,
	from: &EventId,
	to: &EventId,
) -> Vec<OwnedEventId> {
	let mut path = Vec::new();
	let mut current = from.to_owned();
	let mut visited = HashSet::new();

	loop {
		if !visited.insert(current.clone()) {
			break; // cycle guard
		}
		path.push(current.clone());
		if current == to {
			break;
		}
		match ancestors.get(&current) {
			| Some((_, Some(parent))) => current = parent.clone(),
			| _ => break,
		}
	}

	path
}

/// Format an origin_server_ts (millis since epoch) as a human-readable UTC
/// string.
fn format_ts(ts_millis: u64) -> String {
	let ts_secs = ts_millis.checked_div(1000).unwrap_or(0);
	let days = ts_secs.checked_div(86_400).unwrap_or(0);
	let time_of_day = ts_secs.checked_rem(86_400).unwrap_or(0);
	let hours = time_of_day.checked_div(3600).unwrap_or(0);
	let minutes = time_of_day
		.checked_rem(3600)
		.unwrap_or(0)
		.checked_div(60)
		.unwrap_or(0);
	let seconds = time_of_day.checked_rem(60).unwrap_or(0);
	let (year, month, day) = civil_from_days(days.cast_signed());
	format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02} UTC")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Based on Howard Hinnant's civil_from_days algorithm.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
	let z = days.saturating_add(719_468);
	let era = if z >= 0 { z } else { z.saturating_sub(146_096) }.saturating_div(146_097);
	let doe = z
		.saturating_sub(era.saturating_mul(146_097))
		.cast_unsigned();
	let yoe = doe
		.saturating_sub(doe.saturating_div(1460))
		.saturating_add(doe.saturating_div(36_524))
		.saturating_sub(doe.saturating_div(146_096))
		.saturating_div(365);
	let y = yoe.cast_signed().saturating_add(era.saturating_mul(400));
	let doy = doe.saturating_sub(
		yoe.saturating_mul(365)
			.saturating_add(yoe.saturating_div(4))
			.saturating_sub(yoe.saturating_div(100)),
	);
	let mp = doy.saturating_mul(5).saturating_add(2).saturating_div(153);
	let d = doy
		.saturating_sub(mp.saturating_mul(153).saturating_add(2).saturating_div(5))
		.saturating_add(1);
	let m = if mp < 10 {
		mp.saturating_add(3)
	} else {
		mp.saturating_sub(9)
	};
	let y = if m <= 2 { y.saturating_add(1) } else { y };
	(y, m, d)
}

#[admin_command]
pub(super) async fn recalculate_extremities(
	&self,
	room: OwnedRoomOrAliasId,
	tail: i64,
) -> Result {
	let room_id = self.services.rooms.alias.resolve(&room).await?;

	let actual_tail = if tail < 0 {
		usize::MAX
	} else {
		usize::try_from(tail).unwrap_or(usize::MAX)
	};

	let tail_str = if tail < 0 {
		"all".to_owned()
	} else {
		actual_tail.to_string()
	};

	self.write_str(&format!(
		"Recalculating forward extremities for room {room_id} using tail {tail_str}...\n"
	))
	.await?;

	let changed = self
		.services
		.rooms
		.timeline
		.recalculate_extremities(&room_id, actual_tail, true)
		.await?;

	if changed {
		self.write_str(
			"SUCCESS: DAG Extremities were silently broken and have now been recalculated and \
			 permanently healed!\n",
		)
		.await?;
	} else {
		self.write_str("DAG Extremities are already mathematically perfect. No changes made.\n")
			.await?;
	}

	Ok(())
}

#[admin_command]
pub(super) async fn check_rooms(&self, problems_only: bool, fix: bool) -> Result {
	let ours = self.services.globals.server_name();

	self.write_str("Scanning all rooms...\n").await?;

	let mut total_rooms = 0_usize;
	let mut problem_rooms = 0_usize;
	let mut fixed_rooms = 0_usize;
	let mut output = String::new();

	let room_ids: Vec<_> = self
		.services
		.rooms
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.collect()
		.await;

	for room_id in &room_ids {
		total_rooms = total_rooms.saturating_add(1);
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

		// Flush every 50 rooms to avoid building huge strings
		if total_rooms.is_multiple_of(50) {
			self.write_str(&output).await?;
			output.clear();
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

#[admin_command]
pub(super) async fn manage_rejected(
	&self,
	event_ids: Vec<OwnedEventId>,
	unreject: bool,
	soft_fail: bool,
) -> Result {
	let mut changed = 0_usize;
	let mut already = 0_usize;

	for event_id in &event_ids {
		let is_rejected = self
			.services
			.rooms
			.pdu_metadata
			.is_event_rejected(event_id)
			.await;
		let is_soft_failed = self
			.services
			.rooms
			.pdu_metadata
			.is_event_soft_failed(event_id)
			.await;

		if unreject {
			if is_rejected {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_rejected(event_id);
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_admin_rejected(event_id);
				changed = changed.saturating_add(1);
			} else {
				already = already.saturating_add(1);
			}
			if soft_fail && is_soft_failed {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_soft_failed(event_id);
			}
		} else {
			if !is_rejected {
				self.services
					.rooms
					.pdu_metadata
					.mark_event_admin_rejected(event_id);
				changed = changed.saturating_add(1);
			} else {
				already = already.saturating_add(1);
			}
			if soft_fail && !is_soft_failed {
				self.services
					.rooms
					.pdu_metadata
					.mark_event_soft_failed(event_id);
			}
		}
	}

	let action = if unreject { "unrejected" } else { "marked rejected" };
	let already_desc = if unreject {
		"already not rejected"
	} else {
		"already rejected"
	};
	let sf_note = if soft_fail { " (+ soft-fail marker)" } else { "" };
	self.write_str(&format!(
		"{changed} event(s) {action}{sf_note} ({already} {already_desc}, {} total)\n",
		event_ids.len()
	))
	.await
}

#[admin_command]
pub(super) async fn unreject_room(
	&self,
	room_id: OwnedRoomId,
	dry_run: bool,
	soft_fail: bool,
) -> Result {
	self.bail_restricted()?;

	let mut unmarked = 0_usize;
	let mut soft_unmarked = 0_usize;
	let mut total = 0_usize;

	// Collect all event IDs from timeline + outlier tree
	let mut pdu_ids: HashSet<OwnedEventId> = self
		.services
		.rooms
		.timeline
		.all_pdus(&room_id)
		.map(|(_, pdu)| pdu.event_id().to_owned())
		.collect()
		.await;

	let outlier_count_before = pdu_ids.len();

	let outliers: Vec<OwnedEventId> = self
		.services
		.rooms
		.outlier
		.room_stream(&room_id)
		.map(|(event_id, _)| event_id)
		.collect()
		.await;

	pdu_ids.extend(outliers);

	self.write_str(&format!(
		"Scanning {} events ({} timeline, {} outliers)...\n",
		pdu_ids.len(),
		outlier_count_before,
		pdu_ids.len().saturating_sub(outlier_count_before),
	))
	.await?;

	for event_id in &pdu_ids {
		if self
			.services
			.rooms
			.pdu_metadata
			.is_event_rejected(event_id)
			.await
		{
			total = total.saturating_add(1);
			if !dry_run {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_rejected(event_id);
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_admin_rejected(event_id);
				unmarked = unmarked.saturating_add(1);
			}
		}
		if soft_fail
			&& self
				.services
				.rooms
				.pdu_metadata
				.is_event_soft_failed(event_id)
				.await
		{
			if !dry_run {
				self.services
					.rooms
					.pdu_metadata
					.unmark_event_soft_failed(event_id);
				soft_unmarked = soft_unmarked.saturating_add(1);
			}
		}
	}

	if dry_run {
		self.write_str(&format!(
			"Dry run: Found {total} rejected events in {room_id} to unmark.\n"
		))
		.await
	} else {
		let soft_msg = if soft_fail {
			format!(", {soft_unmarked} soft-fail markers cleared")
		} else {
			String::new()
		};
		self.write_str(&format!("Unmarked {unmarked} rejected events{soft_msg} in {room_id}.\n"))
			.await
	}
}

#[admin_command]
pub(super) async fn heal_all_rooms(
	&self,
	server: OwnedServerName,
	dry_run: bool,
	limit: usize,
) -> Result {
	use conduwuit::matrix::event::gen_event_id_canonical_json;

	let ours = self.services.globals.server_name();

	// Collect all rooms we participate in
	let rooms: Vec<OwnedRoomId> = self
		.services
		.rooms
		.state_cache
		.server_rooms(ours)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let total = if limit > 0 { limit.min(rooms.len()) } else { rooms.len() };
	self.write_str(&format!(
		"Healing {total}/{} rooms against {server} {}...\n\n",
		rooms.len(),
		if dry_run { "(DRY RUN)" } else { "" }
	))
	.await?;

	let mut healed = 0_usize;
	let mut skipped = 0_usize;
	let mut errors = 0_usize;
	let mut total_rejected = 0_usize;

	for (i, room_id) in rooms.iter().take(total).enumerate() {
		let Ok(room_version) = self
			.services
			.rooms
			.state
			.get_room_version_or_fallback(room_id)
			.await
		else {
			skipped = skipped.saturating_add(1);
			continue;
		};

		// Get our latest event
		let Ok(latest_pdu) = self
			.services
			.rooms
			.timeline
			.latest_pdu_in_room(room_id)
			.await
		else {
			skipped = skipped.saturating_add(1);
			continue;
		};
		let at_event_id = latest_pdu.event_id().to_owned();

		// Fetch remote state
		let Ok(response) = self
			.services
			.sending
			.send_federation_request(&server, get_room_state::v1::Request {
				room_id: room_id.clone(),
				event_id: at_event_id.clone(),
			})
			.await
		else {
			skipped = skipped.saturating_add(1);
			continue;
		};

		// Build remote state map
		let mut remote_state: HashMap<(String, String), OwnedEventId> = HashMap::new();
		for pdu_raw in &response.pdus {
			let Ok((event_id, _value)) = gen_event_id_canonical_json(pdu_raw, &room_version)
			else {
				continue;
			};

			let pdu: PduEvent = match serde_json::from_str(pdu_raw.get()) {
				| Ok(p) => p,
				| Err(_) => continue,
			};

			if let Some(state_key) = &pdu.state_key {
				remote_state.insert((pdu.kind.to_string(), state_key.to_string()), event_id);
			}
		}

		if remote_state.is_empty() {
			skipped = skipped.saturating_add(1);
			continue;
		}

		// Build local state map
		let Ok(local_state_hash) = self
			.services
			.rooms
			.state
			.get_room_shortstatehash(room_id)
			.await
		else {
			skipped = skipped.saturating_add(1);
			continue;
		};

		let mut local_state: HashMap<(String, String), OwnedEventId> = HashMap::new();
		{
			let state_full = self
				.services
				.rooms
				.state_accessor
				.state_full(local_state_hash);
			pin_mut!(state_full);
			while let Some(((event_type, state_key), pdu)) = state_full.next().await {
				local_state.insert(
					(event_type.to_string(), state_key.to_string()),
					pdu.event_id().to_owned(),
				);
			}
		}

		// Find extra local events (in local but not remote, or different event ID)
		let remote_eids: HashSet<OwnedEventId> = remote_state.values().cloned().collect();
		let local_eids: HashSet<OwnedEventId> = local_state.values().cloned().collect();
		let extra: Vec<OwnedEventId> = local_eids.difference(&remote_eids).cloned().collect();

		if extra.is_empty() {
			// Room is healthy
			continue;
		}

		let n_extra = extra.len();
		self.write_str(&format!(
			"[{}/{}] {} — {n_extra} extra events",
			i.saturating_add(1),
			total,
			room_id,
		))
		.await?;

		if dry_run {
			self.write_str(" (would reject + force-set)\n").await?;
			healed = healed.saturating_add(1);
			total_rejected = total_rejected.saturating_add(n_extra);
			continue;
		}

		// Mark extra events as rejected
		for eid in &extra {
			if !self
				.services
				.rooms
				.pdu_metadata
				.is_event_rejected(eid)
				.await
			{
				self.services.rooms.pdu_metadata.mark_event_rejected(eid);
			}
		}
		total_rejected = total_rejected.saturating_add(n_extra);

		// Force-set state from backbone
		match Box::pin(self.force_set_state(
			room_id.clone(),
			vec![server.clone()],
			None,
			true,  // overwrite
			true,  // skip_sig_verify
			true,  // absolute (direct override, no state-res merge)
			None,  // output
			None,  // input
			false, // dry_run
			false, // skip_membership_rebuild (safe-ish: rebuild & silent calls)
		))
		.await
		{
			| Ok(()) => {
				self.write_str(&format!(" ✓ rejected {n_extra}, state reset\n"))
					.await?;
				healed = healed.saturating_add(1);
			},
			| Err(e) => {
				self.write_str(&format!(" ✗ force-set failed: {e}\n"))
					.await?;
				errors = errors.saturating_add(1);
			},
		}
	}

	self.write_str(&format!(
		"\n**Summary**: {healed} healed, {skipped} skipped, {errors} errors, {total_rejected} \
		 events rejected\n"
	))
	.await
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
pub(super) async fn rebuild_membership_cache(&self, room_id: OwnedRoomId) -> Result {
	use conduwuit::info;
	use ruma::events::StateEventType;

	let short_state_hash = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	info!("Rebuilding membership cache from state snapshot {short_state_hash} for {room_id}");

	let mut state_joined: HashSet<OwnedUserId> = HashSet::new();
	let mut state_invited: HashSet<OwnedUserId> = HashSet::new();
	let mut members_updated = 0_usize;

	// Collect membership data into a Vec FIRST to drop the zero-copy
	// RocksDB iterator before the write phase. Holding an iterator
	// across .await points risks SEGV if compaction invalidates the
	// underlying memory.
	let members: Vec<(OwnedUserId, String)> = self
		.services
		.rooms
		.state_accessor
		.state_full(short_state_hash)
		.filter_map(|((event_type, state_key), pdu)| async move {
			if event_type != StateEventType::RoomMember {
				return None;
			}
			let user_id = OwnedUserId::try_from(state_key.as_str()).ok()?;
			let content = pdu.get_content_as_value();
			let membership = content
				.get("membership")
				.and_then(|v| v.as_str())
				.unwrap_or("leave")
				.to_owned();
			Some((user_id, membership))
		})
		.collect()
		.await;

	for (user_id, membership) in &members {
		match membership.as_str() {
			| "join" => {
				state_joined.insert(user_id.clone());
				if !self
					.services
					.rooms
					.state_cache
					.is_joined(user_id, &room_id)
					.await
				{
					self.services
						.rooms
						.state_cache
						.mark_as_joined_silent(user_id, &room_id)
						.await;
					members_updated = members_updated.saturating_add(1);
				}
			},
			| "invite" => {
				state_invited.insert(user_id.clone());
			},
			| "leave" | "ban" => {
				if self
					.services
					.rooms
					.state_cache
					.is_invited_or_joined(user_id, &room_id)
					.await
				{
					self.services
						.rooms
						.state_cache
						.mark_as_left_silent(user_id, &room_id)
						.await;
					members_updated = members_updated.saturating_add(1);
				}
			},
			| _ => {},
		}
	}

	// Sweep stale joined cache entries
	let cached_members: Vec<OwnedUserId> = self
		.services
		.rooms
		.state_cache
		.room_members(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut stale_removed = 0_usize;
	for user_id in &cached_members {
		if !state_joined.contains(user_id) && !state_invited.contains(user_id) {
			self.services
				.rooms
				.state_cache
				.mark_as_left_silent(user_id, &room_id)
				.await;
			stale_removed = stale_removed.saturating_add(1);
		}
	}

	// Sweep stale invited cache entries
	let cached_invited: Vec<OwnedUserId> = self
		.services
		.rooms
		.state_cache
		.room_members_invited(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	for user_id in &cached_invited {
		if !state_invited.contains(user_id) && !state_joined.contains(user_id) {
			self.services
				.rooms
				.state_cache
				.mark_as_left_silent(user_id, &room_id)
				.await;
			stale_removed = stale_removed.saturating_add(1);
		}
	}

	self.services
		.rooms
		.state_cache
		.update_joined_count(&room_id)
		.await;

	let out = format!(
		"Rebuilt membership cache for {room_id}: updated {members_updated}, removed \
		 {stale_removed} stale entries"
	);
	info!("{out}");
	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn set_state_event(
	&self,
	room_id: OwnedRoomId,
	event_type: String,
	state_key: String,
	event_id: OwnedEventId,
) -> Result {
	use conduwuit_service::rooms::state_compressor::CompressedState;

	self.bail_restricted()?;

	let event_type: StateEventType = event_type.into();

	// Verify the event exists locally (timeline or outlier)
	let pdu = match self.services.rooms.timeline.get_pdu(&event_id).await {
		| Ok(pdu) => pdu,
		| Err(_) => {
			// Try outlier: get the JSON and parse it
			let json = self
				.services
				.rooms
				.outlier
				.get_outlier_pdu_json(&event_id)
				.await
				.map_err(|_| err!(Request(NotFound("Event {event_id} not found locally"))))?;
			serde_json::from_value::<PduEvent>(
				serde_json::to_value(&json).map_err(|e| err!(Request(InvalidParam("{e}"))))?,
			)
			.map_err(|e| err!(Request(InvalidParam("Failed to parse outlier: {e}"))))?
		},
	};

	// Verify it matches the claimed type/state_key
	if pdu.kind.to_string() != event_type.to_string() {
		return Err!(Request(InvalidParam(
			"Event type mismatch: expected {event_type}, got {}",
			pdu.kind
		)));
	}
	if pdu.state_key.as_deref() != Some(&*state_key) {
		return Err!(Request(InvalidParam(
			"State key mismatch: expected {state_key:?}, got {:?}",
			pdu.state_key
		)));
	}

	let state_lock = self.services.rooms.state.mutex.lock(&room_id).await;

	// Get current state
	let current_shortstatehash = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await
		.map_err(|_| err!(Request(NotFound("Room has no state"))))?;

	// Get the full state as (shortstatekey, event_id) pairs
	let current_state: HashMap<u64, OwnedEventId> = self
		.services
		.rooms
		.state_accessor
		.state_full_ids(current_shortstatehash)
		.collect()
		.await;

	// Build new compressed state
	let target_shortstatekey = self
		.services
		.rooms
		.short
		.get_or_create_shortstatekey(&event_type, &state_key)
		.await;

	let mut new_state = CompressedState::new();

	for (shortstatekey, eid) in &current_state {
		if *shortstatekey == target_shortstatekey {
			// Replace with our target event
			let compressed = self
				.services
				.rooms
				.state_compressor
				.compress_state_event(*shortstatekey, &event_id)
				.await;
			new_state.insert(compressed);
		} else {
			let compressed = self
				.services
				.rooms
				.state_compressor
				.compress_state_event(*shortstatekey, eid)
				.await;
			new_state.insert(compressed);
		}
	}

	// If the (type, state_key) wasn't in the current state, add it
	if !current_state.contains_key(&target_shortstatekey) {
		let compressed = self
			.services
			.rooms
			.state_compressor
			.compress_state_event(target_shortstatekey, &event_id)
			.await;
		new_state.insert(compressed);
	}

	// Save the new state
	let new_state = std::sync::Arc::new(new_state);
	let new_shortstatehash = self
		.services
		.rooms
		.state
		.set_event_state(&event_id, &room_id, new_state)
		.await?;

	self.services
		.rooms
		.state
		.set_room_state(&room_id, new_shortstatehash, &state_lock);

	// Rebuild membership cache if this is a member event
	if event_type == StateEventType::RoomMember {
		if let Ok(user_id) = ruma::UserId::parse(&state_key) {
			self.services
				.rooms
				.state_cache
				.update_membership(&room_id, user_id, &pdu, false)
				.await?;
		}
		self.services
			.rooms
			.state_cache
			.update_joined_count(&room_id)
			.await;
	}

	let membership = if event_type == StateEventType::RoomMember {
		pdu.content
			.get()
			.parse::<serde_json::Value>()
			.ok()
			.and_then(|c: serde_json::Value| {
				c.get("membership")
					.and_then(|m| m.as_str().map(String::from))
			})
			.unwrap_or_default()
	} else {
		String::new()
	};

	let out = format!(
		"Set ({event_type}, {state_key:?}) => {event_id}{}\n",
		if membership.is_empty() {
			String::new()
		} else {
			format!(" (membership: {membership})")
		}
	);
	info!("{out}");
	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn fetch_missing_events(
	&self,
	room_id: OwnedRoomId,
	event_ids: Vec<OwnedEventId>,
	rounds: usize,
) -> Result {
	use std::time::Instant;

	use futures::{StreamExt, stream::FuturesUnordered};

	let room_version = self
		.services
		.rooms
		.state
		.get_room_version_or_fallback(&room_id)
		.await?;

	// Build EMA-sorted server list
	let servers = self
		.services
		.rooms
		.event_handler
		.build_federation_server_list(
			&room_id,
			self.services.globals.server_name(),
			self.services.server.config.federation_fallback_room_servers,
		)
		.await;

	// Use current forward extremities as the "earliest" boundary
	let extremities: Vec<OwnedEventId> = self
		.services
		.rooms
		.state
		.get_forward_extremities(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	// If no explicit event IDs given, use extremities as the gap target too
	let targets: Vec<OwnedEventId> = if event_ids.is_empty() {
		extremities.clone()
	} else {
		event_ids
	};

	self.write_str(&format!(
		"fetch-missing-events: room={room_id} servers={} extremities={} targets={} rounds={}\n",
		servers.len(),
		extremities.len(),
		targets.len(),
		rounds,
	))
	.await?;

	let mut total_filled: usize = 0;

	for round in 1..=rounds {
		// Filter to IDs still missing locally
		let remaining: Vec<OwnedEventId> = {
			let mut r = Vec::new();
			for id in &targets {
				if !self.services.rooms.timeline.pdu_exists(id).await
					&& self
						.services
						.rooms
						.outlier
						.get_pdu_outlier(id)
						.await
						.is_err()
				{
					r.push(id.clone());
				}
			}
			r
		};

		if remaining.is_empty() {
			self.write_str(&format!("Round {round}: all targets present locally, done.\n"))
				.await?;
			break;
		}

		self.write_str(&format!(
			"Round {round}/{rounds}: {} targets still missing, fanning out to {} servers...\n",
			remaining.len(),
			servers.len(),
		))
		.await?;

		// Fan out POST /get_missing_events to all servers in parallel
		let mut active: FuturesUnordered<_> = FuturesUnordered::new();
		for server in &servers {
			let room_id_c = room_id.clone();
			let earliest = extremities.clone();
			let latest = remaining.clone();
			active.push(async move {
				let t = Instant::now();
				let res = self
					.services
					.sending
					.send_federation_request(server, get_missing_events::v1::Request {
						room_id: room_id_c,
						earliest_events: earliest,
						latest_events: latest,
						limit: 100_u32.into(),
						min_depth: 0_u32.into(),
					})
					.await;
				self.services.rooms.event_handler.update_peer_stats(
					server,
					res.is_ok(),
					t.elapsed(),
				);
				res.map(|r| r.events)
			});
		}

		let mut round_filled: usize = 0;
		while let Some(result) = active.next().await {
			if let Ok(events) = result {
				for raw in events {
					if let Ok((event_id, value)) = self
						.services
						.server_keys
						.validate_and_add_event_id(raw.as_ref(), &room_version)
						.await
					{
						self.services.rooms.outlier.add_pdu_outlier(
							&event_id,
							&value,
							Some(&room_id),
						);
						round_filled = round_filled.saturating_add(1);
					}
				}
			}
		}

		total_filled = total_filled.saturating_add(round_filled);
		self.write_str(&format!("Round {round}: filled {round_filled} events.\n"))
			.await?;

		if round_filled == 0 {
			self.write_str("No new events found this round, stopping early.\n")
				.await?;
			break;
		}
	}

	self.write_str(&format!("Done. Total events stored as outliers: {total_filled}\n"))
		.await
}

#[admin_command]
pub(super) async fn clear_ratelimiter(&self) -> Result {
	self.bail_restricted()?;
	self.services.globals.bad_event_ratelimiter.write().clear();
	self.write_str("Cleared the global bad_event ratelimiter cache.")
		.await
}
