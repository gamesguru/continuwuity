use std::{
	borrow::Borrow,
	collections::{BTreeMap, HashMap},
	sync::Arc,
	time::Instant,
};

use conduwuit::{
	Err, Result, debug, debug_info, debug_warn, err, implement, info,
	matrix::{Event, EventTypeExt, PduEvent, StateKey, state_res},
	trace, warn,
};
use futures::{FutureExt, StreamExt, future::ready};
use ruma::{
	CanonicalJsonValue, OwnedEventId, RoomId, RoomVersionId, ServerName, events::StateEventType,
};

use super::{get_room_version_id, to_room_version};
use crate::rooms::{
	state_compressor::{CompressedState, HashSetCompressStateEvent},
	timeline::RawPduId,
};

/// Upgrade an outlier PDU to a full timeline event.
///
/// Performs auth checks, state resolution, soft-fail evaluation, and finally
/// appends the PDU to the room timeline.  The function is deliberately kept
/// thin; the heavy lifting is delegated to the helpers below so that each
/// async state-machine stays within the stack-frame budget.
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
	force_local: bool,
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
		if self
			.services
			.pdu_metadata
			.is_event_accepted(incoming_pdu.event_id())
			.await
		{
			return Ok(Some(pduid));
		}
	}

	let soft_failed_early = self
		.services
		.pdu_metadata
		.is_event_soft_failed(incoming_pdu.event_id())
		.await;

	if soft_failed_early && !skip_soft_fail {
		// Return Ok(None) so the remote server stops endlessly retrying
		info!("Event was previously soft-failed; acknowledging receipt");
		return Ok(None);
	}

	// If any auth events are rejected/soft-failed, the event is also rejected.
	if !skip_soft_fail {
		for aid in incoming_pdu.auth_events() {
			if !self.services.pdu_metadata.is_event_accepted(aid).await {
				info!(
					"Rejecting incoming event {} which depends on rejected/soft-failed auth \
					 event {aid}",
					incoming_pdu.event_id()
				);
				self.services
					.pdu_metadata
					.mark_event_rejected(incoming_pdu.event_id());
				warn!(
					"Event failed auth_check! Event ID: {}, Sender: {}, Type: {}",
					incoming_pdu.event_id(),
					incoming_pdu.sender(),
					incoming_pdu.kind()
				);
				return Err!(Request(Forbidden("Event depends on rejected auth event {aid}")));
			}
		}
	}

	info!(
		event_id = %incoming_pdu.event_id,
		"Upgrading PDU from outlier to timeline"
	);
	let timer = Instant::now();
	let room_version_id = get_room_version_id(create_event)?;

	// --- Phase 1: resolve state at the incoming event (extracted to reduce frame)
	// ---
	let state_at_incoming_event = self
		.resolve_state_at_incoming_event(
			&incoming_pdu,
			create_event,
			origin,
			room_id,
			&room_version_id,
			skip_soft_fail,
			force_local,
		)
		.await?;

	let room_version = to_room_version(&room_version_id);

	// Check the auth of the event passes based on the state of the event
	debug!(event_id = %incoming_pdu.event_id, "Running initial auth check");
	let state_fetch_state = &state_at_incoming_event;
	let state_fetch = |k: StateEventType, s: StateKey| async move {
		let shortstatekey = self.services.short.get_shortstatekey(&k, &s).await.ok()?;
		let event_id = state_fetch_state.get(&shortstatekey)?;
		self.services.timeline.get_pdu(event_id).await.ok()
	};

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

			warn!(
				"Event failed auth_check! Event ID: {}, Sender: {}, Type: {}",
				incoming_pdu.event_id(),
				incoming_pdu.sender(),
				incoming_pdu.kind()
			);
			return Err!(Request(Forbidden(
				"Event authorisation fails based on the state before the event"
			)));
		}
	}

	// --- Use state resolution to find new room state ---
	//
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

	// Re-check if the PDU was added to the timeline while we were waiting
	if let Ok(pduid) = self
		.services
		.timeline
		.get_pdu_id(incoming_pdu.event_id())
		.await
	{
		if self
			.services
			.pdu_metadata
			.is_event_accepted(incoming_pdu.event_id())
			.await
		{
			return Ok(Some(pduid));
		}
	}

	debug!(event_id = %incoming_pdu.event_id, "Gathering auth events");
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

	debug!(event_id = %incoming_pdu.event_id, "Running auth check with claimed state auth");
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
	debug!(event_id = %incoming_pdu.event_id, "Performing soft-fail check");
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

		// Additionally, if this is a redaction for a soft-failed event, we
		// soft-fail it also.
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

	// --- Phase 2: state event handling (extracted to reduce frame) ---
	//
	// Derive new room state for all incoming state events, including
	// soft-failed ones. State resolution merges forks deterministically —
	// a soft-failed event may carry state from a fork we haven't seen,
	// and feeding it into resolve_state heals local drift.
	//
	// OCC (Optimistic Concurrency Control): We compute the state delta
	// WITHOUT holding the room lock, then acquire the lock and verify the
	// base state hash hasn't changed. If it has, we DROP the lock and
	// retry. This avoids holding the exclusive mutex during CPU-bound
	// state resolution.
	let state_delta_opt;
	let state_lock;

	loop {
		// Capture base state hash BEFORE the unlocked computation
		let base_shortstatehash = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.ok();

		// Heavy computation WITHOUT lock
		let delta = self
			.calculate_state_delta(
				&incoming_pdu,
				state_at_incoming_event.clone(),
				room_id,
				&room_version_id,
			)
			.await?;

		// Acquire lock for the commit phase
		trace!(room_id = %room_id, "Locking the room");
		let lock = self.services.state.mutex.lock(room_id).await;

		// Re-check if the PDU was already added while we were unlocked
		if let Ok(pduid) = self
			.services
			.timeline
			.get_pdu_id(incoming_pdu.event_id())
			.await
		{
			if self
				.services
				.pdu_metadata
				.is_event_accepted(incoming_pdu.event_id())
				.await
			{
				return Ok(Some(pduid));
			}
		}

		// OCC verification: has the base state shifted?
		let current_shortstatehash = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.ok();

		if base_shortstatehash == current_shortstatehash {
			// State is consistent — break while HOLDING the lock
			state_delta_opt = delta;
			state_lock = lock;
			break;
		}

		// State changed — drop the lock and retry so we don't block the room
		info!(
			%room_id,
			?base_shortstatehash,
			?current_shortstatehash,
			"Room state changed during unlocked state-res, dropping lock and retrying"
		);
		drop(lock);
	}

	// 6. Apply the state delta (still holding state_lock from the successful break)
	trace!("Appending pdu to timeline");
	if let Some(HashSetCompressStateEvent { shortstatehash, added, removed }) = state_delta_opt {
		Box::pin(self.services.state.force_state(
			room_id,
			shortstatehash,
			added,
			removed,
			&state_lock,
		))
		.await?;
	}

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
		// Clear any previous rejected markers now that it passed auth organically
		self.services
			.pdu_metadata
			.clear_pdu_markers(incoming_pdu.event_id());
		debug_info!(
			elapsed = ?timer.elapsed(),
			"Accepted",
		);
	}

	// Event has passed all auth/stateres checks
	drop(state_lock);

	Ok(pdu_id)
}

/// Determine the room state that was in effect just before the incoming PDU
/// was sent.  Tries local DB first; if unavailable, falls back to a
/// synchronous `/state_ids` fetch from the sending server; if that also fails,
/// enqueues a DAG-healer request and returns `MissingAuthEvents`.
///
/// When `skip_soft_fail` is set and state cannot be found at all, the current
/// room state is used as a best-effort fallback to avoid wiping state.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
#[allow(clippy::too_many_arguments)]
async fn resolve_state_at_incoming_event<Pdu>(
	&self,
	incoming_pdu: &PduEvent,
	create_event: &Pdu,
	origin: &ServerName,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	skip_soft_fail: bool,
	force_local: bool,
) -> Result<HashMap<u64, OwnedEventId>>
where
	Pdu: Event + Send + Sync,
{
	// 10. Fetch missing state and auth chain events by calling /state_ids at
	//     backwards extremities doing all the checks in this list starting at 1.
	//     These are not timeline events.
	let current_extremities: Vec<_> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let prev_events: Vec<_> = incoming_pdu.prev_events().map(ToOwned::to_owned).collect();
	let exact_match = !current_extremities.is_empty()
		&& prev_events.len() == current_extremities.len()
		&& current_extremities.iter().all(|e| prev_events.contains(e));

	// Fast-forward path: If the incoming PDU matches current extremities exactly,
	// we can bypass state resolution entirely and just use the current room state.
	if exact_match {
		info!(
			"Incoming PDU matches current extremities exactly (fast-forward candidate). \
			 Fetching current state..."
		);
		if let Ok(current_shortstatehash) =
			self.services.state.get_room_shortstatehash(room_id).await
		{
			if current_shortstatehash != 0 {
				return Ok(self
					.services
					.state_accessor
					.state_full_ids(current_shortstatehash)
					.collect()
					.await);
			}
		}
	}

	// Local State Resolution: Try to resolve state locally using prev_events
	info!(
		"Resolving state locally for incoming PDU (prev_events count: {})",
		incoming_pdu.prev_events().count()
	);

	let mut state = if incoming_pdu.prev_events().count() == 1 {
		self.state_at_incoming_degree_one(incoming_pdu, room_id)
			.await?
	} else {
		self.state_at_incoming_resolved(incoming_pdu, room_id, room_version_id)
			.await?
	};

	// Federation Fetch: If local resolution failed, try fetching state from the
	// sender
	if state.is_none() && !skip_soft_fail && !force_local {
		let mut prev_event_rejected = false;
		for prev_id in &prev_events {
			if self.services.pdu_metadata.is_event_rejected(prev_id).await {
				prev_event_rejected = true;
				info!(
					event_id = %incoming_pdu.event_id,
					rejected_prev_event = %prev_id,
					"prev_event is rejected; bypassing federation /state_ids fetch"
				);
				break;
			}
		}

		if !prev_event_rejected {
			// Local state is unavailable — prev_events are not yet in DB or their
			// state hashes have not been computed. Attempt a synchronous /state_ids
			// fetch from the sending server BEFORE queuing the async DAG healer.
			debug!(
				event_id = %incoming_pdu.event_id,
				%origin,
				"local state unavailable; attempting synchronous /state_ids fetch"
			);

			if let Ok(Some(fetched_state)) = self
				.fetch_state(origin, create_event, room_id, incoming_pdu.event_id(), false)
				.await
			{
				info!(
					target: "state_res_debug",
					event_id = %incoming_pdu.event_id,
					n_state = fetched_state.len(),
					"fetched state via /state_ids; proceeding with auth check"
				);
				state = Some(fetched_state);
			} else {
				debug!(
					event_id = %incoming_pdu.event_id,
					"fetch_state failed; falling back to current room state"
				);
			}
		}
	}

	// Fallback: If all else fails, use the current room state
	if let Some(resolved_state) = state {
		Ok(resolved_state)
	} else {
		// State could not be determined from prev_events or federation.
		// Fall back to current room state — the auth check at step 11 will
		// still reject invalid events.
		debug!(
			event_id = %incoming_pdu.event_id,
			"Could not find state at event — using current room state as fallback"
		);
		let current_shortstatehash = self
			.services
			.state
			.get_room_shortstatehash(room_id)
			.await
			.map_err(|_| err!(Database("Room has no state")))?;

		Ok(self
			.services
			.state_accessor
			.state_full_ids(current_shortstatehash)
			.collect()
			.await)
	}
}

/// For state events: build the new post-event state, run state resolution
/// against the current room state, and return the state delta for application.
///
/// Extracted from `upgrade_outlier_to_timeline_pdu` to give `save_state`
/// its own async state-machine frame and allow Optimistic Concurrency Control
/// (running without a lock).
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn calculate_state_delta(
	&self,
	incoming_pdu: &PduEvent,
	state_at_incoming_event: HashMap<u64, OwnedEventId>,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<Option<HashSetCompressStateEvent>> {
	if incoming_pdu.state_key().is_none() {
		return Ok(None);
	}

	debug!("Event is a state-event. Deriving new room state");

	// We also add state after incoming event to the fork states
	let mut state_after = state_at_incoming_event;
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
			.resolve_state(room_id, room_version_id, state_after)
			.await?;
		info!(
			event_id = %incoming_pdu.event_id(),
			%room_id,
			elapsed = ?t.elapsed(),
			"state_res: resolve_state complete"
		);
		result
	};

	// Save the resolved state delta into the database (safe to do concurrently)
	debug!("Compressing new room state");
	let state_delta = self
		.services
		.state_compressor
		.save_state(room_id, new_room_state)
		.await?;

	Ok(Some(state_delta))
}
