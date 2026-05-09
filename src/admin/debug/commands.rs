use std::{
	collections::{HashMap, HashSet},
	fmt::Write,
	iter::once,
	time::{Instant, SystemTime},
};

use conduwuit::{
	Err, Result, debug_error, err, info,
	matrix::{
		Event,
		pdu::{PduEvent, PduId, RawPduId},
	},
	trace, utils,
	utils::{
		stream::{IterStream, ReadyExt},
		string::EMPTY,
	},
	warn,
};
use futures::{FutureExt, StreamExt, TryStreamExt};
use lettre::message::Mailbox;
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName,
	OwnedUserId, RoomVersionId,
	api::federation::event::{get_event, get_room_state},
	events::{AnyStateEvent, StateEventType},
	serde::Raw,
};
use service::rooms::{
	short::{ShortEventId, ShortRoomId},
	state_compressor::HashSetCompressStateEvent,
};
use tracing_subscriber::EnvFilter;

use crate::admin_command;

#[admin_command]
pub(super) async fn echo(&self, message: Vec<String>) -> Result {
	let message = message.join(" ");
	self.write_str(&message).await
}

#[admin_command]
pub(super) async fn get_auth_chain(&self, event_id: OwnedEventId) -> Result {
	let Ok(event) = self.services.rooms.timeline.get_pdu(&event_id).await else {
		return Err!("Event not found.");
	};

	let room_id = event
		.room_id_or_hash()
		.ok_or_else(|| err!(Database("Event has no room_id")))?;

	let start = Instant::now();
	let count = self
		.services
		.rooms
		.auth_chain
		.event_ids_iter(&room_id, once(event_id.as_ref()))
		.ready_filter_map(Result::ok)
		.count()
		.await;

	let elapsed = start.elapsed();
	let out = format!("Loaded auth chain with length {count} in {elapsed:?}");

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn parse_pdu(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&EMPTY).trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.");
	}

	let string = self.body[1..self.body.len().saturating_sub(1)].join("\n");
	match serde_json::from_str(&string) {
		| Err(e) => return Err!("Invalid json in command body: {e}"),
		| Ok(value) => match ruma::signatures::reference_hash(&value, &RoomVersionId::V6) {
			| Err(e) => return Err!("Could not parse PDU JSON: {e:?}"),
			| Ok(hash) => {
				let event_id = OwnedEventId::parse(format!("${hash}"));
				match serde_json::from_value::<PduEvent>(serde_json::to_value(value)?) {
					| Err(e) => return Err!("EventId: {event_id:?}\nCould not parse event: {e}"),
					| Ok(pdu) => write!(self, "EventId: {event_id:?}\n{pdu:#?}"),
				}
			},
		},
	}
	.await
}

#[admin_command]
pub(super) async fn get_pdu(&self, event_id: OwnedEventId) -> Result {
	let in_timeline = self
		.services
		.rooms
		.timeline
		.get_pdu_id(&event_id)
		.await
		.is_ok();
	let in_outlier = self
		.services
		.rooms
		.outlier
		.get_pdu_outlier(&event_id)
		.await
		.is_ok();

	if !in_timeline && !in_outlier {
		return Err!("PDU not found locally.");
	}

	let pdu_json = self.services.rooms.timeline.get_pdu_json(&event_id).await?;
	let text = serde_json::to_string_pretty(&pdu_json)?;

	let mut status = String::new();
	if in_timeline && in_outlier {
		status.push_str("STUCK STATE (Both Timeline and Outlier tables)");
	} else if in_timeline {
		status.push_str("Timeline PDU");
	} else {
		let soft_failed = self
			.services
			.rooms
			.pdu_metadata
			.is_event_soft_failed(&event_id)
			.await;
		if soft_failed {
			status.push_str("Outlier (Soft Failed / Rejected) PDU");
		} else {
			status.push_str("Outlier PDU");
		}
	}

	let out = format!("Status: {status}\n\n```json\n{text}\n```");
	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn get_short_pdu(
	&self,
	shortroomid: ShortRoomId,
	shorteventid: ShortEventId,
) -> Result {
	let pdu_id: RawPduId = PduId {
		shortroomid,
		shorteventid: shorteventid.into(),
	}
	.into();

	let pdu_json = self
		.services
		.rooms
		.timeline
		.get_pdu_json_from_id(&pdu_id)
		.await;

	match pdu_json {
		| Err(_) => return Err!("PDU not found locally."),
		| Ok(json) => {
			let json_text = serde_json::to_string_pretty(&json)?;
			write!(self, "```json\n{json_text}\n```")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn get_remote_pdu_list(&self, server: OwnedServerName, force: bool) -> Result {
	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.",);
	}

	if server == self.services.globals.server_name() {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs from the database.",
		);
	}

	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&EMPTY).trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let list = self
		.body
		.iter()
		.collect::<Vec<_>>()
		.drain(1..self.body.len().saturating_sub(1))
		.filter_map(|pdu| EventId::parse(pdu).ok())
		.collect::<Vec<_>>();

	let mut failed_count: usize = 0;
	let mut success_count: usize = 0;

	for event_id in list {
		if force {
			match self
				.get_remote_pdu(event_id.to_owned(), server.clone())
				.await
			{
				| Err(e) => {
					failed_count = failed_count.saturating_add(1);
					self.services
						.admin
						.send_text(&format!("Failed to get remote PDU, ignoring error: {e}"))
						.await;

					warn!("Failed to get remote PDU, ignoring error: {e}");
				},
				| _ => {
					success_count = success_count.saturating_add(1);
				},
			}
		} else {
			self.get_remote_pdu(event_id.to_owned(), server.clone())
				.await?;
			success_count = success_count.saturating_add(1);
		}
	}

	let out =
		format!("Fetched {success_count} remote PDUs successfully with {failed_count} failures");

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn get_remote_pdu(
	&self,
	event_id: OwnedEventId,
	server: OwnedServerName,
) -> Result {
	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.");
	}

	if server == self.services.globals.server_name() {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs.",
		);
	}

	match self
		.services
		.sending
		.send_federation_request(&server, get_event::v1::Request::new(event_id.clone(), None))
		.await
	{
		| Err(e) => {
			return Err!(
				"Remote server did not have PDU or failed sending request to remote server: {e}"
			);
		},
		| Ok(response) => {
			let json: CanonicalJsonObject =
				serde_json::from_str(response.pdu.get()).map_err(|e| {
					warn!(
						"Requested event ID {event_id} from server but failed to convert from \
						 RawValue to CanonicalJsonObject (malformed event/response?): {e}"
					);
					err!(Request(Unknown(
						"Received response from server but failed to parse PDU"
					)))
				})?;

			trace!("Attempting to parse PDU: {:?}", &response.pdu);
			let _parsed_pdu = {
				let parsed_result = self
					.services
					.rooms
					.event_handler
					.parse_incoming_pdu(&response.pdu)
					.boxed()
					.await;

				let (event_id, value, room_id) = match parsed_result {
					| Ok(t) => t,
					| Err(e) => {
						warn!("Failed to parse PDU: {e}");
						info!("Full PDU: {:?}", &response.pdu);
						return Err!("Failed to parse PDU remote server {server} sent us: {e}");
					},
				};

				vec![(event_id, value, room_id)]
			};

			let text = serde_json::to_string_pretty(&json)?;
			let msg = "Got PDU from specified server:";
			write!(self, "{msg}. Event body:\n```json\n{text}\n```")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn get_room_state(&self, room: OwnedRoomOrAliasId) -> Result {
	self.bail_restricted()?;

	let room_id = self.services.rooms.alias.resolve(&room).await?;
	let room_state: Vec<Raw<AnyStateEvent>> = self
		.services
		.rooms
		.state_accessor
		.room_state_full_pdus(&room_id)
		.map_ok(Event::into_format)
		.try_collect()
		.await?;

	if room_state.is_empty() {
		return Err!("Unable to find room state in our database (vector is empty)",);
	}

	let json = serde_json::to_string_pretty(&room_state).map_err(|e| {
		err!(Database(
			"Failed to convert room state events to pretty JSON, possible invalid room state \
			 events in our database {e}",
		))
	})?;

	let out = format!("```json\n{json}\n```");
	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn ping(&self, server: OwnedServerName) -> Result {
	if server == self.services.globals.server_name() {
		return Err!("Not allowed to send federation requests to ourselves.");
	}

	let timer = tokio::time::Instant::now();

	match self
		.services
		.sending
		.send_federation_request(
			&server,
			ruma::api::federation::discovery::get_server_version::v1::Request {},
		)
		.await
	{
		| Err(e) => {
			return Err!("Failed sending federation request to specified server:\n\n{e}");
		},
		| Ok(response) => {
			let ping_time = timer.elapsed();
			let json_text_res = serde_json::to_string_pretty(&response.server);

			let out = if let Ok(json) = json_text_res {
				format!("Got response which took {ping_time:?} time:\n```json\n{json}\n```")
			} else {
				format!("Got non-JSON response which took {ping_time:?} time:\n{response:?}")
			};

			write!(self, "{out}")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn force_device_list_updates(&self) -> Result {
	// Force E2EE device list updates for all users
	self.services
		.users
		.stream()
		.for_each(|user_id| self.services.users.mark_device_key_update(user_id))
		.await;

	write!(self, "Marked all devices for all users as having new keys to update").await
}

#[admin_command]
pub(super) async fn change_log_level(&self, filter: Option<String>, reset: bool) -> Result {
	let handles = &["console"];

	if reset {
		let old_filter_layer = match EnvFilter::try_new(&self.services.server.config.log) {
			| Ok(s) => s,
			| Err(e) => return Err!("Log level from config appears to be invalid now: {e}"),
		};

		match self
			.services
			.server
			.log
			.reload
			.reload(&old_filter_layer, Some(handles))
		{
			| Err(e) => {
				return Err!("Failed to modify and reload the global tracing log level: {e}");
			},
			| Ok(()) => {
				let value = &self.services.server.config.log;
				let out = format!("Successfully changed log level back to config value {value}");
				return self.write_str(&out).await;
			},
		}
	}

	if let Some(filter) = filter {
		let new_filter_layer = match EnvFilter::try_new(filter) {
			| Ok(s) => s,
			| Err(e) => return Err!("Invalid log level filter specified: {e}"),
		};

		match self
			.services
			.server
			.log
			.reload
			.reload(&new_filter_layer, Some(handles))
		{
			| Ok(()) => {
				return self.write_str("Successfully changed log level").await;
			},
			| Err(e) => {
				return Err!("Failed to modify and reload the global tracing log level: {e}");
			},
		}
	}

	Err!("No log level was specified.")
}

#[admin_command]
pub(super) async fn verify_json(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&EMPTY).trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.");
	}

	let string = self.body[1..self.body.len().checked_sub(1).unwrap()].join("\n");
	match serde_json::from_str::<CanonicalJsonObject>(&string) {
		| Err(e) => return Err!("Invalid json: {e}"),
		| Ok(value) => match self.services.server_keys.verify_json(&value, None).await {
			| Err(e) => return Err!("Signature verification failed: {e}"),
			| Ok(()) => write!(self, "Signature correct"),
		},
	}
	.await
}

#[admin_command]
pub(super) async fn verify_pdu(&self, event_id: OwnedEventId) -> Result {
	use ruma::signatures::Verified;

	let mut event = self.services.rooms.timeline.get_pdu_json(&event_id).await?;

	event.remove("event_id");
	let msg = match self.services.server_keys.verify_event(&event, None).await {
		| Err(e) => return Err(e),
		| Ok(Verified::Signatures) => "signatures OK, but content hash failed (redaction).",
		| Ok(Verified::All) => "signatures and hashes OK.",
	};

	self.write_str(msg).await
}

#[admin_command]
#[tracing::instrument(skip(self), level = "info")]
pub(super) async fn first_pdu_in_room(&self, room_id: OwnedRoomId) -> Result {
	self.bail_restricted()?;

	if !self
		.services
		.rooms
		.state_cache
		.server_is_participant(&self.services.server.name, &room_id)
		.await
	{
		return Err!("We are not participating in the room / we don't know about the room ID.",);
	}

	let first_pdu = self
		.services
		.rooms
		.timeline
		.first_pdu_in_room(&room_id)
		.await
		.map_err(|_| err!(Database("Failed to find the first PDU in database")))?;

	let out = format!("{first_pdu:?}");
	self.write_str(&out).await
}

#[admin_command]
#[tracing::instrument(skip(self), level = "info")]
pub(super) async fn latest_pdu_in_room(&self, room_id: OwnedRoomId) -> Result {
	self.bail_restricted()?;

	if !self
		.services
		.rooms
		.state_cache
		.server_is_participant(&self.services.server.name, &room_id)
		.await
	{
		return Err!("We are not participating in the room / we don't know about the room ID.");
	}

	let latest_pdu = self
		.services
		.rooms
		.timeline
		.latest_pdu_in_room(&room_id)
		.await
		.map_err(|_| err!(Database("Failed to find the latest PDU in database")))?;

	let out = format!("{latest_pdu:?}");
	self.write_str(&out).await
}

#[admin_command]
#[tracing::instrument(skip(self), level = "info")]
pub(crate) async fn force_set_room_state_from_server(
	&self,
	room_id: OwnedRoomId,
	server_name: OwnedServerName,
	at_event: Option<OwnedEventId>,
	overwrite: bool,
	output: Option<String>,
) -> Result {
	self.bail_restricted()?;

	let at_event_id = match at_event {
		| Some(event_id) => event_id,
		| None => {
			if !self
				.services
				.rooms
				.state_cache
				.server_is_participant(&self.services.server.name, &room_id)
				.await
			{
				return Err!(Request(InvalidParam(
					"We are not participating in the room; provide an event_id to bootstrap \
					 (positional arg after server_name)."
				)));
			}
			self.services
				.rooms
				.timeline
				.latest_pdu_in_room(&room_id)
				.await
				.map_err(|_| err!(Database("Failed to find the latest PDU in database")))?
				.event_id()
				.to_owned()
		},
	};

	let room_version = self
		.services
		.rooms
		.state
		.get_room_version(&room_id)
		.await
		.unwrap_or(RoomVersionId::V11);

	let mut state: HashMap<u64, OwnedEventId> = HashMap::new();

	let at_event_id_clone = at_event_id.clone();
	let at_event_id_str = at_event_id.to_string();
	let remote_state_response = self
		.services
		.sending
		.send_federation_request(&server_name, get_room_state::v1::Request {
			room_id: room_id.clone(),
			event_id: at_event_id,
		})
		.await?;

	if let Some(ref path) = output {
		info!("Dumping federation state response to {path}");
		let dump = serde_json::json!({
			"room_id": room_id,
			"server_name": server_name,
			"event_id": at_event_id_str,
			"pdus": remote_state_response.pdus,
			"auth_chain": remote_state_response.auth_chain,
		});
		std::fs::write(path, serde_json::to_string_pretty(&dump).unwrap_or_default())
			.map_err(|e| err!(Database("Failed to write output file: {e:?}")))?;
		info!(
			"Dumped {} state PDUs and {} auth chain events",
			remote_state_response.pdus.len(),
			remote_state_response.auth_chain.len()
		);
	}

	for pdu in remote_state_response.pdus.clone() {
		match self
			.services
			.rooms
			.event_handler
			.parse_incoming_pdu(&pdu)
			.await
		{
			| Ok(t) => t,
			| Err(e) => {
				warn!("Could not parse PDU, ignoring: {e}");
				continue;
			},
		};
	}

	info!("Going through room_state response PDUs");
	for result in remote_state_response.pdus.iter().map(|pdu| {
		self.services
			.server_keys
			.validate_and_add_event_id(pdu, &room_version)
	}) {
		let Ok((event_id, value)) = result.await else {
			continue;
		};

		let pdu = PduEvent::from_id_val(&event_id, value.clone(), Some(room_id.as_ref()))
			.map_err(|e| {
				debug_error!(
					"Invalid PDU in fetching remote room state PDUs response: {value:#?}"
				);
				err!(BadServerResponse(debug_error!("Invalid PDU in send_join response: {e:?}")))
			})?;

		if pdu.room_id_or_hash().as_deref() != Some(room_id.as_ref()) {
			return Err!(BadServerResponse("Remote room_state PDU belongs to a different room"));
		}

		if let Ok(pdu_id) = self.services.rooms.timeline.get_pdu_id(&event_id).await {
			trace!(
				"PDU {event_id} already in timeline (pdu_id={pdu_id:?}), skipping outlier insert"
			);
		} else if self
			.services
			.rooms
			.outlier
			.get_outlier_pdu_json(&event_id)
			.await
			.is_ok()
		{
			trace!("PDU {event_id} already an outlier, skipping");
		} else {
			info!("PDU {event_id} NOT in timeline, adding as outlier");
			self.services
				.rooms
				.outlier
				.add_pdu_outlier(&event_id, &value, Some(&room_id));
		}

		if let Some(state_key) = &pdu.state_key {
			let shortstatekey = self
				.services
				.rooms
				.short
				.get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)
				.await;

			state.insert(shortstatekey, pdu.event_id.clone());
		}
	}

	// Federation /state returns state BEFORE the queried event. When the
	// at_event is a state event, inject it into the state map so force-set
	// includes its own state change (e.g. a join event for the local user).
	if let Ok(at_pdu) = self
		.services
		.rooms
		.timeline
		.get_pdu(&at_event_id_clone)
		.await
	{
		if let Some(state_key) = &at_pdu.state_key {
			let shortstatekey = self
				.services
				.rooms
				.short
				.get_or_create_shortstatekey(&at_pdu.kind.to_string().into(), state_key)
				.await;
			info!("Injecting at_event {at_event_id_clone} into state (state-after)");
			state.insert(shortstatekey, at_event_id_clone);
		}
	}

	info!("Going through auth_chain response");
	let mut auth_existing = 0_usize;
	let mut auth_added = 0_usize;
	for result in remote_state_response.auth_chain.iter().map(|pdu| {
		self.services
			.server_keys
			.validate_and_add_event_id(pdu, &room_version)
	}) {
		let Ok((event_id, value)) = result.await else {
			continue;
		};

		if self
			.services
			.rooms
			.timeline
			.get_pdu_id(&event_id)
			.await
			.is_ok() || self
			.services
			.rooms
			.outlier
			.get_outlier_pdu_json(&event_id)
			.await
			.is_ok()
		{
			auth_existing = auth_existing.saturating_add(1);
		} else {
			self.services
				.rooms
				.outlier
				.add_pdu_outlier(&event_id, &value, Some(&room_id));
			auth_added = auth_added.saturating_add(1);
		}
	}
	info!("Auth chain: {auth_added} added as outliers, {auth_existing} already in timeline");

	let new_room_state = if overwrite {
		info!("Resolving new room state (ABSOLUTE OVERRIDE)");
		let compressed: conduwuit_service::rooms::state_compressor::CompressedState = self
			.services
			.rooms
			.state_compressor
			.compress_state_events(state.iter().map(|(ssk, eid)| (ssk, (*eid).as_ref())))
			.collect()
			.await;
		std::sync::Arc::new(compressed)
	} else {
		info!("Resolving new room state (state-res)");
		match self
			.services
			.rooms
			.event_handler
			.resolve_state(&room_id, &room_version, state.clone())
			.await
		{
			| Ok(resolved) => resolved,
			| Err(_) => {
				info!("No prior state for room — using remote state directly (cold bootstrap)");
				let compressed: conduwuit_service::rooms::state_compressor::CompressedState =
					self.services
						.rooms
						.state_compressor
						.compress_state_events(
							state.iter().map(|(ssk, eid)| (ssk, (*eid).as_ref())),
						)
						.collect()
						.await;
				std::sync::Arc::new(compressed)
			},
		}
	};

	info!("Compressing new room state");
	let HashSetCompressStateEvent {
		shortstatehash: short_state_hash,
		added,
		removed,
	} = self
		.services
		.rooms
		.state_compressor
		.save_state(room_id.clone().as_ref(), new_room_state)
		.await?;

	let state_lock = self.services.rooms.state.mutex.lock(&*room_id).await;

	info!("Forcing new room state");
	Box::pin(self.services.rooms.state.force_state(
		room_id.clone().as_ref(),
		short_state_hash,
		added,
		removed,
		&state_lock,
	))
	.await?;

	// Set the tip event as the sole forward extremity. Previous behavior
	// scattered extremities across all state events, fracturing the DAG.
	// The state is already corrected by force_state above; extremities
	// should just point at the timeline tip.
	let tip_event_id = self
		.services
		.rooms
		.timeline
		.latest_pdu_in_room(&room_id)
		.await;
	if let Ok(tip_pdu) = tip_event_id {
		self.services
			.rooms
			.state
			.set_forward_extremities(room_id.as_ref(), once(tip_pdu.event_id()), &state_lock)
			.await;

		// NOTE: Do NOT update pdu_shortstatehash here. short_state_hash is
		// state-after, but pdu_shortstatehash must be state-before per spec.
		// The event's original pdu_shortstatehash from append is correct.
		info!("Set tip {} as sole extremity (room SSH {short_state_hash})", tip_pdu.event_id());
	} else {
		// No timeline events — /sync won't deliver this room.
		// Promote the most recent state event as a timeline anchor.
		Box::pin(self.promote_sync_anchor(&room_id, short_state_hash, &state_lock)).await;
	}

	Box::pin(self.rebuild_membership_cache(room_id.clone(), short_state_hash)).await;

	self.write_str("Successfully forced the room state from the requested remote server.")
		.await
}

/// Rebuild membership cache from a state snapshot. Extracted to keep
/// `force_set_room_state_from_server` below the stack-frame limit.
#[admin_command]
async fn rebuild_membership_cache(&self, room_id: OwnedRoomId, short_state_hash: u64) {
	use conduwuit::{info, warn};

	info!("Rebuilding membership cache from state snapshot for {room_id}");

	let mut state_joined: HashSet<OwnedUserId> = HashSet::new();
	let mut state_invited: HashSet<OwnedUserId> = HashSet::new();
	let mut members_updated = 0_usize;

	{
		let state_full = self
			.services
			.rooms
			.state_accessor
			.state_full(short_state_hash);

		futures::pin_mut!(state_full);
		while let Some(((event_type, state_key), pdu)) = state_full.next().await {
			if event_type != StateEventType::RoomMember {
				continue;
			}
			let Ok(user_id) = OwnedUserId::try_from(state_key.as_str()) else {
				continue;
			};

			let content = pdu.get_content_as_value();
			let membership = content
				.get("membership")
				.and_then(|v| v.as_str())
				.unwrap_or("leave");

			match membership {
				| "join" => {
					state_joined.insert(user_id.clone());
					if !self
						.services
						.rooms
						.state_cache
						.is_joined(&user_id, &room_id)
						.await
					{
						self.services
							.rooms
							.state_cache
							.mark_as_joined(&user_id, &room_id)
							.await;
						members_updated = members_updated.saturating_add(1);
					}
				},
				| "invite" => {
					state_invited.insert(user_id.clone());
					// TODO: check-before-write for invites
				},
				| "leave" | "ban" => {
					// TODO: distinguish left vs kicked vs banned for proper
					// Cinny/Element display. Currently all three map to
					// mark_as_left which loses the distinction.
					if self
						.services
						.rooms
						.state_cache
						.is_invited_or_joined(&user_id, &room_id)
						.await
					{
						self.services
							.rooms
							.state_cache
							.mark_as_left(&user_id, &room_id, None)
							.await;
						members_updated = members_updated.saturating_add(1);
					}
				},
				| unknown => {
					warn!("Unknown membership state '{unknown}' for {user_id} in {room_id}");
				},
			}
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
		if !state_joined.contains(user_id) {
			self.services
				.rooms
				.state_cache
				.mark_as_left(user_id, &room_id, None)
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
		if !state_invited.contains(user_id) {
			self.services
				.rooms
				.state_cache
				.mark_as_left(user_id, &room_id, None)
				.await;
			stale_removed = stale_removed.saturating_add(1);
		}
	}

	self.services
		.rooms
		.state_cache
		.update_joined_count(&room_id)
		.await;
	info!("Updated {members_updated} member entries, removed {stale_removed} stale caches");
}

/// Promote the most recent state event to the timeline as a Normal PDU,
/// giving `/sync` a positional cursor to deliver the room to clients.
#[admin_command]
async fn promote_sync_anchor(
	&self,
	room_id: &ruma::RoomId,
	short_state_hash: u64,
	state_lock: &conduwuit_service::rooms::state::RoomMutexGuard,
) {
	use conduwuit::matrix::Event;
	use futures::StreamExt;

	info!("No timeline events found; promoting anchor event for /sync visibility");

	let mut best: Option<(u64, OwnedEventId, PduEvent, CanonicalJsonObject)> = None;

	let anchor_candidates = self
		.services
		.rooms
		.state_accessor
		.state_full_pdus(short_state_hash);
	futures::pin_mut!(anchor_candidates);

	while let Some(pdu) = anchor_candidates.next().await {
		let ts: u64 = pdu.origin_server_ts().0.into();
		let eid = pdu.event_id().to_owned();
		if best.as_ref().is_none_or(|(best_ts, ..)| ts > *best_ts) {
			// Check both timeline AND outlier tables — force-set imports
			// state events into the outlier table, not timeline.
			let json_result = match self.services.rooms.timeline.get_pdu_json(&eid).await {
				| Ok(json) => Ok(json),
				| Err(_) => self.services.rooms.outlier.get_outlier_pdu_json(&eid).await,
			};
			if let Ok(json) = json_result {
				let pdu_owned: PduEvent =
					serde_json::from_value(serde_json::to_value(&json).unwrap_or_default())
						.unwrap_or_else(|_| panic!("Bad PDU JSON for {eid}"));
				best = Some((ts, eid, pdu_owned, json));
			}
		}
	}

	if let Some((_ts, anchor_id, anchor_pdu, anchor_json)) = best {
		match self
			.services
			.rooms
			.timeline
			.force_insert_pdu(room_id, &anchor_id, &anchor_pdu, &anchor_json)
			.await
		{
			| Ok(_pdu_id) => {
				self.services
					.rooms
					.state
					.set_forward_extremities(room_id, once(anchor_id.as_ref()), state_lock)
					.await;
				info!("Promoted {anchor_id} as timeline anchor for /sync");
			},
			| Err(e) => {
				warn!("Failed to promote anchor event {anchor_id}: {e}");
			},
		}
	} else {
		warn!("No state events found to promote as timeline anchor");
	}
}

#[admin_command]
pub(super) async fn get_signing_keys(
	&self,
	server_name: Option<OwnedServerName>,
	notary: Option<OwnedServerName>,
	query: bool,
) -> Result {
	let server_name = server_name.unwrap_or_else(|| self.services.server.name.clone());

	if let Some(notary) = notary {
		let signing_keys = self
			.services
			.server_keys
			.notary_request(&notary, &server_name)
			.await?;

		let out = format!("```rs\n{signing_keys:#?}\n```");
		return self.write_str(&out).await;
	}

	let signing_keys = if query {
		self.services
			.server_keys
			.server_request(&server_name)
			.await?
	} else {
		self.services
			.server_keys
			.signing_keys_for(&server_name)
			.await?
	};

	let out = format!("```rs\n{signing_keys:#?}\n```");
	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn get_verify_keys(&self, server_name: Option<OwnedServerName>) -> Result {
	let server_name = server_name.unwrap_or_else(|| self.services.server.name.clone());

	let keys = self
		.services
		.server_keys
		.verify_keys_for(&server_name)
		.await;

	let mut out = String::new();
	writeln!(out, "| Key ID | Public Key |")?;
	writeln!(out, "| --- | --- |")?;
	for (key_id, key) in keys {
		writeln!(out, "| {key_id} | {key:?} |")?;
	}

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn resolve_true_destination(
	&self,
	server_name: OwnedServerName,
	no_cache: bool,
) -> Result {
	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.",);
	}

	if server_name == self.services.server.name {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs.",
		);
	}

	let actual = self
		.services
		.resolver
		.resolve_actual_dest(&server_name, !no_cache)
		.await?;

	let msg = format!("Destination: {}\nHostname URI: {}", actual.dest, actual.host);
	self.write_str(&msg).await
}

#[admin_command]
pub(super) async fn memory_stats(&self, opts: Option<String>) -> Result {
	const OPTS: &str = "abcdefghijklmnopqrstuvwxyz";

	let opts: String = OPTS
		.chars()
		.filter(|&c| {
			let allow_any = opts.as_ref().is_some_and(|opts| opts == "*");

			let allow = allow_any || opts.as_ref().is_some_and(|opts| opts.contains(c));

			!allow
		})
		.collect();

	let stats = conduwuit::alloc::memory_stats(&opts).unwrap_or_default();

	self.write_str("```\n").await?;
	self.write_str(&stats).await?;
	self.write_str("\n```").await?;
	Ok(())
}

#[cfg(tokio_unstable)]
#[admin_command]
pub(super) async fn runtime_metrics(&self) -> Result {
	let out = self.services.server.metrics.runtime_metrics().map_or_else(
		|| "Runtime metrics are not available.".to_owned(),
		|metrics| {
			format!(
				"```rs\nnum_workers: {}\nnum_alive_tasks: {}\nglobal_queue_depth: {}\n```",
				metrics.num_workers(),
				metrics.num_alive_tasks(),
				metrics.global_queue_depth()
			)
		},
	);

	self.write_str(&out).await
}

#[cfg(not(tokio_unstable))]
#[admin_command]
pub(super) async fn runtime_metrics(&self) -> Result {
	self.write_str("Runtime metrics require building with `tokio_unstable`.")
		.await
}

#[cfg(all(tokio_unstable, feature = "tokio_metrics"))]
#[admin_command]
pub(super) async fn runtime_interval(&self) -> Result {
	let out = self.services.server.metrics.runtime_interval().map_or_else(
		|| "Runtime metrics are not available.".to_owned(),
		|metrics| format!("```rs\n{metrics:#?}\n```"),
	);

	self.write_str(&out).await
}

#[cfg(not(all(tokio_unstable, feature = "tokio_metrics")))]
#[admin_command]
pub(super) async fn runtime_interval(&self) -> Result {
	self.write_str("Runtime metrics require building with `tokio_unstable` and `tokio_metrics`.")
		.await
}

#[admin_command]
pub(super) async fn time(&self) -> Result {
	let now = SystemTime::now();
	let now = utils::time::format(now, "%+");

	self.write_str(&now).await
}

#[admin_command]
pub(super) async fn database_stats(
	&self,
	property: Option<String>,
	map: Option<String>,
) -> Result {
	let map_name = map.as_ref().map_or(EMPTY, String::as_str);
	let property = property.unwrap_or_else(|| "rocksdb.stats".to_owned());
	self.services
		.db
		.iter()
		.filter(|&(&name, _)| map_name.is_empty() || map_name == name)
		.try_stream()
		.try_for_each(|(&name, map)| {
			let res = map.property(&property).expect("invalid property");
			writeln!(self, "##### {name}:\n```\n{}\n```", res.trim())
		})
		.await
}

#[admin_command]
pub(super) async fn database_files(&self, map: Option<String>, level: Option<i32>) -> Result {
	let mut files: Vec<_> = self.services.db.db.file_list().collect::<Result<_>>()?;

	files.sort_by_key(|f| f.name.clone());

	writeln!(self, "| lev  | sst  | keys | dels | size | column |").await?;
	writeln!(self, "| ---: | :--- | ---: | ---: | ---: | :---   |").await?;
	files
		.into_iter()
		.filter(|file| {
			map.as_deref()
				.is_none_or(|map| map == file.column_family_name)
		})
		.filter(|file| level.as_ref().is_none_or(|&level| level == file.level))
		.try_stream()
		.try_for_each(|file| {
			writeln!(
				self,
				"| {} | {:<13} | {:7}+ | {:4}- | {:9} | {} |",
				file.level,
				file.name,
				file.num_entries,
				file.num_deletions,
				file.size,
				file.column_family_name,
			)
		})
		.await
}

#[admin_command]
pub(super) async fn trim_memory(&self) -> Result {
	conduwuit::alloc::trim(None)?;

	writeln!(self, "done").await
}

#[admin_command]
pub(super) async fn send_test_email(&self) -> Result {
	self.bail_restricted()?;

	let mailer = self.services.mailer.expect_mailer()?;
	let Some(sender) = self.sender else {
		return Err!("No sender user provided in context");
	};

	let Some(email) = self
		.services
		.threepid
		.get_email_for_localpart(sender.localpart())
		.await
	else {
		return Err!("{} has no associated email address", sender);
	};

	mailer
		.send(Mailbox::new(None, email.clone()), service::mailer::messages::Test)
		.await?;

	self.write_str(&format!("Test email successfully sent to {email}"))
		.await?;

	Ok(())
}
