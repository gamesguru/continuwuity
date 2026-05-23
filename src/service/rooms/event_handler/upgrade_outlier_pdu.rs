use std::{
	borrow::Borrow,
	collections::{BTreeMap, HashMap},
	sync::Arc,
	time::Instant,
};

use conduwuit::{
	Err, Result, debug, debug_info, debug_warn, err, implement, info,
	matrix::{Event, EventTypeExt, PduEvent, StateKey, state_res},
	trace,
	utils::stream::ReadyExt,
	warn,
};
use futures::{FutureExt, StreamExt, future::ready};
use ruma::{CanonicalJsonValue, RoomId, ServerName, events::StateEventType};

use super::{get_room_version_id, to_room_version};
use crate::rooms::{
	state_compressor::{CompressedState, HashSetCompressStateEvent},
	timeline::RawPduId,
};

#[implement(super::Service)]
#[allow(clippy::too_many_arguments)]
pub async fn upgrade_outlier_to_timeline_pdu<Pdu>(
	&self,
	incoming_pdu: PduEvent,
	val: BTreeMap<String, CanonicalJsonValue>,
	create_event: &Pdu,
	origin: &ServerName,
	room_id: &RoomId,
	skip_soft_fail: bool,
	is_forward_extremity: bool,
) -> Result<Option<RawPduId>>
where
	Pdu: Event + Send + Sync,
{
	// Skip the PDU if we already have it as a timeline event
	if let Ok(pduid) = self
		.services
		.timeline
		.get_pdu_id(incoming_pdu.event_id())
		.await
	{
		return Ok(Some(pduid));
	}

	let (rejected, soft_failed_early) = tokio::join!(
		self.services
			.pdu_metadata
			.is_event_rejected(incoming_pdu.event_id()),
		self.services
			.pdu_metadata
			.is_event_soft_failed(incoming_pdu.event_id())
	);
	if rejected && !skip_soft_fail {
		return Err!(Request(Forbidden("Event has been rejected")));
	} else if soft_failed_early && !skip_soft_fail {
		// Return Ok(None) so the remote server stops endlessly retrying
		info!("Event was previously soft-failed; acknowledging receipt");
		return Ok(None);
	}

	// If any of the auth events are rejected, this event is also rejected.
	if !skip_soft_fail {
		for aid in incoming_pdu.auth_events() {
			if self.services.pdu_metadata.is_event_rejected(aid).await {
				info!(
					"Rejecting incoming event {} which depends on rejected auth event {aid}",
					incoming_pdu.event_id()
				);
				self.services
					.pdu_metadata
					.mark_event_rejected(incoming_pdu.event_id());
				return Err!(Request(Forbidden("Event has rejected auth event")));
			}
		}
	}

	debug!(
		event_id = %incoming_pdu.event_id,
		"Upgrading PDU from outlier to timeline"
	);
	let timer = Instant::now();
	let room_version_id = get_room_version_id(create_event)?;

	// 10. Fetch missing state and auth chain events by calling /state_ids at
	//     backwards extremities doing all the checks in this list starting at 1.
	//     These are not timeline events.

	debug!(
		event_id = %incoming_pdu.event_id,
		"Resolving state at event"
	);
	let mut state_at_incoming_event = if incoming_pdu.prev_events().count() == 1 {
		self.state_at_incoming_degree_one(&incoming_pdu, room_id)
			.await?
	} else {
		self.state_at_incoming_resolved(&incoming_pdu, room_id, &room_version_id)
			.await?
	};

	if state_at_incoming_event.is_none() {
		if !skip_soft_fail {
			// Local state is unavailable — prev_events are not yet in DB or their
			// state hashes have not been computed. Attempt a synchronous /state_ids
			// fetch from the sending server BEFORE queuing the async DAG healer.
			//
			// The healer fires asynchronously (after a delay), which races with the
			// sending server's lifetime: in Complement tests the fake federation
			// server shuts down when the test times out, so the healer's /state_ids
			// calls always arrive too late and "all servers failed". Fetching inline
			// here gives us a shot while the sender is still alive.
			debug!(
				event_id = %incoming_pdu.event_id,
				%origin,
				"local state unavailable; attempting synchronous /state_ids fetch"
			);
			match self
				.fetch_state(origin, create_event, room_id, incoming_pdu.event_id(), false)
				.await
			{
				| Ok(Some(fetched_state)) => {
					info!(
						target: "state_res_debug",
						event_id = %incoming_pdu.event_id,
						n_state = fetched_state.len(),
						"fetched state via /state_ids; proceeding with auth check"
					);
					state_at_incoming_event = Some(fetched_state);
				},
				| Ok(None) | Err(_) => {
					// /state_ids also failed — hand off to the healer for a later retry.
					let _ = self.dag_healer.send(super::HealRequest::MissingState {
						room_id: room_id.to_owned(),
						event_id: incoming_pdu.event_id().to_owned(),
						origin: origin.to_owned(),
						waiting_pdu: None,
					});

					return Err(conduwuit::Error::MissingAuthEvents(vec![]));
				},
			}
		}
	}

	if state_at_incoming_event.is_none() {
		if skip_soft_fail {
			warn!(
				event_id = %incoming_pdu.event_id,
				"Could not find state at event, but skip_soft_fail is set — using current room state as fallback"
			);
			// Use the current room state as the base instead of empty state.
			// This ensures state resolution has real data to work with and
			// won't wipe the room's state.
			let current_shortstatehash = self
				.services
				.state
				.get_room_shortstatehash(room_id)
				.await
				.map_err(|_| err!(Database("Room has no state")))?;

			let current_state: HashMap<_, _> = self
				.services
				.state_accessor
				.state_full_shortids(current_shortstatehash)
				.ready_filter_map(Result::ok)
				.map(|(shortstatekey, shorteventid)| async move {
					let event_id = self
						.services
						.short
						.get_eventid_from_short::<Box<_>>(shorteventid)
						.await
						.ok()?;
					Some((shortstatekey, (*event_id).to_owned()))
				})
				.buffer_unordered(64)
				.filter_map(ready)
				.collect()
				.await;

			state_at_incoming_event = Some(current_state);
		} else {
			return Err!(Request(Unknown("Could not find state at event")));
		}
	}

	let state_at_incoming_event = state_at_incoming_event.unwrap_or_default();

	let room_version = to_room_version(&room_version_id);

	debug!(
		event_id = %incoming_pdu.event_id,
		"Performing auth check to upgrade"
	);
	// 11. Check the auth of the event passes based on the state of the event
	let state_fetch_state = &state_at_incoming_event;
	let state_fetch = |k: StateEventType, s: StateKey| async move {
		let shortstatekey = self.services.short.get_shortstatekey(&k, &s).await.ok()?;

		let event_id = state_fetch_state.get(&shortstatekey)?;
		self.services.timeline.get_pdu(event_id).await.ok()
	};

	debug!(
		event_id = %incoming_pdu.event_id,
		"Running initial auth check"
	);
	let auth_check = state_res::event_auth::auth_check(
		&room_version,
		&incoming_pdu,
		None, // TODO: third party invite
		|ty, sk| state_fetch(ty.clone(), sk.into()),
		create_event.as_pdu(),
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	if !auth_check {
		if skip_soft_fail {
			warn!(
				event_id = %incoming_pdu.event_id,
				"Event failed auth check against state-at-event, but skip_soft_fail is set — continuing"
			);
		} else {
			// SYNAPSE PARITY: Mark as REJECTED, not soft-failed!
			self.services
				.pdu_metadata
				.mark_event_rejected(incoming_pdu.event_id());

			return Err!(Request(Forbidden(
				"Event authorisation fails based on the state before the event"
			)));
		}
	}

	// 13. Use state resolution to find new room state

	// Pre-fetch missing auth chain events from federation BEFORE
	// acquiring the room lock. This is parallel (32 concurrent) and
	// multi-server (origin + trusted + room members) with a 300s budget.
	if incoming_pdu.state_key().is_some() {
		self.pre_fetch_state_res_deps(
			room_id,
			&room_version_id,
			&state_at_incoming_event,
			origin,
		)
		.await;
	}

	// We start looking at current room state now, so lets lock the room
	trace!(
		room_id = %room_id,
		"Locking the room"
	);
	let state_lock = self.services.state.mutex.lock(room_id).await;

	// Re-check if the PDU was added to the timeline while we were waiting for the
	// lock
	if let Ok(pduid) = self
		.services
		.timeline
		.get_pdu_id(incoming_pdu.event_id())
		.await
	{
		return Ok(Some(pduid));
	}

	debug!(
		event_id = %incoming_pdu.event_id,
		"Gathering auth events"
	);
	let auth_events = self
		.services
		.state
		.get_auth_events(
			room_id,
			incoming_pdu.kind(),
			incoming_pdu.sender(),
			incoming_pdu.state_key(),
			incoming_pdu.content(),
			&room_version,
		)
		.await?;

	let state_fetch = |k: &StateEventType, s: &str| {
		let key = k.with_state_key(s);
		ready(auth_events.get(&key).map(ToOwned::to_owned))
	};

	debug!(
		event_id = %incoming_pdu.event_id,
		"Running auth check with claimed state auth"
	);
	let auth_check = state_res::event_auth::auth_check(
		&room_version,
		&incoming_pdu,
		None, // third-party invite
		state_fetch,
		create_event.as_pdu(),
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	// Soft fail check before doing state res
	debug!(
		event_id = %incoming_pdu.event_id,
		"Performing soft-fail check"
	);
	let mut soft_fail = if skip_soft_fail {
		false
	} else {
		match (auth_check, incoming_pdu.redacts_id(&room_version_id)) {
			| (false, _) => {
				info!(
					event_id = %incoming_pdu.event_id,
					"Soft-failing: auth check against current state failed"
				);
				true
			},
			| (true, None) => false,
			| (true, Some(redact_id)) =>
				!self
					.services
					.state_accessor
					.user_can_redact(&redact_id, incoming_pdu.sender(), room_id, true)
					.await?,
		}
	};

	let state_ids_compressed: Arc<CompressedState> = self
		.services
		.state_compressor
		.compress_state_events(
			state_at_incoming_event
				.iter()
				.map(|(ssk, eid)| (ssk, eid.borrow())),
		)
		.collect()
		.map(Arc::new)
		.await;

	// Finalize soft_fail before any state processing: check policy server
	// and redaction status so we can skip expensive state resolution for
	// events that will be rejected.
	if !soft_fail {
		// 14-pre. If the event is not a state event, ask the policy server about it
		if incoming_pdu.state_key.is_none() {
			debug!(event_id = %incoming_pdu.event_id, "Checking policy server for event");
			match self
				.ask_policy_server(
					&incoming_pdu,
					&mut incoming_pdu.to_canonical_object(),
					room_id,
					true,
				)
				.await
			{
				| Ok(false) => {
					warn!(
						event_id = %incoming_pdu.event_id,
						"Event has been marked as spam by policy server"
					);
					soft_fail = true;
				},
				| _ => {
					debug!(
						event_id = %incoming_pdu.event_id,
						"Event has passed policy server check or the policy server was unavailable."
					);
				},
			}
		}

		// Additionally, if this is a redaction for a soft-failed event, we soft-fail it
		// also.

		// TODO: this is supposed to hide redactions from policy servers, however, for
		// full efficacy it also needs to hide redactions for unknown events. This
		// needs to be investigated at a later time.
		if let Some(redact_id) = incoming_pdu.redacts_id(&room_version_id) {
			debug!(
				redact_id = %redact_id,
				"Checking if redaction is for a soft-failed or rejected event"
			);
			if !self
				.services
				.pdu_metadata
				.is_event_accepted(&redact_id)
				.await
			{
				warn!(
					redact_id = %redact_id,
					"Redaction targets a non-accepted event, soft failing"
				);
				soft_fail = true;
			}
		}
	}

	// Derive new room state for all incoming state events, including
	// soft-failed ones. State resolution merges forks deterministically —
	// a soft-failed event may carry state from a fork we haven't seen,
	// and feeding it into resolve_state heals local drift. This is the
	// continuous state reconciliation mechanism in Matrix federation.
	if incoming_pdu.state_key().is_some() {
		debug!("Event is a state-event. Deriving new room state");

		// We also add state after incoming event to the fork states
		let mut state_after = state_at_incoming_event.clone();
		if let Some(state_key) = incoming_pdu.state_key() {
			let shortstatekey = self
				.services
				.short
				.get_or_create_shortstatekey(&incoming_pdu.kind().to_string().into(), state_key)
				.await;

			let event_id = incoming_pdu.event_id();
			state_after.insert(shortstatekey, event_id.to_owned());
		}

		let new_room_state = {
			let t = Instant::now();
			info!(
				event_id = %incoming_pdu.event_id(),
				%room_id,
				"state_res: starting resolve_state for incoming state event"
			);
			let result = self
				.resolve_state(room_id, &room_version_id, state_after)
				.await?;
			info!(
				event_id = %incoming_pdu.event_id(),
				%room_id,
				elapsed = ?t.elapsed(),
				"state_res: resolve_state complete"
			);
			result
		};

		// Set the new room state to the resolved state
		debug!("Forcing new room state");
		let HashSetCompressStateEvent { shortstatehash, added, removed } = self
			.services
			.state_compressor
			.save_state(room_id, new_room_state)
			.await?;

		Box::pin(self.services.state.force_state(
			room_id,
			shortstatehash,
			added,
			removed,
			&state_lock,
		))
		.await?;
	}

	// Calculate forward extremities AFTER the soft-fail evaluation.
	// Per spec, soft-failed events are NOT added as forward extremities.
	trace!("Appending pdu to timeline");
	let current_extremities: Vec<_> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let prev_events: Vec<&ruma::EventId> = incoming_pdu.prev_events().collect();
	let room_id_owned = room_id.to_owned();
	let is_referenced = |event_id: &ruma::EventId| {
		let eid = event_id.to_owned();
		let rid = room_id_owned.clone();
		async move {
			self.services
				.pdu_metadata
				.is_event_referenced(&rid, &eid)
				.await
		}
	};

	let extremities = super::extremities::calculate_forward_extremities(
		current_extremities,
		incoming_pdu.event_id(),
		&prev_events,
		soft_fail,
		is_referenced,
		is_forward_extremity,
	)
	.await;

	let pdu_id = self
		.services
		.timeline
		.append_incoming_pdu(
			&incoming_pdu,
			val,
			extremities.iter().map(Borrow::borrow),
			state_ids_compressed,
			soft_fail,
			&state_lock,
			room_id,
		)
		.await?;

	if soft_fail {
		self.services
			.pdu_metadata
			.mark_event_soft_failed(incoming_pdu.event_id());

		debug_warn!(
			elapsed = ?timer.elapsed(),
			"Event has been soft-failed",
		);
	} else {
		debug_info!(
			elapsed = ?timer.elapsed(),
			"Accepted",
		);
	}

	// Event has passed all auth/stateres checks
	drop(state_lock);

	Ok(pdu_id)
}
