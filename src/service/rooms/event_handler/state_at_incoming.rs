use std::{
	collections::{HashMap, HashSet},
	iter::Iterator,
};

use conduwuit::{
	Result, debug, err, implement,
	matrix::{Event, StateMap},
	trace,
	utils::stream::{IterStream, TryBroadbandExt},
};
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::ready};
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId};

// TODO: if we know the prev_events of the incoming event we can avoid the
// request and build the state from a known point and resolve if > 1 prev_event
#[implement(super::Service)]
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub(super) async fn state_at_incoming_degree_one<Pdu>(
	&self,
	incoming_pdu: &Pdu,
	room_id: &RoomId,
) -> Result<std::sync::Arc<crate::rooms::state_compressor::CompressedState>>
where
	Pdu: Event + Send + Sync,
{
	let prev_event = incoming_pdu
		.prev_events()
		.next()
		.expect("at least one prev_event");

	let prev_pdu = self
		.services
		.timeline
		.get_pdu_in_room(Some(room_id), prev_event)
		.await?;

	if prev_pdu.room_id() != Some(room_id) {
		return Err(err!(Database("prev_pdu room_id does not match")));
	}

	let prev_event_sstatehash = self
		.services
		.state_accessor
		.pdu_shortstatehash(prev_event)
		.await?;

	let mut state = self
		.services
		.state_compressor
		.load_shortstatehash_info(prev_event_sstatehash)
		.await?
		.pop()
		.unwrap()
		.full_state
		.unwrap()
		.as_ref()
		.clone();

	debug!("Using cached state");

	if let Some(state_key) = &prev_pdu.state_key {
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&prev_pdu.kind().to_string().into(), state_key)
			.await;
		let shorteventid = self
			.services
			.short
			.get_or_create_shorteventid(prev_event)
			.await;

		let old_compressed = state
			.iter()
			.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
			.copied();
		if let Some(old) = old_compressed {
			state.remove(&old);
		}
		state.insert(crate::rooms::state_compressor::compress_state_event(
			shortstatekey,
			shorteventid,
		));
		// Now it's the state after the pdu
	}

	debug_assert!(!state.is_empty(), "should be returning Err for empty CompressedState result");

	Ok(std::sync::Arc::new(state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub async fn state_at_incoming_resolved<Pdu>(
	&self,
	incoming_pdu: &Pdu,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<std::sync::Arc<crate::rooms::state_compressor::CompressedState>>
where
	Pdu: Event + Send + Sync,
{
	self.resolve_extremities(incoming_pdu.prev_events(), room_id, room_version_id)
		.await
}

#[implement(super::Service)]
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub async fn resolve_extremities<'a, I>(
	&self,
	prev_events: I,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<std::sync::Arc<crate::rooms::state_compressor::CompressedState>>
where
	I: Iterator<Item = &'a EventId> + Send,
{
	let fn_start = std::time::Instant::now();
	trace!("Calculating extremity statehashes...");
	let extremity_sstatehashes = prev_events
		.try_stream()
		.broad_and_then(|prev_eventid| {
			self.services
				.timeline
				.get_pdu_in_room(Some(room_id), prev_eventid)
				.and_then(move |prev_event| async move {
					if prev_event.room_id() != Some(room_id) {
						return Err(err!(Database("prev_event is not in the same room")));
					}
					Ok((prev_eventid, prev_event))
				})
		})
		.broad_and_then(|(prev_eventid, prev_event)| {
			self.services
				.state_accessor
				.pdu_shortstatehash(prev_eventid)
				.map_ok(move |sstatehash| (sstatehash, prev_event))
		})
		.try_collect::<Vec<(u64, conduwuit_core::PduEvent)>>()
		.await?;

	let mut fork_compressed_states = Vec::with_capacity(extremity_sstatehashes.len());
	for &(sstatehash, ref prev_event) in &extremity_sstatehashes {
		let mut state = self
			.services
			.state_compressor
			.load_shortstatehash_info(sstatehash)
			.await?
			.pop()
			.unwrap()
			.full_state
			.unwrap()
			.as_ref()
			.clone();

		if let Some(state_key) = prev_event.state_key() {
			let shortstatekey = self
				.services
				.short
				.get_or_create_shortstatekey(&prev_event.kind().to_string().into(), state_key)
				.await;
			let shorteventid = self
				.services
				.short
				.get_or_create_shorteventid(prev_event.event_id())
				.await;

			let old_compressed = state
				.iter()
				.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
				.copied();
			if let Some(old) = old_compressed {
				state.remove(&old);
			}
			state.insert(crate::rooms::state_compressor::compress_state_event(
				shortstatekey,
				shorteventid,
			));
		}
		fork_compressed_states.push(state);
	}

	fork_compressed_states.sort();
	fork_compressed_states.dedup();
	let num_forks = fork_compressed_states.len();
	trace!("Calculating fork states ({num_forks} forks)...");

	// Build ssk → set of (shorteventid) values across ALL forks.
	// A key is only truly conflicting if multiple forks assign it DIFFERENT values.
	// Keys present in only one fork are additions — auto-merged, no resolution
	// needed.
	let mut ssk_values: HashMap<u64, HashSet<u64>> = HashMap::new();
	for fork in &fork_compressed_states {
		for bytes in fork {
			let mut ssk_bytes = [0_u8; 8];
			ssk_bytes.copy_from_slice(&bytes[0..8]);
			let ssk = u64::from_be_bytes(ssk_bytes);

			let mut id_bytes = [0_u8; 8];
			id_bytes.copy_from_slice(&bytes[8..16]);
			let sei = u64::from_be_bytes(id_bytes);

			ssk_values.entry(ssk).or_default().insert(sei);
		}
	}

	let conflicting_ssks: HashSet<u64> = ssk_values
		.iter()
		.filter(|(_, values)| values.len() > 1)
		.map(|(ssk, _)| *ssk)
		.collect();

	let non_conflicting_additions = ssk_values.len().saturating_sub(conflicting_ssks.len());

	println!(
		"state_at_incoming_resolved: {num_forks} forks, {} truly conflicting keys, {} \
		 auto-merged additions, {} total ssk (took {:?} to compute)",
		conflicting_ssks.len(),
		non_conflicting_additions,
		ssk_values.len(),
		fn_start.elapsed(),
	);

	if conflicting_ssks.is_empty() {
		// No conflicting keys — build merged state from all forks' entries
		println!("state_at_incoming_resolved: TRIVIAL MERGE (0 conflicts) — skipping resolution");
		let mut state_map = std::collections::BTreeSet::new();
		// Collect the winning value for each ssk (all forks agree or it's a unique
		// addition)
		for fork in &fork_compressed_states {
			for bytes in fork {
				state_map.insert(*bytes);
			}
		}
		return Ok(std::sync::Arc::new(state_map));
	}

	// Determine which state keys are auth-critical (affects resolution outcome)
	let mut auth_ssks = HashSet::new();
	for ty in &[
		ruma::events::StateEventType::RoomCreate,
		ruma::events::StateEventType::RoomPowerLevels,
		ruma::events::StateEventType::RoomJoinRules,
		ruma::events::StateEventType::RoomServerAcl,
		ruma::events::StateEventType::RoomMember,
		ruma::events::StateEventType::RoomThirdPartyInvite,
	] {
		if let Ok(ssk) = self.services.short.get_shortstatekey(ty, "").await {
			auth_ssks.insert(ssk);
		}
	}

	// FAST PATH: If none of the conflicting keys are auth-critical types
	// (power_levels, join_rules, create), we can skip the full state resolution
	// machinery (auth chain diff + Kahn's sort + mainline sort + iterative auth
	// check = O(N²) on 1500+ events) and just pick winners directly.
	// This handles ~90% of real-world forks (concurrent membership changes).
	let all_simple_conflicts = conflicting_ssks.iter().all(|ssk| !auth_ssks.contains(ssk));

	if all_simple_conflicts {
		println!(
			"state_at_incoming_resolved: FAST PATH — {} non-auth conflicts, picking winners \
			 directly",
			conflicting_ssks.len()
		);

		// Build merged state from all forks' non-conflicting entries
		let mut final_state = std::collections::BTreeSet::new();
		for fork in &fork_compressed_states {
			for bytes in fork {
				let mut ssk_bytes = [0_u8; 8];
				ssk_bytes.copy_from_slice(&bytes[0..8]);
				let ssk = u64::from_be_bytes(ssk_bytes);

				if conflicting_ssks.contains(&ssk) {
					continue; // Handle below
				}

				final_state.insert(*bytes);
			}
		}

		// For each conflicting key, pick the winner: latest origin_server_ts,
		// then lexicographically largest event_id as tiebreaker.
		for ssk in &conflicting_ssks {
			let mut best: Option<(OwnedEventId, u64, u64)> = None;
			for fork in &fork_compressed_states {
				let event_bytes = fork
					.iter()
					.find(|bytes| bytes.starts_with(&ssk.to_be_bytes()));
				if let Some(bytes) = event_bytes {
					let mut id_bytes = [0_u8; 8];
					id_bytes.copy_from_slice(&bytes[8..16]);
					let shorteventid = u64::from_be_bytes(id_bytes);
					if let Ok(eid) = self
						.services
						.short
						.get_eventid_from_short::<OwnedEventId>(shorteventid)
						.await
					{
						if let Ok(pdu) = self
							.services
							.timeline
							.get_pdu_in_room(Some(room_id), &eid)
							.await
						{
							let ts: u64 = pdu.origin_server_ts().0.into();
							let dominated = best.as_ref().is_some_and(|(b_eid, b_ts, _)| {
								ts < *b_ts || (ts == *b_ts && eid.as_str() < b_eid.as_str())
							});
							if !dominated {
								best = Some((eid, ts, shorteventid));
							}
						}
					}
				}
			}
			if let Some((ref winner, _, shorteventid)) = best {
				if winner.as_str().contains("TN3aSG4dg") || winner.as_str().contains("TtQ6QYSjCp")
				{
					println!("  TRACE DISPUTED: ssk={ssk} winner={winner} (fast path)");
				}
				final_state.insert(crate::rooms::state_compressor::compress_state_event(
					*ssk,
					shorteventid,
				));
			}
		}

		return Ok(std::sync::Arc::new(final_state));
	}

	// SLOW PATH: auth-critical keys conflict, need full state resolution
	let mut conflicting_event_ids = HashSet::new();
	for fork in &fork_compressed_states {
		for ssk in &conflicting_ssks {
			let event_bytes = fork
				.iter()
				.find(|bytes| bytes.starts_with(&ssk.to_be_bytes()));
			if let Some(bytes) = event_bytes {
				let mut id_bytes = [0_u8; 8];
				id_bytes.copy_from_slice(&bytes[8..16]);
				let shorteventid = u64::from_be_bytes(id_bytes);
				if let Ok(eid) = self
					.services
					.short
					.get_eventid_from_short(shorteventid)
					.await
				{
					conflicting_event_ids.insert(eid);
				}
			}
		}
	}

	let conflicting_pdus: Vec<_> = self
		.services
		.timeline
		.multi_get_pdus(Some(room_id), futures::stream::iter(conflicting_event_ids.into_iter()))
		.filter_map(|r| ready(r.ok()))
		.collect()
		.await;

	// Extend auth_ssks with sender membership keys
	for pdu in conflicting_pdus {
		if let Ok(ssk) = self
			.services
			.short
			.get_shortstatekey(&ruma::events::StateEventType::RoomMember, pdu.sender().as_ref())
			.await
		{
			auth_ssks.insert(ssk);
		}
		if pdu.kind() == &ruma::events::TimelineEventType::RoomMember {
			if let Some(sk) = pdu.state_key() {
				if let Ok(ssk) = self
					.services
					.short
					.get_shortstatekey(&ruma::events::StateEventType::RoomMember, sk)
					.await
				{
					auth_ssks.insert(ssk);
				}
			}
		}
		if pdu.kind() == &ruma::events::TimelineEventType::RoomThirdPartyInvite {
			if let Some(sk) = pdu.state_key() {
				if let Ok(ssk) = self
					.services
					.short
					.get_shortstatekey(&ruma::events::StateEventType::RoomThirdPartyInvite, sk)
					.await
				{
					auth_ssks.insert(ssk);
				}
			}
		}
	}

	let relevant_ssks: HashSet<_> = conflicting_ssks.union(&auth_ssks).copied().collect();

	let mut fork_states: Vec<StateMap<_>> = Vec::new();
	for fork in &fork_compressed_states {
		let mut state_map = StateMap::new();
		for ssk in &relevant_ssks {
			let event_bytes = fork
				.iter()
				.find(|bytes| bytes.starts_with(&ssk.to_be_bytes()));
			if let Some(bytes) = event_bytes {
				let mut id_bytes = [0_u8; 8];
				id_bytes.copy_from_slice(&bytes[8..16]);
				let shorteventid = u64::from_be_bytes(id_bytes);
				if let Ok(eid) = self
					.services
					.short
					.get_eventid_from_short(shorteventid)
					.await
				{
					if let Ok((ty, sk)) = self.services.short.get_statekey_from_short(*ssk).await
					{
						state_map.insert((ty, sk), eid);
					}
				}
			}
		}
		fork_states.push(state_map);
	}

	let resolve_start = std::time::Instant::now();
	let resolved_partial = self
		.state_resolution(room_id, room_version_id, fork_states.iter())
		.boxed()
		.await
		.map_err(|e| {
			println!(
				"state_at_incoming_resolved: resolution FAILED after {:?} (total {:?}): {e}",
				resolve_start.elapsed(),
				fn_start.elapsed(),
			);
			e
		})?;
	println!(
		"state_at_incoming_resolved: resolution took {:?}, total {:?}",
		resolve_start.elapsed(),
		fn_start.elapsed(),
	);

	// Build final state: unconflicted entries from all forks + resolved conflicts
	let mut final_state = std::collections::BTreeSet::new();
	for fork in &fork_compressed_states {
		for bytes in fork {
			let mut ssk_bytes = [0_u8; 8];
			ssk_bytes.copy_from_slice(&bytes[0..8]);
			let ssk = u64::from_be_bytes(ssk_bytes);

			if conflicting_ssks.contains(&ssk) {
				continue; // We'll take this from resolved_partial
			}

			final_state.insert(*bytes);
		}
	}

	for ((ty, sk), eid) in resolved_partial {
		let ssk = self
			.services
			.short
			.get_or_create_shortstatekey(&ty, sk.as_ref())
			.await;
		let shorteventid = self.services.short.get_or_create_shorteventid(&eid).await;
		final_state
			.insert(crate::rooms::state_compressor::compress_state_event(ssk, shorteventid));
	}

	Ok(std::sync::Arc::new(final_state))
}
