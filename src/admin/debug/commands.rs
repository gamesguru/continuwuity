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
	state_res, trace, utils,
	utils::{
		stream::{BroadbandExt, IterStream, ReadyExt},
		string::EMPTY,
	},
	warn,
};
use futures::{FutureExt, StreamExt, TryStreamExt, future::ready, pin_mut};
use lettre::message::Mailbox;
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName,
	OwnedUserId, RoomId, RoomVersionId,
	api::federation::event::{get_event, get_room_state},
	events::{AnyStateEvent, StateEventType, TimelineEventType},
	serde::Raw,
};
use service::rooms::{
	short::{ShortEventId, ShortRoomId},
	state_compressor::HashSetCompressStateEvent,
};
use tokio::io::AsyncWriteExt as _;
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
pub(super) async fn rescue_pdu(
	&self,
	event_id: OwnedEventId,
	force: bool,
	skip_soft_fail: bool,
) -> Result {
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

	// Only un-soft-fail when --force is passed
	if force || skip_soft_fail {
		self.services
			.rooms
			.pdu_metadata
			.unmark_event_soft_failed(&event_id);
	}

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
				skip_soft_fail,
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
			.take(limit.saturating_add(1))
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
			.take(limit.saturating_add(1))
			.collect()
			.await
	};

	// Sort by origin_server_ts
	outliers.sort_by_key(|(_, pdu)| pdu.origin_server_ts);

	let mut count = 0_usize;
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
		let room_id_str = pdu.room_id().map_or("unknown", RoomId::as_str);
		let sender = pdu.sender();
		let kind = pdu.kind.to_string();
		let ts = pdu.origin_server_ts;
		let stuck_flag = if is_stuck { " [STUCK]" } else { "" };
		writeln!(
			body,
			"{event_id}\tTS: {ts}\tRoom: {room_id_str}\tSender: {sender}\tType: \
			 {kind}{stuck_flag}"
		)?;
		count = count.saturating_add(1);
	}

	if body.is_empty() {
		return Err!("No outliers found.");
	}

	self.write_str(&format!("Outliers:\n```\n{body}\n```"))
		.await
}

#[admin_command]
pub(super) async fn view_extremities(&self, room: OwnedRoomOrAliasId) -> Result {
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
	room_id: Option<OwnedRoomOrAliasId>,
	sender: Option<OwnedUserId>,
	all: bool,
	force: bool,
) -> Result {
	if room_id.is_none() && sender.is_none() && !all {
		return Err!("You must specify a room, a sender, or use --all to purge outliers.");
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
			self.services.rooms.outlier.remove_outlier(event_id).await;
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
			self.services.rooms.outlier.remove_outlier(event_id).await;
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
pub(super) async fn rescue_room(
	&self,
	room_id: OwnedRoomId,
	force: bool,
	nuclear: bool,
	all: bool,
	timeline_limit: Option<usize>,
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
			if Box::pin(self.rescue_room(room_id, force, nuclear, false, None))
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
		let state_pdus = self.services.rooms.state_accessor.state_full(state_hash);
		pin_mut!(state_pdus);
		while let Some(((event_type, state_key), event)) = state_pdus.next().await {
			let eid = event.event_id().to_owned();
			// Fetch the full PduEvent for depth access
			if let Ok(full_pdu) = self.services.rooms.timeline.get_pdu(&eid).await {
				current_state.insert(
					(event_type.to_string(), state_key.to_string()),
					(full_pdu.origin_server_ts, full_pdu.depth, eid),
				);
			}
		}
	}

	let mut count = 0_usize;
	let mut skipped = 0_usize;
	for event_id in sorted {
		let (pdu, pdu_json) = outliers.get(&event_id).expect("in sorted list");

		// Skip state events that are superseded by a newer event already in the
		// timeline for the same (event_type, state_key). Uses 3 tiebreakers:
		// origin_server_ts, depth, event_id (matching state-res ordering).
		// When --force is set, skip this check to allow historical state events
		// to be inserted for complete timeline history.
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

		let origin = pdu
			.origin
			.clone()
			.unwrap_or_else(|| pdu.sender.server_name().to_owned());

		// Only un-soft-fail when --force is passed; otherwise previously
		// rejected events stay rejected to prevent infinite rescue loops.
		if force {
			self.services
				.rooms
				.pdu_metadata
				.unmark_event_soft_failed(&event_id);
		}

		if Box::pin(
			self.services
				.rooms
				.event_handler
				.upgrade_outlier_to_timeline_pdu(
					pdu.clone(),
					pdu_json.clone(),
					&create_event,
					&origin,
					&room_id,
					nuclear,
				),
		)
		.await
		.is_ok()
		{
			count = count.saturating_add(1);
			// Update current_state so subsequent events can compare against
			// the just-rescued event
			if let Some(state_key) = &pdu.state_key {
				let key = (pdu.kind.to_string(), state_key.to_string());
				current_state
					.insert(key, (pdu.origin_server_ts, pdu.depth, pdu.event_id.clone()));
			}
		}

		// Yield every 10 events to prevent blocking the executor too long
		if count.is_multiple_of(10) {
			tokio::task::yield_now().await;
		}
	}

	let msg = if skipped > 0 {
		format!("Rescued {count} PDUs in room {room_id} (skipped {skipped} superseded).")
	} else {
		format!("Rescued {count} PDUs in room {room_id}.")
	};
	self.write_str(&msg).await
}

#[admin_command]
pub(super) async fn reorder_timeline(&self, room_id: OwnedRoomId, all: bool) -> Result {
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
			if self
				.services
				.rooms
				.timeline
				.reorder_timeline(&room_id)
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

	self.write_str(&format!("Reordering timeline for {room_id} by origin_server_ts..."))
		.await?;

	let count = self
		.services
		.rooms
		.timeline
		.reorder_timeline(&room_id)
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
	}

	self.write_str(&format!(
		"Promoted {promoted} outliers, {failed} failed out of {total} total for {room_id}. \
		 Clients should re-sync."
	))
	.await
}

#[admin_command]
pub(super) async fn purge_outlier(&self, event_id: OwnedEventId) -> Result {
	self.bail_restricted()?;

	self.services.rooms.outlier.remove_outlier(&event_id).await;

	self.write_str(&format!("Purged outlier {event_id}")).await
}

#[admin_command]
pub(super) async fn get_room_dag(
	&self,
	room_id: OwnedRoomOrAliasId,
	start: u64,
	end: i64,
) -> Result {
	self.bail_restricted()?;

	let room_id = self.services.rooms.alias.resolve(&room_id).await?;
	let pdus = self.services.rooms.timeline.all_pdus(&room_id);
	pin_mut!(pdus);

	let mut i = 0_u64;
	let mut count = 0_u64;
	let path = format!("/tmp/dag-{room_id}-{start}.jsonl");
	let mut file = tokio::fs::File::create(&path)
		.await
		.map_err(|e| err!(Database("Failed to create file {path}: {e:?}")))?;

	while let Some((_, pdu)) = pdus.next().await {
		if i >= start {
			let json = serde_json::to_string(&pdu)?;
			file.write_all(json.as_bytes()).await?;
			file.write_all(b"\n").await?;
			count = count.saturating_add(1);
		}
		i = i.saturating_add(1);
		if let Ok(end) = u64::try_from(end) {
			if i > end {
				break;
			}
		}
	}

	self.write_str(&format!("Successfully wrote {count} PDUs to {path}"))
		.await
}

#[admin_command]
pub(super) async fn get_remote_dag(
	&self,
	room_id: OwnedRoomId,
	server: OwnedServerName,
	limit: usize,
) -> Result {
	self.bail_restricted()?;

	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.");
	}

	if server == self.services.globals.server_name() {
		return Err!("Cannot fetch from ourselves. Use get-room-dag instead.");
	}

	let room_version = self.services.rooms.state.get_room_version(&room_id).await?;

	// Start from the latest local event in the room
	let latest = self
		.services
		.rooms
		.timeline
		.latest_pdu_in_room(&room_id)
		.await?;

	let path = format!("/tmp/remote-dag-{room_id}-{server}.jsonl");
	let mut file = tokio::fs::File::create(&path)
		.await
		.map_err(|e| err!(Database("Failed to create file {path}: {e:?}")))?;

	let mut seen = HashSet::<OwnedEventId>::new();
	let mut queue = vec![latest.event_id().to_owned()];
	let mut total = 0_usize;
	let batch_size = ruma::uint!(100);

	self.write_str(&format!("Fetching DAG from {server} for {room_id} (limit: {limit})..."))
		.await?;

	while !queue.is_empty() && total < limit {
		let request = ruma::api::federation::backfill::get_backfill::v1::Request {
			room_id: room_id.clone(),
			v: queue.clone(),
			limit: batch_size,
		};

		let response = match self
			.services
			.sending
			.send_federation_request(&server, request)
			.await
		{
			| Ok(r) => r,
			| Err(e) => {
				self.write_str(&format!("Federation request failed: {e}"))
					.await?;
				break;
			},
		};

		if response.pdus.is_empty() {
			break;
		}

		queue.clear();

		for raw_pdu in &response.pdus {
			let Ok((event_id, value)) = self
				.services
				.server_keys
				.validate_and_add_event_id(raw_pdu, &room_version)
				.await
			else {
				continue;
			};

			if seen.contains(&event_id) {
				continue;
			}
			seen.insert(event_id.clone());

			let Ok(pdu) = PduEvent::from_id_val(&event_id, value.clone(), Some(room_id.as_ref()))
			else {
				continue;
			};

			let json = serde_json::to_string(&pdu)?;
			file.write_all(json.as_bytes()).await?;
			file.write_all(b"\n").await?;
			total = total.saturating_add(1);

			// Add prev_events to the queue for next iteration
			for prev in pdu.prev_events() {
				if !seen.contains(prev) {
					queue.push(prev.to_owned());
				}
			}

			if total >= limit {
				break;
			}
		}

		// Yield to avoid blocking
		tokio::task::yield_now().await;
	}

	self.write_str(&format!("Successfully fetched {total} PDUs from {server} to {path}"))
		.await
}

#[admin_command]
pub(super) async fn fetch_pdu(
	&self,
	room_id: OwnedRoomId,
	event_id: OwnedEventId,
	server: OwnedServerName,
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

	let room_version = self.services.rooms.state.get_room_version(&room_id).await?;

	let response = self
		.services
		.sending
		.send_federation_request(&server, get_event::v1::Request::new(event_id, None))
		.await?;

	let (event_id, value) = self
		.services
		.server_keys
		.validate_and_add_event_id(&response.pdu, &room_version)
		.await?;

	let create_event = self
		.services
		.rooms
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await?;

	let pdu = PduEvent::from_id_val(&event_id, value.clone(), Some(room_id.as_ref()))
		.map_err(|e| err!(Database("Invalid PDU: {e:?}")))?;

	let result = Box::pin(
		self.services
			.rooms
			.event_handler
			.upgrade_outlier_to_timeline_pdu(pdu, value, &create_event, &server, &room_id, false),
	)
	.await?;

	match result {
		| Some(id) => write!(self, "Successfully fetched and rescued PDU: {id:?}"),
		| None => write!(self, "PDU was already present or promoted successfully."),
	}
	.await
}

#[admin_command]
#[tracing::instrument(skip(self), level = "info")]
pub(super) async fn force_set_room_state_from_server(
	&self,
	room_id: OwnedRoomId,
	server_name: OwnedServerName,
	at_event: Option<OwnedEventId>,
	overwrite: bool,
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
					"We are not participating in the room; you must specify an event ID with \
					 --at-event to bootstrap."
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

	let remote_state_response = self
		.services
		.sending
		.send_federation_request(&server_name, get_room_state::v1::Request {
			room_id: room_id.clone(),
			event_id: at_event_id,
		})
		.await?;

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
			info!(
				"PDU {event_id} already in timeline (pdu_id={pdu_id:?}), skipping outlier insert"
			);
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

	info!("Going through auth_chain response");
	for result in remote_state_response.auth_chain.iter().map(|pdu| {
		self.services
			.server_keys
			.validate_and_add_event_id(pdu, &room_version)
	}) {
		let Ok((event_id, value)) = result.await else {
			continue;
		};

		if let Ok(pdu_id) = self.services.rooms.timeline.get_pdu_id(&event_id).await {
			info!(
				"Auth PDU {event_id} already in timeline (pdu_id={pdu_id:?}), skipping outlier \
				 insert"
			);
		} else {
			info!("Auth PDU {event_id} NOT in timeline, adding as outlier");
			self.services
				.rooms
				.outlier
				.add_pdu_outlier(&event_id, &value, Some(&room_id));
		}
	}

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
		self.services
			.rooms
			.event_handler
			.resolve_state(&room_id, &room_version, state)
			.await?
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
	self.services
		.rooms
		.state
		.force_state(room_id.clone().as_ref(), short_state_hash, added, removed, &state_lock)
		.await?;

	info!(
		"Updating joined counts for room just in case (e.g. we may have found a difference in \
		 the room's m.room.member state"
	);
	self.services
		.rooms
		.state_cache
		.update_joined_count(&room_id)
		.await;

	self.write_str("Successfully forced the room state from the requested remote server.")
		.await
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

#[admin_command]
pub(super) async fn compare_room_state(
	&self,
	room_id: OwnedRoomId,
	server: OwnedServerName,
	at_event: Option<OwnedEventId>,
) -> Result {
	self.bail_restricted()?;

	let room_version = self.services.rooms.state.get_room_version(&room_id).await?;
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

	let response = match self
		.services
		.sending
		.send_federation_request(&server, get_room_state::v1::Request {
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

	let mut remote_state = HashMap::new();
	let mut skipped = 0_usize;
	for pdu in &response.pdus {
		let (event_id, value) = match self
			.services
			.server_keys
			.validate_and_add_event_id(pdu, &room_version)
			.await
		{
			| Ok(r) => r,
			| Err(e) => {
				warn!("Skipping PDU with bad signature: {e}");
				skipped = skipped.saturating_add(1);
				continue;
			},
		};

		let pdu = PduEvent::from_id_val(&event_id, value, Some(room_id.as_ref()))?;
		if let Some(state_key) = &pdu.state_key {
			remote_state.insert((pdu.kind.to_string(), state_key.to_string()), event_id);
		}
	}

	let local_state_hash = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await?;
	let local_state: HashMap<_, _> = self
		.services
		.rooms
		.state_accessor
		.state_full(local_state_hash)
		.map(|((ty, sk), pdu)| ((ty.to_string(), sk.to_string()), pdu.event_id().to_owned()))
		.collect()
		.await;

	let mut missing_locally = Vec::new();
	for (key, event_id) in &remote_state {
		if local_state.get(key) != Some(event_id) {
			missing_locally.push(format!("{event_id} ({:?} \"{}\")", key.0, key.1));
		}
	}

	let mut extra_locally = Vec::new();
	for (key, event_id) in &local_state {
		if remote_state.get(key) != Some(event_id) {
			extra_locally.push(format!("{event_id} ({:?} \"{}\")", key.0, key.1));
		}
	}

	self.write_str(&format!(
		"Room State Comparison for {room_id} vs {server}:\n- Missing locally: {}\n- Extra \
		 locally: {}\n\nMissing IDs:\n```\n{:#?}\n```\n\nExtra IDs:\n```\n{:#?}\n```",
		missing_locally.len(),
		extra_locally.len(),
		missing_locally,
		extra_locally
	))
	.await
}

#[admin_command]
pub(super) async fn compare_remote_state(
	&self,
	room_id: OwnedRoomId,
	server1: OwnedServerName,
	server2: OwnedServerName,
	at_event: Option<OwnedEventId>,
) -> Result {
	self.bail_restricted()?;

	// Try to resolve at_event: explicit > local timeline > error
	let at_event_id = match at_event {
		| Some(event_id) => event_id,
		| None => {
			// Fall back to local timeline if we know the room
			match self
				.services
				.rooms
				.timeline
				.latest_pdu_in_room(&room_id)
				.await
			{
				| Ok(pdu) => pdu.event_id().to_owned(),
				| Err(_) => {
					return Err!(Request(NotFound(
						"Room not known locally. Provide an --at-event ID to compare remote \
						 state for rooms this server hasn't joined."
					)));
				},
			}
		},
	};

	// Fetch state from both servers at the same reference PDU
	let (response1, response2) = futures::join!(
		self.services
			.sending
			.send_federation_request(&server1, get_room_state::v1::Request {
				room_id: room_id.clone(),
				event_id: at_event_id.clone(),
			}),
		self.services
			.sending
			.send_federation_request(&server2, get_room_state::v1::Request {
				room_id: room_id.clone(),
				event_id: at_event_id,
			}),
	);

	let (response1, response2) = (response1?, response2?);

	// Determine room version: try local first, fall back to remote create event
	let room_version = match self.services.rooms.state.get_room_version(&room_id).await {
		| Ok(v) => v,
		| Err(_) => {
			// Extract from m.room.create in server1's response
			let mut found_version = None;
			for pdu_raw in &response1.pdus {
				let value: serde_json::Value = serde_json::from_str(pdu_raw.get())?;
				if value.get("type").and_then(|t| t.as_str()) == Some("m.room.create") {
					// Room version from content.room_version, default to "1"
					let version_str = value
						.get("content")
						.and_then(|c| c.get("room_version"))
						.and_then(|v| v.as_str())
						.unwrap_or("1");
					found_version =
						Some(RoomVersionId::try_from(version_str).unwrap_or(RoomVersionId::V11));
					break;
				}
			}
			found_version.ok_or_else(|| {
				err!(
					"Could not determine room version from remote state (no m.room.create found)"
				)
			})?
		},
	};

	let mut state1 = HashMap::new();
	let mut verify_errors1 = 0_usize;
	for pdu in &response1.pdus {
		let Ok((event_id, value)) = self
			.services
			.server_keys
			.validate_and_add_event_id(pdu, &room_version)
			.await
		else {
			verify_errors1 = verify_errors1.saturating_add(1);
			continue;
		};

		let Ok(pdu) = PduEvent::from_id_val(&event_id, value, Some(room_id.as_ref())) else {
			continue;
		};
		if let Some(state_key) = &pdu.state_key {
			state1.insert((pdu.kind.to_string(), state_key.to_string()), event_id);
		}
	}

	let mut state2 = HashMap::new();
	let mut verify_errors2 = 0_usize;
	for pdu in &response2.pdus {
		let Ok((event_id, value)) = self
			.services
			.server_keys
			.validate_and_add_event_id(pdu, &room_version)
			.await
		else {
			verify_errors2 = verify_errors2.saturating_add(1);
			continue;
		};

		let Ok(pdu) = PduEvent::from_id_val(&event_id, value, Some(room_id.as_ref())) else {
			continue;
		};
		if let Some(state_key) = &pdu.state_key {
			state2.insert((pdu.kind.to_string(), state_key.to_string()), event_id);
		}
	}

	let mut only_on_server1 = Vec::new();
	for (key, event_id) in &state1 {
		if state2.get(key) != Some(event_id) {
			only_on_server1.push(format!("{event_id} ({:?} \"{}\")", key.0, key.1));
		}
	}

	let mut only_on_server2 = Vec::new();
	for (key, event_id) in &state2 {
		if state1.get(key) != Some(event_id) {
			only_on_server2.push(format!("{event_id} ({:?} \"{}\")", key.0, key.1));
		}
	}

	let verify_note = if verify_errors1 > 0 || verify_errors2 > 0 {
		format!(
			"\n\nNote: {verify_errors1} events from {server1} and {verify_errors2} from \
			 {server2} skipped (signature verification failed)"
		)
	} else {
		String::new()
	};

	self.write_str(&format!(
		"Remote State Comparison for {room_id}:\n- {server1} vs {server2}\n- Only on {server1}: \
		 {}\n- Only on {server2}: {}\n\nIDs only on {server1}:\n```\n{:#?}\n```\n\nIDs only on \
		 {server2}:\n```\n{:#?}\n```{verify_note}",
		only_on_server1.len(),
		only_on_server2.len(),
		only_on_server1,
		only_on_server2
	))
	.await
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
		self.write_str(&format!("Phase 1: Rescuing local outliers in {room_id}..."))
			.await?;
		Box::pin(self.rescue_room(room_id.clone(), nuclear, nuclear, false, None)).await?;
	} else {
		self.write_str(&format!("Phase 1: [dry-run] Would rescue local outliers in {room_id}"))
			.await?;
	}

	// Phase 2: Walk the DAG to find genuinely missing events
	self.write_str("Phase 2: Scanning DAG for gaps...").await?;
	let room_version = self.services.rooms.state.get_room_version(&room_id).await?;
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

	let mut queue: std::collections::VecDeque<OwnedEventId> =
		latest.prev_events().map(ToOwned::to_owned).collect();
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
		queue.extend(pdu.prev_events().map(ToOwned::to_owned));
		queue.extend(pdu.auth_events().map(ToOwned::to_owned));
		fetched = fetched.saturating_add(1);

		// Yield periodically to avoid blocking the executor
		if fetched.is_multiple_of(10) {
			tokio::task::yield_now().await;
		}
	}

	self.write_str(&format!(
		"Phase 2: Scanned {seen} events ({local_found} local, {fetched} {action})",
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
		Box::pin(self.rescue_room(room_id.clone(), nuclear, nuclear, false, None)).await?;
	} else {
		self.write_str("Phase 3: No missing events found (DAG is complete locally).")
			.await?;
	}

	// Phase 4: Reorder timeline by origin_server_ts so auth checks work correctly
	self.write_str("Phase 4: Reordering timeline by timestamp...")
		.await?;
	Box::pin(self.reorder_timeline(room_id.clone(), false)).await?;
	self.write_str("Phase 4: Reordered timeline.").await?;

	// Phase 5: Resync state from the remote server (opt-in)
	if resync_state {
		self.write_str("Phase 5: Resyncing room state from server...")
			.await?;
		Box::pin(self.force_set_room_state_from_server(room_id.clone(), server, None, nuclear))
			.await?;

		// Phase 5b: Rescue any outliers created by Phase 5's state resync
		self.write_str("Phase 5b: Rescuing state outliers from resync...")
			.await?;
		Box::pin(self.rescue_room(room_id.clone(), nuclear, nuclear, false, None)).await?;
	} else {
		self.write_str("Phase 5: Skipped state resync (pass --resync-state to enable).")
			.await?;
	}

	// Phase 6: Purge stuck outliers (events that now exist in both tables)
	if purge_after {
		self.write_str("Phase 6: Purging stuck outliers...").await?;
		let room_alias = OwnedRoomOrAliasId::from(room_id);
		Box::pin(self.purge_outliers(Some(room_alias), None, false, false)).await?;
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
