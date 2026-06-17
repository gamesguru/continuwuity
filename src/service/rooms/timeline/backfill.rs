use std::iter::once;

use conduwuit::{Err, Error, PduEvent, RoomVersion};
use conduwuit_core::{
	Result, debug, debug_warn, err, implement, info,
	matrix::{
		event::Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	utils::{IterStream, ReadyExt, stream::WidebandExt},
	validated, warn,
};
use futures::{FutureExt, StreamExt};
use ruma::{
	CanonicalJsonObject, EventId, Int, RoomId, ServerName,
	api::federation,
	events::{
		StateEventType, TimelineEventType,
		room::{create::RoomCreateEventContent, power_levels::RoomPowerLevelsEventContent},
	},
	uint,
};
use serde_json::value::RawValue as RawJsonValue;

use super::ExtractBody;

#[implement(super::Service)]
#[tracing::instrument(name = "backfill", level = "trace", skip(self))]
pub async fn backfill_if_required(
	&self,
	room_id: &RoomId,
	from: PduCount,
	limit: usize,
) -> Result<()> {
	if self
		.services
		.state_cache
		.room_joined_count(room_id)
		.await
		.is_ok_and(|count| count <= 1)
		&& !self
			.services
			.state_accessor
			.is_world_readable(room_id)
			.await
	{
		// Room is empty (1 user or none), there is no one that can backfill
		debug_warn!("Room {room_id} is empty, skipping backfill");
		return Ok(());
	}

	let mut backwards_extremities = Vec::new();
	let mut pdus = self
		.pdus_rev(room_id, Some(from.saturating_inc(ruma::api::Direction::Forward)))
		.take(limit)
		.boxed();
	while let Some(Ok((_, pdu))) = pdus.next().await {
		for prev_event_id in &pdu.prev_events {
			if self.get_pdu_id(prev_event_id).await.is_err() {
				backwards_extremities.push(pdu.event_id.clone());
				break;
			}
		}
	}

	if backwards_extremities.is_empty() {
		// No gaps found in this chunk, no backfill required
		return Ok(());
	}

	let power_levels: RoomPowerLevelsEventContent = self
		.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
		.await
		.unwrap_or_default();
	let create_event_content: RoomCreateEventContent = self
		.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
		.await?;
	let create_event = self
		.services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.await?;

	let room_version =
		RoomVersion::new(&create_event_content.room_version).expect("supported room version");
	let mut users = power_levels.users.clone();
	if room_version.explicitly_privilege_room_creators {
		users.insert(create_event.sender().to_owned(), Int::MAX);
		if let Some(additional_creators) = &create_event_content.additional_creators {
			for user_id in additional_creators {
				users.insert(user_id.to_owned(), Int::MAX);
			}
		}
	}

	let room_mods = users.iter().filter_map(|(user_id, level)| {
		let remote_powered =
			level > &power_levels.users_default && !self.services.globals.user_is_local(user_id);
		let creator = if room_version.explicitly_privilege_room_creators {
			create_event.sender() == user_id
				|| create_event_content
					.additional_creators
					.as_ref()
					.is_some_and(|c| c.contains(user_id))
		} else {
			false
		};

		if remote_powered || creator {
			debug!(%remote_powered, %creator, "User {user_id} can backfill in room {room_id}");
			Some(user_id.server_name())
		} else {
			debug!(%remote_powered, %creator, "User {user_id} cannot backfill in room {room_id}");
			None
		}
	});

	let canonical_room_alias_server = once(
		self.services
			.state_accessor
			.get_canonical_alias(room_id)
			.await,
	)
	.filter_map(Result::ok)
	.map(|alias| alias.server_name().to_owned())
	.stream();

	let mut servers = room_mods
		.stream()
		.map(ToOwned::to_owned)
		.chain(canonical_room_alias_server)
		.chain(
			self.services
				.server
				.config
				.trusted_servers
				.iter()
				.map(ToOwned::to_owned)
				.stream(),
		)
		.chain(
			self.services
				.state_cache
				.room_servers(room_id)
				.map(ToOwned::to_owned),
		)
		.ready_filter(|server_name| {
			!self.services.globals.server_is_ours(server_name)
				&& !self
					.services
					.server
					.config
					.forbidden_remote_server_names
					.is_match(server_name.host())
		})
		.wide_filter_map(|server_name| async move {
			self.services
				.state_cache
				.server_in_room(&server_name, room_id)
				.await
				.then_some(server_name)
		})
		.boxed();

	let mut federated_room = false;

	while let Some(ref backfill_server) = servers.next().await {
		if !self.services.globals.server_is_ours(backfill_server) {
			federated_room = true;
		}
		info!(
			"Asking {backfill_server} for backfill in {room_id} (extremities: \
			 {backwards_extremities:?})"
		);
		let response = self
			.services
			.sending
			.send_federation_request(
				backfill_server,
				federation::backfill::get_backfill::v1::Request {
					room_id: room_id.to_owned(),
					v: backwards_extremities.clone(),
					limit: uint!(100),
				},
			)
			.await;
		match response {
			| Ok(response) => {
				let pdus = response.pdus;
				// Handle timeline events newest-first (maintain timeline integrity)
				for pdu in pdus {
					if let Err(e) = self.backfill_pdu(backfill_server, pdu, None).boxed().await {
						debug_warn!("Failed to add backfilled pdu in room {room_id}: {e}");
					}
				}
				return Ok(());
			},
			| Err(ref e) => {
				// If the server explicitly forbids us, drop it from candidates
				if matches!(e, Error::Federation(_, _)) && e.to_string().contains("not allowed") {
					info!("{backfill_server} forbade backfill for {room_id}, skipping");
					continue;
				}
				warn!("{backfill_server} failed to provide backfill for room {room_id}: {e}");
			},
		}
	}

	if federated_room {
		warn!("No servers could backfill, but backfill was needed in room {room_id}");
	}
	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(name = "get_remote_pdu", level = "debug", skip(self))]
pub async fn get_remote_pdu(&self, room_id: &RoomId, event_id: &EventId) -> Result<PduEvent> {
	let _mutex = self.mutex_fetch.lock(event_id).await;

	let local = self.get_pdu(event_id).await;
	if local.is_ok() {
		// We already have this PDU, no need to backfill
		debug!("We already have {event_id} in {room_id}, no need to backfill.");
		return local;
	}
	debug!("Preparing to fetch event {event_id} in room {room_id} from remote servers.");
	// Similar to backfill_if_required, but only for a single PDU
	// Fetch a list of servers to try
	if self
		.services
		.state_cache
		.room_joined_count(room_id)
		.await
		.is_ok_and(|count| count <= 1)
		&& !self
			.services
			.state_accessor
			.is_world_readable(room_id)
			.await
	{
		// Room is empty (1 user or none), there is no one that can backfill
		return Err!(Request(NotFound("No one can backfill this PDU, room is empty.")));
	}

	let power_levels: RoomPowerLevelsEventContent = self
		.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
		.await
		.unwrap_or_default();

	let room_mods = power_levels.users.iter().filter_map(|(user_id, level)| {
		if level > &power_levels.users_default && !self.services.globals.user_is_local(user_id) {
			Some(user_id.server_name())
		} else {
			None
		}
	});

	let canonical_room_alias_server = once(
		self.services
			.state_accessor
			.get_canonical_alias(room_id)
			.await,
	)
	.filter_map(Result::ok)
	.map(|alias| alias.server_name().to_owned())
	.stream();
	let mut servers = room_mods
		.stream()
		.map(ToOwned::to_owned)
		.chain(canonical_room_alias_server)
		.chain(
			self.services
				.server
				.config
				.trusted_servers
				.iter()
				.map(ToOwned::to_owned)
				.stream(),
		)
		.chain(
			self.services
				.state_cache
				.room_servers(room_id)
				.map(ToOwned::to_owned),
		)
		.ready_filter(|server_name| {
			!self.services.globals.server_is_ours(server_name)
				&& !self
					.services
					.server
					.config
					.forbidden_remote_server_names
					.is_match(server_name.host())
		})
		.wide_filter_map(|server_name| async move {
			self.services
				.state_cache
				.server_in_room(&server_name, room_id)
				.await
				.then_some(server_name)
		})
		.boxed();

	while let Some(ref backfill_server) = servers.next().await {
		info!("Asking {backfill_server} for event {}", event_id);
		let value = self
			.services
			.sending
			.send_federation_request(backfill_server, federation::event::get_event::v1::Request {
				event_id: event_id.to_owned(),
				include_unredacted_content: Some(false),
			})
			.await
			.and_then(|response| {
				serde_json::from_str::<CanonicalJsonObject>(response.pdu.get()).map_err(|e| {
					err!(BadServerResponse(debug_warn!(
						"Error parsing incoming event {e:?} from {backfill_server}"
					)))
				})
			});
		let pdu = match value {
			| Ok(value) => match self
				.services
				.event_handler
				.handle_incoming_pdu(backfill_server, room_id, event_id, value, false, None)
				.boxed()
				.await
			{
				| Ok(_) => {
					debug!("Successfully backfilled {event_id} from {backfill_server}");
					Some(self.get_pdu(event_id).await)
				},
				| Err(e) => {
					warn!(
						"{backfill_server} provided an invalid PDU or failed state resolution \
						 for {event_id}: {e}"
					);

					if e.to_string().contains("Server was denied by room ACL") {
						return Err(e);
					}

					None
				},
			},
			| Err(e) => {
				warn!("{backfill_server} failed to provide backfill for room {room_id}: {e}");
				None
			},
		};
		if let Some(pdu) = pdu {
			debug!("Fetched {event_id} from {backfill_server}");
			return pdu;
		}
	}

	Err!("No servers could be used to fetch {} in {}.", room_id, event_id)
}

#[implement(super::Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub async fn backfill_pdu(
	&self,
	origin: &ServerName,
	pdu: Box<RawJsonValue>,
	count: Option<u64>,
) -> Result<()> {
	let (room_id, event_id, value) = self.services.event_handler.parse_incoming_pdu(&pdu).await?;

	// Lock so we cannot backfill the same pdu twice at the same time
	let mutex_lock = self
		.services
		.event_handler
		.mutex_federation
		.lock(&room_id)
		.await;

	// If the PDU already exists as a timeline event, check whether it needs
	// to be demoted from Normal to Backfilled. A concurrent federation /send
	// transaction can race with backfill and insert the event with a Normal
	// count; leaving it there breaks backward pagination ordering because
	// Normal events sort after all Backfilled events.
	if let Ok(existing_pdu_id) = self.get_pdu_id(&event_id).await {
		let existing_count = existing_pdu_id.pdu_count();
		if matches!(existing_count, PduCount::Backfilled(_)) {
			debug!("We already know {event_id} as backfilled, skipping");
			return Ok(());
		}
		// Normal count — demote to Backfilled below (don't skip)
		debug!(
			"Demoting {event_id} from Normal to Backfilled (federation /send raced with \
			 backfill)"
		);
		self.db
			.remove_from_timeline_by_id(&existing_pdu_id, &event_id);
	}

	// Backfill events come from a trusted /backfill response. We only need
	// signature verification + basic auth, not the full federation pipeline
	// (which fails when auth chain events aren't available locally — common
	// after a new join). If outlier processing fails (e.g. missing auth
	// events), fall back to storing the raw PDU directly.
	let room_version_id = self.services.state.get_room_version(&room_id).await?;

	let (pdu_event, json_value) = match self
		.services
		.event_handler
		.handle_outlier_pdu(
			origin,
			None::<&PduEvent>,
			&event_id,
			&room_id,
			value.clone(),
			false,
			false,
			Some(&room_version_id),
		)
		.await
	{
		| Ok(result) => result,
		| Err(e) => {
			// Missing auth events are expected during backfill (we don't have
			// the room's full history yet). Insert the raw PDU directly.
			debug!("handle_outlier_pdu failed for backfill event {event_id}, inserting raw: {e}");
			let mut raw = value;
			raw.insert(
				"event_id".to_owned(),
				ruma::CanonicalJsonValue::String(event_id.as_str().to_owned()),
			);
			let parsed: PduEvent =
				serde_json::from_value(serde_json::to_value(&raw).expect("valid json"))
					.map_err(|e| err!(Database("Bad backfill PDU {event_id}: {e}")))?;
			(parsed, raw)
		},
	};

	let shortroomid = self
		.services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let insert_lock = self.mutex_insert.lock(&room_id).await;

	let count: i64 = match count {
		| Some(count) => count.try_into()?,
		| None => self.services.globals.next_count()?.try_into()?,
	};

	let pdu_id: RawPduId = PduId {
		shortroomid,
		shorteventid: PduCount::Backfilled(validated!(0 - count)),
	}
	.into();

	// Insert pdu
	self.db
		.prepend_backfill_pdu(&pdu_id, &event_id, &json_value)
		.await;

	drop(insert_lock);

	if pdu_event.kind == TimelineEventType::RoomMessage {
		let content: ExtractBody = pdu_event.get_content()?;
		if let Some(body) = content.body {
			self.services.search.index_pdu(shortroomid, &pdu_id, &body);
		}
	}
	drop(mutex_lock);

	debug!("Prepended backfill pdu");
	Ok(())
}

/// Promote an outlier event into the visible timeline as a Normal PDU.
/// This skips all auth checks — the caller is responsible for ensuring
/// the event is valid (e.g. it came from a send_join response).
#[implement(super::Service)]
pub async fn promote_outlier(&self, room_id: &RoomId, event_id: &EventId) -> Result<()> {
	// Skip if already in timeline
	if self.get_pdu_id(event_id).await.is_ok() {
		return Ok(());
	}

	let value = self.services.outlier.get_outlier_pdu_json(event_id).await?;

	let pdu: PduEvent = serde_json::from_value(
		serde_json::to_value(&value).map_err(|e| err!(Database("Bad outlier JSON: {e:?}")))?,
	)
	.map_err(|e| err!(Database("Bad outlier PDU: {e:?}")))?;

	let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;

	let insert_lock = self.mutex_insert.lock(room_id).await;

	// Use backfill (negative) PDU count — these are historical events
	// that predate the join, not new forward events.
	let count: i64 = self.services.globals.next_count()?.try_into()?;

	let pdu_id: RawPduId = PduId {
		shortroomid,
		shorteventid: PduCount::Backfilled(validated!(0 - count)),
	}
	.into();

	self.db
		.prepend_backfill_pdu(&pdu_id, event_id, &value)
		.await;

	drop(insert_lock);

	if pdu.kind == TimelineEventType::RoomMessage {
		let content: ExtractBody = pdu.get_content()?;
		if let Some(body) = content.body {
			self.services.search.index_pdu(shortroomid, &pdu_id, &body);
		}
	}

	// Remove from outlier room index
	self.services.outlier.remove_outlier(event_id).await;

	Ok(())
}

/// Force-insert a PDU directly into the timeline, bypassing all auth checks.
/// The caller provides the already-parsed PDU and its canonical JSON.
/// Returns the assigned PduId on success.
#[implement(super::Service)]
pub async fn force_insert_pdu(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	pdu: &PduEvent,
	value: &CanonicalJsonObject,
	backfill: bool,
) -> Result<RawPduId> {
	// Skip if already in timeline
	if self.get_pdu_id(event_id).await.is_ok() {
		return Err!(Database("PDU {event_id} already in timeline"));
	}

	let shortroomid = self.services.short.get_or_create_shortroomid(room_id).await;

	let insert_lock = self.mutex_insert.lock(room_id).await;

	let count: u64 = self.services.globals.next_count()?;

	let (pdu_count, pdu_id) = if backfill {
		let count_i64: i64 = count.try_into()?;
		let pcount = PduCount::Backfilled(conduwuit_core::validated!(0 - count_i64));
		(pcount, RawPduId::from(PduId { shortroomid, shorteventid: pcount }))
	} else {
		let pcount = PduCount::Normal(count);
		(pcount, RawPduId::from(PduId { shortroomid, shorteventid: pcount }))
	};

	let mut value = value.clone();
	value.insert(
		"event_id".into(),
		ruma::CanonicalJsonValue::String(event_id.as_str().to_owned()),
	);

	if backfill {
		self.db
			.prepend_backfill_pdu(&pdu_id, event_id, &value)
			.await;
	} else {
		self.db.append_pdu(&pdu_id, pdu, &value, pdu_count).await;
	}

	drop(insert_lock);

	if pdu.kind == TimelineEventType::RoomMessage {
		let content: ExtractBody = pdu.get_content()?;
		if let Some(body) = content.body {
			self.services.search.index_pdu(shortroomid, &pdu_id, &body);
		}
	}

	Ok(pdu_id)
}
