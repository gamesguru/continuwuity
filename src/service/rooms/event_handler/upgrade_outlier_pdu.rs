use std::{
	borrow::Borrow,
	collections::{BTreeMap, HashMap},
	sync::Arc,
	time::Instant,
};

use conduwuit::{
	Err, Result, debug, debug_info, debug_warn, err, implement, info,
	matrix::{Event, PduEvent, StateKey, state_res},
	trace,
	utils::stream::ReadyExt,
	warn,
};
use futures::{FutureExt, StreamExt, future::ready};
use ruma::{
	CanonicalJsonValue, OwnedEventId, RoomId, RoomVersionId, ServerName, events::StateEventType,
};

use super::{get_room_version_id, to_room_version};
use crate::rooms::{
	short::ShortStateHash, state_compressor::HashSetCompressStateEvent, timeline::RawPduId,
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
	// Non-spec-compliant admin override to force-accept events.
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

	// If we reject/soft-fail/are missing auth events, the event is also rejected.
	if !skip_soft_fail {
		for aid in incoming_pdu.auth_events() {
			let exists = self.services.timeline.pdu_exists(aid).await;
			let accepted = self.services.pdu_metadata.is_event_accepted(aid).await;
			if !exists || !accepted {
				info!(
					"Rejecting incoming event {} which depends on missing/rejected auth event \
					 {aid}",
					incoming_pdu.event_id()
				);
				self.services.pdu_metadata.mark_event_rejected(
					incoming_pdu.event_id(),
					&format!("depends on missing or rejected auth event {aid}"),
				);
				return Err!(Request(Forbidden(
					"Event depends on missing or rejected auth event {aid}"
				)));
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
	let current_extremities: Vec<OwnedEventId> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.collect()
		.await;

	let prev_events_vec: Vec<_> = incoming_pdu.prev_events().map(ToOwned::to_owned).collect();
	let is_fast_forward = !current_extremities.is_empty()
		&& current_extremities.len() == prev_events_vec.len()
		&& current_extremities
			.iter()
			.all(|e| prev_events_vec.contains(e));

	// Pre-fetch missing auth chain events from federation BEFORE state resolution.
	// We deleted iterative pre-fetching, so for non-fast-forward state events we
	// must blindly trigger a bulk /state_ids to ensure state_res has the auth
	// chain.
	//
	// Skip the pre-fetch entirely if any auth event is already rejected locally.
	// The event will be rejected anyway during auth checking, so the /state_ids
	// call would be wasted network traffic.
	if !is_fast_forward && !skip_soft_fail {
		let any_auth_rejected = futures::stream::iter(incoming_pdu.auth_events())
			.any(|aid| async move { self.services.pdu_metadata.is_event_rejected(aid).await })
			.await;

		let any_prev_rejected = futures::stream::iter(incoming_pdu.prev_events())
			.any(|pid| async move { self.services.pdu_metadata.is_event_rejected(pid).await })
			.await;

		if any_auth_rejected || any_prev_rejected {
			debug!(
				event_id = %incoming_pdu.event_id,
				"Skipping /state pre-fetch: auth or prev events include rejected events"
			);
		} else {
			debug!(
				event_id = %incoming_pdu.event_id,
				"Event is a DAG fork state event; pre-fetching auth chain via /state_ids"
			);
			let _ = Box::pin(self.fetch_state(
				origin,
				create_event,
				room_id,
				incoming_pdu.event_id(),
				false,
			))
			.await;
		}
	}

	let mut state_at_incoming_event = Box::pin(self.resolve_state_at_incoming_event(
		&incoming_pdu,
		create_event,
		origin,
		room_id,
		&room_version_id,
		skip_soft_fail,
	))
	.await?;

	let room_version = to_room_version(&room_version_id);

	// Re-check if the PDU was added to the timeline while we were waiting
	if let Ok(pduid) = self
		.services
		.timeline
		.get_pdu_id(incoming_pdu.event_id())
		.await
	{
		return Ok(Some(pduid));
	}

	debug!(event_id = %incoming_pdu.event_id, "Gathering explicitly claimed auth events");
	let mut auth_events = HashMap::new();
	let mut missing_auth_events = false;

	for event_id in incoming_pdu.auth_events() {
		let is_rejected = self.services.pdu_metadata.is_event_rejected(event_id).await;
		if is_rejected && !skip_soft_fail {
			warn!(
				event_id = %incoming_pdu.event_id,
				auth_event_id = %event_id,
				"Event rejected because auth_event is rejected"
			);
			self.services.pdu_metadata.mark_event_rejected(
				incoming_pdu.event_id(),
				&format!("auth event {event_id} is rejected"),
			);
			return Err!(Request(Forbidden(
				"Event authorisation fails because it references a rejected auth_event"
			)));
		}

		if let Ok(pdu) = self.services.timeline.get_pdu(event_id).await {
			if let Some(state_key) = &pdu.state_key {
				let key = StateEventType::from(pdu.kind().clone());
				auth_events.insert((key, state_key.clone()), pdu);
			}
		} else {
			missing_auth_events = true;
		}
	}

	if missing_auth_events {
		debug!(event_id = %incoming_pdu.event_id, "Missing claimed auth events locally. Falling back to state-based auth events");
		if let Ok(state_auth_events) = self
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
			.await
		{
			for ((k, s), pdu) in state_auth_events {
				auth_events.entry((k, s)).or_insert(pdu);
			}
		}
	}

	let state_fetch_auth = |k: &StateEventType, s: &str| {
		let key = (k.to_owned(), s.into());
		ready(auth_events.get(&key).cloned())
	};

	// Check the auth of the event passes based on the claimed auth_events
	debug!(event_id = %incoming_pdu.event_id, "Running auth check with claimed state auth");
	let auth_check_claimed = state_res::event_auth::auth_check(
		&room_version,
		&incoming_pdu,
		None, // third-party invite
		state_fetch_auth,
		create_event.as_pdu(),
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	if !auth_check_claimed {
		if skip_soft_fail {
			warn!(
				event_id = %incoming_pdu.event_id,
				"Event failed auth check against claimed auth_events, but skip_soft_fail is set — continuing"
			);
		} else {
			self.services.pdu_metadata.mark_event_rejected(
				incoming_pdu.event_id(),
				"auth check failed against claimed auth_events",
			);

			return Err!(Request(Forbidden(
				"Event authorisation fails based on its auth_events"
			)));
		}
	}

	// Check auth of event passes based on its state (soft-fail check)
	debug!(event_id = %incoming_pdu.event_id, "Running initial auth check against state-at-event");
	let state_fetch_state = &state_at_incoming_event;
	let state_fetch = |k: StateEventType, s: StateKey| async move {
		match state_fetch_state {
			| StateAtEvent::Resolved(state) => {
				let shortstatekey = self.services.short.get_shortstatekey(&k, &s).await.ok()?;
				let event_id = state.get(&shortstatekey)?;
				self.services.timeline.get_pdu(event_id).await.ok()
			},
			| StateAtEvent::Compressed(compressed) => {
				let shortstatekey = self.services.short.get_shortstatekey(&k, &s).await.ok()?;
				let event_bytes = compressed
					.iter()
					.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))?;
				let mut id_bytes = [0_u8; 8];
				id_bytes.copy_from_slice(&event_bytes[8..16]);
				let shorteventid = u64::from_be_bytes(id_bytes);
				let event_id = self
					.services
					.short
					.get_eventid_from_short::<OwnedEventId>(shorteventid)
					.await
					.ok()?;
				self.services.timeline.get_pdu(&event_id).await.ok()
			},
			| StateAtEvent::FastForward(shortstatehash) => {
				let shorteventid = self
					.services
					.state_accessor
					.state_get_shortid(*shortstatehash, &k, &s)
					.await
					.ok()?;
				let event_id = self
					.services
					.short
					.get_eventid_from_short::<Box<_>>(shorteventid)
					.await
					.ok()?;
				self.services.timeline.get_pdu(&event_id).await.ok()
			},
		}
	};

	let auth_check_state = state_res::event_auth::auth_check(
		&room_version,
		&incoming_pdu,
		None, // TODO: third party invite
		|ty, sk| state_fetch(ty.clone(), sk.into()),
		create_event.as_pdu(),
	)
	.await
	.map_err(|e| err!(Request(Forbidden("Auth check failed: {e:?}"))))?;

	if !auth_check_state {
		if skip_soft_fail {
			warn!(
				event_id = %incoming_pdu.event_id,
				"Event failed auth check against state at event, but skip_soft_fail is set — continuing"
			);
		} else {
			self.services.pdu_metadata.mark_event_rejected(
				incoming_pdu.event_id(),
				"auth check failed against state at event",
			);

			return Err!(Request(Forbidden("Event authorisation fails based on state at event")));
		}
	}

	let mut soft_fail = if skip_soft_fail {
		false
	} else {
		let mut is_soft_failed = match incoming_pdu.redacts_id(&room_version_id) {
			| None => false,
			| Some(redact_id) =>
				!self
					.services
					.state_accessor
					.user_can_redact(&redact_id, incoming_pdu.sender(), room_id, true)
					.await?,
		};

		if !is_soft_failed {
			let auth_check_current = Box::pin(self.check_current_state_auth(
				room_id,
				&room_version,
				&incoming_pdu,
				create_event,
			))
			.await;

			if !auth_check_current {
				warn!(
					event_id = %incoming_pdu.event_id,
					"Event passed auth against state-at-event, but FAILED auth against the current room state. \
					This indicates a DAG fracture. Soft-failing event."
				);
				is_soft_failed = true;
			}
		}

		is_soft_failed
	};

	let state_ids_compressed = match &state_at_incoming_event {
		| StateAtEvent::FastForward(shortstatehash) => {
			self.services
				.state_compressor
				.load_shortstatehash_info(*shortstatehash)
				.await?
				.pop()
				.expect("top frame must have full_state")
				.full_state
				.expect("must have full_state")
				.clone() // This is Arc<CompressedState>
		},
		| StateAtEvent::Compressed(compressed) => compressed.clone(),
		| StateAtEvent::Resolved(state) =>
			self.services
				.state_compressor
				.compress_state_events(state.iter().map(|(ssk, eid)| (ssk, eid.borrow())))
				.collect()
				.map(Arc::new)
				.await,
	};

	// Finalize soft_fail before any state processing: check policy server
	// and redaction status so we can skip expensive state resolution for
	// events that will be rejected.
	if !soft_fail {
		// If the event is not a state event, ask the policy server about it
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

	if !is_forward_extremity {
		state_delta_opt = None;
		// Dummy lock to satisfy lifetimes since we aren't mutating state
		state_lock = self.services.state.mutex.lock(room_id).await;
	} else {
		loop {
			// Capture base state hash BEFORE the unlocked computation
			let base_shortstatehash = self
				.services
				.state
				.get_room_shortstatehash(room_id)
				.await
				.ok();

			if let StateAtEvent::FastForward(shortstatehash) = &state_at_incoming_event {
				if Some(*shortstatehash) != base_shortstatehash {
					info!(
						"Fast-forward state hash shift ({} -> {:?}), re-eval state @ incoming",
						shortstatehash, base_shortstatehash
					);
					state_at_incoming_event = Box::pin(self.resolve_state_at_incoming_event(
						&incoming_pdu,
						create_event,
						origin,
						room_id,
						&room_version_id,
						skip_soft_fail,
					))
					.await?;
				}
			}

			// Heavy computation WITHOUT the lock
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
				return Ok(Some(pduid));
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
	}

	// Apply the state delta (still holding state_lock from the successful break)
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

	let current_extremities: Vec<OwnedEventId> = self
		.services
		.state
		.get_forward_extremities(room_id)
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
			extremities.into_iter(),
			state_ids_compressed,
			soft_fail,
			&state_lock,
			room_id,
		)
		.await?;

	if soft_fail {
		self.services.pdu_metadata.mark_event_soft_failed(
			incoming_pdu.event_id(),
			"auth check failed against current room state",
		);

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

#[derive(Clone)]
enum StateAtEvent {
	Resolved(HashMap<u64, OwnedEventId>),
	Compressed(Arc<crate::rooms::state_compressor::CompressedState>),
	FastForward(ShortStateHash),
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn check_current_state_auth<Pdu>(
	&self,
	room_id: &RoomId,
	room_version: &state_res::RoomVersion,
	incoming_pdu: &PduEvent,
	create_event: &Pdu,
) -> bool
where
	Pdu: Event + Send + Sync,
{
	let state_fetch_current = |k: StateEventType, s: StateKey| async move {
		self.services
			.state_accessor
			.room_state_get(room_id, &k, s.as_ref())
			.await
			.ok()
	};

	state_res::event_auth::auth_check(
		room_version,
		incoming_pdu,
		None,
		|ty, sk| state_fetch_current(ty.clone(), sk.into()),
		create_event.as_pdu(),
	)
	.await
	.unwrap_or(false)
}

/// Find the state-at-event for an incoming PDU. If the PDU is a fast-forward
/// candidate we bypass full state resolution. If we are unable to resolve state
/// (e.g. auth chain fetch fails or soft-fail is active) then the room's current
/// room state is used as a best-effort fallback to avoid wiping state.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn resolve_state_at_incoming_event<Pdu>(
	&self,
	incoming_pdu: &PduEvent,
	create_event: &Pdu,
	origin: &ServerName,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	skip_soft_fail: bool,
) -> Result<StateAtEvent>
where
	Pdu: Event + Send + Sync,
{
	// Fetch missing state and auth chain events by calling /state_ids at
	// backwards extremities doing all the checks in this list starting at 1.
	// These are not timeline events.
	let current_extremities: Vec<OwnedEventId> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.collect()
		.await;

	let prev_events: Vec<_> = incoming_pdu.prev_events().map(ToOwned::to_owned).collect();
	let exact_match = !current_extremities.is_empty()
		&& prev_events.len() == current_extremities.len()
		&& current_extremities.iter().all(|e| prev_events.contains(e));

	let mut state_at_event: Option<StateAtEvent> = None;

	if exact_match {
		info!(
			"Incoming PDU matches current extremities exactly (fast-forward candidate). \
			 Skipping full state lookup."
		);
		if let Ok(current_shortstatehash) =
			self.services.state.get_room_shortstatehash(room_id).await
		{
			if current_shortstatehash != 0 {
				return Ok(StateAtEvent::FastForward(current_shortstatehash));
			}
		}
	}

	if state_at_event.is_none() {
		info!(
			"State is none. Resolving state for incoming PDU (prev_events count: {})",
			incoming_pdu.prev_events().count()
		);
		let resolved_state = if incoming_pdu.prev_events().count() == 1 {
			self.state_at_incoming_degree_one(incoming_pdu, room_id)
				.await
		} else {
			self.state_at_incoming_resolved(incoming_pdu, room_id, room_version_id, None)
				.await
		};
		if let Ok(compressed) = resolved_state {
			state_at_event = Some(StateAtEvent::Compressed(compressed));
		}
		info!("State resolution completed for incoming PDU");
	}

	if state_at_event.is_none() && !skip_soft_fail {
		// Local state is unavailable — prev_events are not yet in DB or their
		// state hashes have not been computed.
		//
		// Before making any network requests, check whether state is missing
		// because prev_events are rejected. If they are, a /state_ids fetch
		// would be wasted traffic — just fall through to the current room
		// state fallback. The auth check will still reject invalid events.
		let all_prevs_rejected = futures::stream::iter(incoming_pdu.prev_events())
			.all(|prev_id| async move {
				self.services.pdu_metadata.is_event_rejected(prev_id).await
					|| self.services.timeline.get_pdu_id(prev_id).await.is_ok()
			})
			.await;

		let any_prev_rejected = futures::stream::iter(incoming_pdu.prev_events())
			.any(
				|prev_id| async move { self.services.pdu_metadata.is_event_rejected(prev_id).await },
			)
			.await;

		if any_prev_rejected && all_prevs_rejected {
			// All non-timeline prev_events are rejected — no point fetching
			// state from federation. Fall through to current room state.
			debug!(
				event_id = %incoming_pdu.event_id,
				"Skipping /state_ids fetch: not a state event or prev_events are rejected; using current room state"
			);
		} else {
			// Attempt a synchronous /state_ids fetch from the sending server
			// BEFORE queuing the async DAG healer.
			//
			// The healer fires asynchronously (after a delay), which races with
			// the sending server's lifetime: in Complement tests the fake
			// federation server shuts down when the test times out, so the
			// healer's /state_ids calls always arrive too late and "all servers
			// failed". Fetching inline here gives us a shot while the sender
			// is still alive.
			debug!(
				event_id = %incoming_pdu.event_id,
				%origin,
				"local state unavailable; attempting synchronous /state_ids fetch"
			);
			match Box::pin(self.fetch_state(
				origin,
				create_event,
				room_id,
				incoming_pdu.event_id(),
				false,
			))
			.await
			{
				| Ok(Some(fetched_state)) => {
					info!(
						target: "state_res_debug",
						event_id = %incoming_pdu.event_id,
						n_state = fetched_state.len(),
						"fetched state via /state_ids; proceeding with auth check"
					);
					state_at_event = Some(StateAtEvent::Resolved(fetched_state));
				},
				| Ok(None) | Err(_) => {
					// Check if prev_events are completely unknown — not in the
					// timeline AND not even stored as outliers. If they are, we
					// cannot determine the correct state-at-event. Mark as
					// rejected so the unreject path can re-evaluate later.
					//
					// Events whose prev_events reference KNOWN events (even
					// rejected outliers) can safely fall through to the current
					// room state fallback — the auth check will still reject
					// invalid events.
					let any_prev_unknown = futures::stream::iter(incoming_pdu.prev_events())
						.any(|prev_id| async move {
							self.services.timeline.get_pdu_id(prev_id).await.is_err()
								&& self
									.services
									.outlier
									.get_pdu_outlier(prev_id)
									.await
									.is_err()
						})
						.await;

					if any_prev_unknown {
						info!(
							event_id = %incoming_pdu.event_id,
							"Rejecting event: prev_events completely unknown and /state_ids fetch failed"
						);
						self.services.pdu_metadata.mark_event_rejected(
							incoming_pdu.event_id(),
							"prev_events unknown and /state_ids fetch failed",
						);
						return Ok(StateAtEvent::Resolved(HashMap::new()));
					}

					// All prev_events exist but state hashes not computed — safe to
					// fall back to current room state for the auth check.
					debug!(
						event_id = %incoming_pdu.event_id,
						"fetch_state failed but prev_events present; falling back to current room state"
					);
				},
			}
		}
	}

	if state_at_event.is_none() {
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

		state_at_event = Some(StateAtEvent::Resolved(current_state));
	}

	Ok(state_at_event.unwrap())
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
	state_at_incoming_event: StateAtEvent,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<Option<HashSetCompressStateEvent>> {
	let current_shortstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.ok();

	if incoming_pdu.state_key().is_none() {
		// Just a normal message, state hasn't diverged: fast path out.
		let state_at_hash = match &state_at_incoming_event {
			| StateAtEvent::FastForward(ssh) => Some(*ssh),
			| StateAtEvent::Compressed(_) | StateAtEvent::Resolved(_) => None, /* We have to
			                                                                    * compress to
			                                                                    * get the hash */
		};

		if let Some(ssh) = state_at_hash {
			if Some(ssh) == current_shortstatehash {
				return Ok(None);
			}
		}
	} else {
		info!("Event is a state-event. Deriving new room state");
	}

	let new_room_state = match state_at_incoming_event {
		| StateAtEvent::FastForward(shortstatehash) => {
			info!("Fast-forward state update, skipping state resolution and map expansion");
			let mut current_state_compressed = self
				.services
				.state_compressor
				.load_shortstatehash_info(shortstatehash)
				.await?
				.pop()
				.expect("must have frame")
				.full_state
				.expect("must have full_state")
				.as_ref()
				.clone();

			if let Some(state_key) = incoming_pdu.state_key() {
				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(
						&incoming_pdu.kind().to_string().into(),
						state_key,
					)
					.await;

				let shorteventid = self
					.services
					.short
					.get_or_create_shorteventid(incoming_pdu.event_id())
					.await;

				if let Ok(old_shorteventid) = self
					.services
					.state_accessor
					.state_get_shortid(
						shortstatehash,
						&incoming_pdu.kind().to_string().into(),
						state_key,
					)
					.await
				{
					let old_compressed = crate::rooms::state_compressor::compress_state_event(
						shortstatekey,
						old_shorteventid,
					);
					current_state_compressed.remove(&old_compressed);
				}

				let new_compressed = crate::rooms::state_compressor::compress_state_event(
					shortstatekey,
					shorteventid,
				);
				current_state_compressed.insert(new_compressed);
			}

			Arc::new(current_state_compressed)
		},
		| StateAtEvent::Compressed(state_after) => {
			let mut state_after = state_after.clone();
			if let Some(state_key) = incoming_pdu.state_key() {
				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(
						&incoming_pdu.kind().to_string().into(),
						state_key,
					)
					.await;
				let shorteventid = self
					.services
					.short
					.get_or_create_shorteventid(incoming_pdu.event_id())
					.await;

				let state_after_mut: &mut std::collections::BTreeSet<[u8; 16]> =
					Arc::make_mut(&mut state_after);
				let old_compressed = state_after_mut
					.iter()
					.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
					.copied();
				if let Some(old) = old_compressed {
					state_after_mut.remove(&old);
				}
				state_after_mut.insert(crate::rooms::state_compressor::compress_state_event(
					shortstatekey,
					shorteventid,
				));
			}
			state_after
		},
		| StateAtEvent::Resolved(state_after) => {
			let mut state_after = state_after.clone();
			if let Some(state_key) = incoming_pdu.state_key() {
				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(
						&incoming_pdu.kind().to_string().into(),
						state_key,
					)
					.await;

				let event_id = incoming_pdu.event_id();
				state_after.insert(shortstatekey, event_id.to_owned());
			}

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
		},
	};

	// Save the resolved state delta into the database (safe to do concurrently)
	debug!("Compressing new room state");
	let state_delta = self
		.services
		.state_compressor
		.save_state(room_id, new_room_state)
		.await?;

	// If the state delta is empty (no added/removed events), we can fast-path out
	// without taking the room state lock and churning caches, UNLESS the state hash
	// shifted (i.e., if we resolved from multiple parents to an existing hash).
	if state_delta.added.is_empty()
		&& state_delta.removed.is_empty()
		&& Some(state_delta.shortstatehash) == current_shortstatehash
	{
		return Ok(None);
	}

	Ok(Some(state_delta))
}
