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
use ruma::{OwnedEventId, RoomId, RoomVersionId};

// TODO: if we know the prev_events of the incoming event we can avoid the
// request and build the state from a known point and resolve if > 1 prev_event
#[implement(super::Service)]
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub(super) async fn state_at_incoming_degree_one<Pdu>(
	&self,
	incoming_pdu: &Pdu,
	room_id: &RoomId,
) -> Result<Option<HashMap<u64, OwnedEventId>>>
where
	Pdu: Event + Send + Sync,
{
	let prev_event = incoming_pdu
		.prev_events()
		.next()
		.expect("at least one prev_event");

	let Ok(prev_pdu) = self
		.services
		.timeline
		.get_pdu_in_room(Some(room_id), prev_event)
		.await
	else {
		return Ok(None);
	};

	if prev_pdu.room_id() != Some(room_id) {
		return Ok(None);
	}

	let Ok(prev_event_sstatehash) = self
		.services
		.state_accessor
		.pdu_shortstatehash(prev_event)
		.await
	else {
		return Ok(None);
	};

	let mut state: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(prev_event_sstatehash)
		.collect()
		.await;

	debug!("Using cached state");

	if let Some(state_key) = &prev_pdu.state_key {
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&prev_pdu.kind().to_string().into(), state_key)
			.await;

		state.insert(shortstatekey, prev_event.to_owned());
		// Now it's the state after the pdu
	}

	debug_assert!(!state.is_empty(), "should be returning None for empty HashMap result");

	Ok(Some(state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "state", level = "debug", skip_all)]
pub async fn state_at_incoming_resolved<Pdu>(
	&self,
	incoming_pdu: &Pdu,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<Option<HashMap<u64, OwnedEventId>>>
where
	Pdu: Event + Send + Sync,
{
	let fn_start = std::time::Instant::now();
	trace!("Calculating extremity statehashes...");
	let Ok(extremity_sstatehashes) = incoming_pdu
		.prev_events()
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
		.try_collect::<HashMap<_, _>>()
		.await
	else {
		return Ok(None);
	};

	let num_forks = extremity_sstatehashes.len();
	trace!("Calculating fork states ({num_forks} forks)...");

	let mut fork_compressed_states = Vec::with_capacity(extremity_sstatehashes.len());
	for (sstatehash, prev_event) in &extremity_sstatehashes {
		let mut state = self
			.services
			.state_compressor
			.load_shortstatehash_info(*sstatehash)
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

	let first_fork = &fork_compressed_states[0];
	let mut conflicting_ssks = HashSet::new();

	for fork in &fork_compressed_states[1..] {
		for diff in first_fork.symmetric_difference(fork) {
			let mut ssk_bytes = [0_u8; 8];
			ssk_bytes.copy_from_slice(&diff[0..8]);
			conflicting_ssks.insert(u64::from_be_bytes(ssk_bytes));
		}
	}

	println!(
		"state_at_incoming_resolved: {num_forks} forks, {} conflicting keys, {} total state entries (took {:?} to compute)",
		conflicting_ssks.len(),
		first_fork.len(),
		fn_start.elapsed(),
	);

	if conflicting_ssks.is_empty() {
		// All forks are identical!
		println!("state_at_incoming_resolved: TRIVIAL MERGE (0 conflicts) — skipping resolution");
		let mut state_map = HashMap::new();
		for bytes in first_fork {
			let mut ssk_bytes = [0_u8; 8];
			ssk_bytes.copy_from_slice(&bytes[0..8]);
			let ssk = u64::from_be_bytes(ssk_bytes);

			let mut id_bytes = [0_u8; 8];
			id_bytes.copy_from_slice(&bytes[8..16]);
			let shorteventid = u64::from_be_bytes(id_bytes);

			if let Ok(eid) = self
				.services
				.short
				.get_eventid_from_short(shorteventid)
				.await
			{
				state_map.insert(ssk, eid);
			}
		}
		return Ok(Some(state_map));
	}

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

	let mut auth_ssks = HashSet::new();
	for ty in &[
		ruma::events::StateEventType::RoomCreate,
		ruma::events::StateEventType::RoomPowerLevels,
		ruma::events::StateEventType::RoomJoinRules,
	] {
		if let Ok(ssk) = self.services.short.get_shortstatekey(ty, "").await {
			auth_ssks.insert(ssk);
		}
	}

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
	let Ok(resolved_partial) = self
		.state_resolution(room_id, room_version_id, fork_states.iter())
		.boxed()
		.await
	else {
		println!(
			"state_at_incoming_resolved: resolution FAILED after {:?} (total {:?})",
			resolve_start.elapsed(),
			fn_start.elapsed(),
		);
		return Ok(None);
	};
	println!(
		"state_at_incoming_resolved: resolution took {:?}, total {:?}",
		resolve_start.elapsed(),
		fn_start.elapsed(),
	);

	let mut final_state = HashMap::new();
	for bytes in first_fork {
		let mut ssk_bytes = [0_u8; 8];
		ssk_bytes.copy_from_slice(&bytes[0..8]);
		let ssk = u64::from_be_bytes(ssk_bytes);

		if conflicting_ssks.contains(&ssk) {
			continue; // We'll take this from resolved_partial
		}

		let mut id_bytes = [0_u8; 8];
		id_bytes.copy_from_slice(&bytes[8..16]);
		let shorteventid = u64::from_be_bytes(id_bytes);

		if let Ok(eid) = self
			.services
			.short
			.get_eventid_from_short(shorteventid)
			.await
		{
			final_state.insert(ssk, eid);
		}
	}

	for ((ty, sk), eid) in resolved_partial {
		let ssk = self
			.services
			.short
			.get_or_create_shortstatekey(&ty, sk.as_ref())
			.await;
		final_state.insert(ssk, eid);
	}

	Ok(Some(final_state))
}
