use std::{
	collections::BTreeMap,
	time::{Duration, Instant},
};

use conduwuit::{
	Err, Event, PduEvent, Result, debug, debug_error, debug_info, debug_warn, defer, err, error,
	info, matrix::PartialPdu, result::DebugInspect, trace, utils::time::jitter, warn,
};
use futures::{
	FutureExt, StreamExt,
	future::{OptionFuture, try_join4},
};
use ruma::{
	CanonicalJsonValue, EventId, OwnedUserId, RoomId, ServerName, UserId,
	events::{
		TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use tokio::sync::mpsc;

use crate::rooms::timeline::{RawPduId, pdu_fits};

async fn should_rescind_invite(
	services: &crate::rooms::event_handler::Services,
	content: &mut BTreeMap<String, CanonicalJsonValue>,
	sender: &UserId,
	room_id: &RoomId,
) -> Result<Option<PduEvent>> {
	// We insert a placeholder event ID since we cannot calculate the real one here.
	content.insert("event_id".to_owned(), CanonicalJsonValue::String("$rescind".to_owned()));
	let pdu_event = serde_json::from_value::<PduEvent>(
		serde_json::to_value(&content).expect("CanonicalJsonObj is a valid JsonValue"),
	)
	.map_err(|e| err!("invalid PDU: {e}"))?;

	if pdu_event.room_id().is_none_or(|r| r != room_id)
		&& pdu_event.sender() != sender
		&& pdu_event.event_type() != &TimelineEventType::RoomMember
		&& pdu_event.state_key().is_none_or(|v| v == sender.as_str())
	{
		return Ok(None);
	}

	let target_user_id = UserId::parse(pdu_event.state_key().unwrap())?;
	if pdu_event
		.get_content::<RoomMemberEventContent>()?
		.membership
		!= MembershipState::Leave
	{
		return Ok(None);
	}

	let Ok(pending_invite_state) = services
		.state_cache
		.invite_state(&target_user_id, room_id)
		.await
	else {
		return Ok(None);
	};

	for event in pending_invite_state {
		if event
			.get_field::<String>("type")?
			.is_some_and(|t| t == "m.room.member")
			|| event
				.get_field::<OwnedUserId>("state_key")?
				.is_some_and(|s| s == *target_user_id)
			|| event
				.get_field::<OwnedUserId>("sender")?
				.is_some_and(|s| s == *sender)
			|| event
				.get_field::<RoomMemberEventContent>("content")?
				.is_some_and(|c| c.membership == MembershipState::Invite)
		{
			return Ok(Some(pdu_event));
		}
	}

	Ok(None)
}

impl super::Service {
	/// Handles an incoming PDU from federation.
	///
	/// First checks that we want to receive this PDU. If we already have it as
	/// a timeline PDU, or we don't want to receive the PDU (e.g. origin ACL'd,
	/// room disabled/unknown), abort.
	///
	/// The PDU is then handled as an outlier event, which performs [PDU checks]
	/// 1 through 4. See: `handle_outlier_pdu`.
	///
	/// Once handled as an outlier, any missing prev events are fetched, and
	/// then the PDU will be promoted/upgraded from an outlier to a timeline
	/// event clients can see. See: `upgrade_outlier_to_timeline_pdu`. After
	/// this finishes, the PDU is either accepted or left as an outlier.
	///
	/// If the PDU is successfully upgraded, the remaining extremity count of
	/// the room is checked. If there are a potentially problematic number of
	/// forward extremities, a squasher task is started with a debounce period,
	/// which will eventually send a dummy event that ties up as many DAG forks
	/// as possible.
	///
	/// [PDU checks]: https://spec.matrix.org/v1.19/server-server-api/#checks-performed-on-receipt-of-a-pdu
	#[tracing::instrument(
		name = "pdu",
		skip_all,
		fields(%room_id, %event_id),
	)]
	pub async fn handle_incoming_pdu<'a>(
		&self,
		origin: &'a ServerName,
		room_id: &'a RoomId,
		event_id: &'a EventId,
		value: BTreeMap<String, CanonicalJsonValue>,
		is_backfilled_event: bool,
	) -> Result<Option<RawPduId>> {
		// Skip the PDU if we already have it as a timeline event. We still re-process
		// outliers in this scenario.
		if let Ok(pdu_id) = self.services.timeline.get_pdu_id(event_id).await {
			debug!("Database hit for incoming PDU, skipping processing");
			return Ok(Some(pdu_id));
		}
		trace!(
			"processing incoming PDU from {origin} for room {room_id} with event id {event_id}"
		);

		// 1.1 Check we even know about the room
		let meta_exists = self.services.metadata.exists(room_id).map(Ok);

		// 1.2 Check if the room is disabled
		let is_disabled = self.services.metadata.is_disabled(room_id).map(Ok);

		// 1.3.1 Check room ACL on origin field/server
		let origin_acl_check = self.acl_check(origin, room_id);

		// 1.3.2 Check room ACL on sender's server name
		let sender: OwnedUserId = value
			.get("sender")
			.and_then(|v| v.as_str())
			.ok_or_else(|| err!("No sender in object"))
			.and_then(|v| Ok(UserId::parse(v)?))
			.map_err(|e| err!(Request(BadJson("PDU does not have a valid sender key: {e}"))))?;

		let sender_acl_check: OptionFuture<_> = sender
			.server_name()
			.ne(origin)
			.then(|| self.acl_check(sender.server_name(), room_id))
			.into();

		let (meta_exists, is_disabled, (), ()) = try_join4(
			meta_exists,
			is_disabled,
			origin_acl_check,
			sender_acl_check.map(|o| o.unwrap_or(Ok(()))),
		)
		.await
		.inspect_err(
			|e| debug_error!(%origin, "failed to handle incoming PDU {event_id}: {e}"),
		)?;

		if is_disabled {
			return Err!(Request(Forbidden(
				"Federation of this room is disabled by this server."
			)));
		}

		if !self
			.services
			.state_cache
			.server_in_room(self.services.globals.server_name(), room_id)
			.await
		{
			let is_room_member_event =
				value.get("type").and_then(|t| t.as_str()) == Some("m.room.member");

			// Is this a federated invite rescind?
			// copied from https://github.com/element-hq/synapse/blob/7e4588a/synapse/handlers/federation_event.py#L255-L300
			if is_room_member_event {
				if let Some(pdu) =
					should_rescind_invite(&self.services, &mut value.clone(), &sender, room_id)
						.await?
				{
					debug_info!(
						"Invite to {room_id} appears to have been rescinded by {sender}, \
						 marking as left"
					);
					self.services
						.state_cache
						.mark_as_left(&sender, room_id, Some(pdu))
						.await;
					return Ok(None);
				}
			}

			if meta_exists && is_room_member_event {
				info!(
					%origin,
					%room_id,
					"Accepting inbound membership PDU for known room before participation cache catches up"
				);
			} else {
				info!(
					%origin,
					%room_id,
					"Dropping inbound PDU for room we aren't participating in"
				);
				return Err!(Request(NotFound("This server is not participating in that room.")));
			}
		}

		if !meta_exists {
			return Err!(Request(NotFound("Room is unknown to this server")));
		}

		// Fetch create event
		let create_event = &self
			.services
			.state_accessor
			.get_room_create_event(room_id)
			.await;

		let start_time = Instant::now();
		self.federation_handletime
			.write()
			.insert(room_id.into(), (event_id.to_owned(), start_time));

		defer! {{
			self.federation_handletime
				.write()
				.remove(room_id);
		}}

		let (incoming_pdu, val) = self
			.handle_outlier_pdu(origin, create_event, event_id, room_id, value)
			.await?;

		// If this event is being processed as part of backfill, we don't want to end up
		// *appending* it during the upgrade process, so we return early.
		if is_backfilled_event {
			debug!("Not promoting incoming event as it is being backfilled");
			return Ok(None);
		}

		// Skip events sent before we joined (they need to be persisted as backfilled
		// events, not timeline events, which is handled elsewhere).
		let first_ts_in_room = self
			.services
			.timeline
			.first_pdu_in_room(room_id)
			.await?
			.origin_server_ts();
		if incoming_pdu.origin_server_ts() < first_ts_in_room {
			debug_warn!(
				"Not promoting incoming event as it is sent before we joined the room (but was \
				 not backfilled)"
			);
			return Ok(None);
		}

		// Fetch any missing prev events doing all checks listed here starting at 1.
		// These are timeline events.
		// TODO: This part needs to be done in a background queue somewhere.

		debug!("Fetching and persisting any missing prev events");
		Box::pin(self.fetch_prevs(
			room_id,
			create_event,
			&incoming_pdu,
			origin,
			first_ts_in_room,
		))
		.await
		.inspect_err(|e| {
			debug_error!("Failed to fetch and persist incoming event's prev_events: {e:?}");
		})?;

		let is_dummy_event = incoming_pdu.event_type().to_string() == "org.matrix.dummy_event"
			&& incoming_pdu.state_key().is_none();

		// Done with prev events, now we can handle promoting the PDU
		let pdu_id = Box::pin(self.upgrade_outlier_to_timeline_pdu(
			incoming_pdu,
			val,
			create_event,
			origin,
			room_id,
		))
		.await?;

		let extremities_count = self
			.services
			.state
			.get_forward_extremities(room_id)
			.count()
			.await;

		self.maybe_squash_extremities(room_id, extremities_count, is_dummy_event)
			.await;

		Ok(pdu_id)
	}

	/// Conditionally starts an extremity squasher. If there is no waiting
	/// extremity squasher, a new one is created. Otherwise, the existing one is
	/// pinged.
	async fn maybe_squash_extremities(
		&self,
		room_id: &RoomId,
		extremities_count: usize,
		is_dummy_event: bool,
	) {
		let (tx, fut) = {
			if let Some(tx) = self.extremity_squashers.read().get(room_id)
				&& !tx.is_closed()
			{
				(tx.clone(), None)
			} else {
				let mut map = self.extremity_squashers.upgradable_read();

				if let Some(tx) = map.get(room_id)
					&& !tx.is_closed()
				{
					(tx.clone(), None)
				} else {
					let (tx, rx) = mpsc::channel(100);
					map.with_upgraded(|map| map.insert(room_id.to_owned(), tx.clone()));

					(tx, Some(self.spawn_squasher(room_id, rx)))
				}
			}
		};

		if let Some(fut) = fut {
			fut.await;
		}
		let _ = tx.try_send((extremities_count, is_dummy_event));
	}

	/// Spawns an extremity squasher with the given room and receiver channel.
	async fn spawn_squasher(&self, room_id: &RoomId, mut rx: mpsc::Receiver<(usize, bool)>) {
		let Some(service) = self.me.upgrade() else {
			return;
		};
		let room_id = room_id.to_owned();

		self.services.server.runtime().spawn(async move {
			let mut latest_extremity_count = None;
			let mut non_dummy_event = false;

			let mut closing = false;

			let waker = tokio::time::sleep(jitter(Duration::from_mins(2), -25.0..=25.0));
			tokio::pin!(waker);

			loop {
				tokio::select! {
					msg = rx.recv() => {
						if let Some((extremities_count, is_dummy_event)) = msg {
							latest_extremity_count = Some(extremities_count);
							non_dummy_event = non_dummy_event || !is_dummy_event;
							let sleep_duration = if extremities_count >= 20 {
								// Skip the original sleep duration and send in the next 3-7 seconds as the number of extremities has grown beyond what one squash can reasonably reduce. We still jitter here in case we receive more events in that time that reduce the number anyway, and to account for other servers sending the same squashes.
								jitter(Duration::from_secs(5), -50.0..=50.0)
							} else {
								jitter(Duration::from_mins(1), -50.0..=50.0)
							};
							#[allow(clippy::arithmetic_side_effects)]
							waker.as_mut().reset(tokio::time::Instant::now() + sleep_duration);
						} else {
							{let mut map = service.extremity_squashers.write();
							if let Some(tx) = map.get(&room_id) && tx.is_closed() {
								map.remove(&room_id);
							}}

							if let Some(count) = latest_extremity_count {
								if non_dummy_event && count >= service.services.server.config.dummy_event_threshold.into() {
									Self::squash_extremities(&service, &room_id, count).await;
								}
							}
							break;
						}
					}
					() = &mut waker, if !closing => {
						if let Some(count) = latest_extremity_count {
							if non_dummy_event && count >= service.services.server.config.dummy_event_threshold.into() {
								Self::squash_extremities(&service, &room_id, count).await;
							}
							latest_extremity_count = None;
							non_dummy_event = false;
							#[allow(clippy::arithmetic_side_effects)]
							waker.as_mut().reset(tokio::time::Instant::now() + Duration::from_mins(2));
						} else {
							rx.close();
							closing = true;
						}
					}
					() = service.server_shutdown.notified(), if !closing => {
						rx.close();
						closing = true;
					}
				}
			}
		});
	}

	/// Squashes extremities in a room by sending dummy events (empty events
	/// that are hidden from clients) to the room. It will only send ONE dummy
	/// event to squash. If there are more than 20 extremities, multiple calls
	/// to `squash_extremities` will be required.
	/// Sending the dummy event will be attempted by iterating over each local
	/// user currently joined to the room (including deactivated users) until
	/// either one of them successfully builds and appends a dummy event PDU, or
	/// there are no more users to try.
	async fn squash_extremities(&self, room_id: &RoomId, extremities_count: usize) {
		debug_warn!(
			%extremities_count,
			threshold=%self.services.server.config.dummy_event_threshold,
			"Attempting to squash extremities after upgrading pdu"
		);
		// Try to send a dummy event to squash extremities. See issue #1844
		let power_levels = self
			.services
			.state_accessor
			.get_room_power_levels(room_id)
			.await;
		let mut local_users = self.services.state_cache.local_users_in_room(room_id);
		while let Some(user_id) = local_users.next().await {
			if !power_levels.user_can_send_message(&user_id, "org.matrix.dummy_event".into()) {
				trace!(%user_id, "user does not have power level to send dummy event, skipping");
				continue;
			}
			let state_lock = self.services.state.mutex.lock(room_id).await;
			if self
				.services
				.timeline
				.build_and_append_pdu(
					PartialPdu {
						event_type: "org.matrix.dummy_event".into(),
						..PartialPdu::default()
					},
					&user_id,
					Some(room_id),
					&state_lock,
				)
				.await
				.inspect(|_| debug!(sender=%user_id, "Successfully sent a dummy event"))
				.inspect_err(
					|e| debug!(sender=%user_id, ?e, "Failed to send a dummy event via user"),
				)
				.is_ok()
			{
				return;
			}
		}
		debug_warn!("Unable to squash extremities using any local user");
	}
}
