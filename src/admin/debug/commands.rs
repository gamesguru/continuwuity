use std::{
	collections::{HashMap, HashSet, VecDeque},
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
	let safe_room_id = room_id.to_string().replace('!', "").replace(':', "_");
	let path = format!("/tmp/dag-{safe_room_id}-{start}.jsonl");
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

	let safe_room_id = room_id.to_string().replace('!', "").replace(':', "_");
	let path = format!("/tmp/remote-dag-{safe_room_id}-{server}.jsonl");
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

	let pdu = PduEvent::from_id_val(&event_id, value.clone(), Some(room_id.as_ref()))
		.map_err(|e| err!(Database("Invalid PDU: {e:?}")))?;

	if skip_auth {
		// Direct insert into timeline, bypassing all auth checks.
		let msg = match self
			.services
			.rooms
			.timeline
			.force_insert_pdu(&room_id, &event_id, &pdu, &value)
			.await
		{
			| Ok(pdu_id) =>
				format!("Force-inserted PDU {event_id} into timeline (skipped auth): {pdu_id:?}"),
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

	info!("Resetting forward extremities to new state snapshot");
	self.services
		.rooms
		.state
		.reset_extremities_to_state(room_id.clone().as_ref(), short_state_hash, &state_lock)
		.await;

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
pub(super) async fn repair_unsigned(&self, room_id: OwnedRoomId) -> Result {
	use conduwuit::PduCount;

	self.bail_restricted()?;

	let pdus = self
		.services
		.rooms
		.timeline
		.pdus(&room_id, Some(PduCount::min()));

	pin_mut!(pdus);
	let mut repaired = 0_usize;
	let mut skipped = 0_usize;
	let mut errors = 0_usize;

	while let Some(Ok((_count, pdu))) = pdus.next().await {
		// Only state events have prev_content
		let Some(state_key) = pdu.state_key() else {
			continue;
		};

		let event_id = pdu.event_id();

		// Get the stored JSON
		let Ok(mut pdu_json) = self.services.rooms.timeline.get_pdu_json(event_id).await else {
			errors = errors.saturating_add(1);
			continue;
		};

		// Look up the state snapshot at this event (state before this event's
		// changes), which is what set_event_state() stored.
		let Ok(shortstatehash) = self
			.services
			.rooms
			.state_accessor
			.pdu_shortstatehash(event_id)
			.await
		else {
			skipped = skipped.saturating_add(1);
			continue;
		};

		// Get or create the unsigned object
		let unsigned = pdu_json.entry("unsigned".to_owned()).or_insert_with(|| {
			ruma::CanonicalJsonValue::Object(std::collections::BTreeMap::new())
		});

		let ruma::CanonicalJsonValue::Object(unsigned) = unsigned else {
			errors = errors.saturating_add(1);
			continue;
		};

		// Remove old (possibly wrong) prev_content fields
		unsigned.remove("prev_content");
		unsigned.remove("prev_sender");
		unsigned.remove("replaces_state");

		// Look up the correct previous state event
		if let Ok(prev_state) = self
			.services
			.rooms
			.state_accessor
			.state_get(shortstatehash, &pdu.kind().to_string().into(), state_key)
			.await
		{
			if let Ok(content_obj) = utils::to_canonical_object(prev_state.get_content_as_value())
			{
				unsigned.insert(
					"prev_content".to_owned(),
					ruma::CanonicalJsonValue::Object(content_obj),
				);
				unsigned.insert(
					"prev_sender".to_owned(),
					ruma::CanonicalJsonValue::String(prev_state.sender().to_string()),
				);
				unsigned.insert(
					"replaces_state".to_owned(),
					ruma::CanonicalJsonValue::String(prev_state.event_id().to_string()),
				);
			}
		}

		// Write back
		let pdu_id = self.services.rooms.timeline.get_pdu_id(event_id).await?;
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

		if repaired.is_multiple_of(1000) {
			info!("Repair progress: {repaired} state events repaired so far");
		}
	}

	self.write_str(&format!(
		"Repair complete for room {room_id}: {repaired} state events repaired, {skipped} \
		 skipped (no state snapshot), {errors} errors"
	))
	.await
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
		Box::pin(self.force_set_room_state_from_server(
			room_id.clone(),
			server,
			None,
			nuclear,
			None,
		))
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

#[admin_command]
pub(super) async fn import_pdus(&self, room_id: OwnedRoomId, path: String) -> Result {
	self.bail_restricted()?;

	let contents = tokio::fs::read_to_string(&path)
		.await
		.map_err(|e| err!("Failed to read file {path}: {e:?}"))?;

	let mut inserted = 0_usize;
	let mut skipped = 0_usize;
	let mut failed = 0_usize;
	let total = contents.lines().filter(|l| !l.trim().is_empty()).count();

	self.write_str(&format!("Importing PDUs from {path} into {room_id} ({total} lines)..."))
		.await?;

	for line in contents.lines() {
		if line.trim().is_empty() {
			continue;
		}

		let value: CanonicalJsonObject = match serde_json::from_str(line) {
			| Ok(v) => v,
			| Err(e) => {
				warn!("import_pdus: bad JSON line: {e:?}");
				failed = failed.saturating_add(1);
				continue;
			},
		};

		let Some(event_id) = value
			.get("event_id")
			.and_then(ruma::CanonicalJsonValue::as_str)
			.and_then(|id| OwnedEventId::parse(id).ok())
		else {
			failed = failed.saturating_add(1);
			continue;
		};

		let pdu = match PduEvent::from_id_val(&event_id, value.clone(), Some(room_id.as_ref())) {
			| Ok(p) => p,
			| Err(e) => {
				warn!("import_pdus: bad PDU {event_id}: {e:?}");
				failed = failed.saturating_add(1);
				continue;
			},
		};

		match self
			.services
			.rooms
			.timeline
			.force_insert_pdu(&room_id, &event_id, &pdu, &value)
			.await
		{
			| Ok(_) => {
				inserted = inserted.saturating_add(1);
			},
			| Err(_) => {
				// Already in timeline
				skipped = skipped.saturating_add(1);
			},
		}
	}

	self.write_str(&format!(
		"Imported {inserted} PDUs, skipped {skipped}, failed {failed} out of {total} total for \
		 {room_id}. Run `reorder-timeline` and `force-set-room-state` to finalize."
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
	server: OwnedServerName,
	event_a: Option<OwnedEventId>,
	event_b: Option<OwnedEventId>,
	max_depth: usize,
) -> Result {
	self.bail_restricted()?;

	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.");
	}

	if server == self.services.globals.server_name() {
		return Err!("Cannot compare against ourselves.");
	}

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

	// Resolve remote tip (event B) via federation
	let event_b = match event_b {
		| Some(id) => id,
		| None => {
			self.write_str(&format!("Fetching remote tip from {server}...\n"))
				.await?;
			// Get state_ids at our local tip — the remote server's latest known
			// event for this room will be in there
			let room_version = self.services.rooms.state.get_room_version(&room_id).await?;
			let request = get_room_state::v1::Request::new(event_a.clone(), room_id.clone());
			let response = self
				.services
				.sending
				.send_federation_request(&server, request)
				.await
				.map_err(|e| err!("Federation request to {server} failed: {e}"))?;
			// Find the most recent PDU from the response (highest depth)
			let mut best: Option<(OwnedEventId, PduEvent)> = None;
			for raw_pdu in &response.pdus {
				if let Ok((event_id, _value)) = self
					.services
					.server_keys
					.validate_and_add_event_id(raw_pdu, &room_version)
					.await
				{
					if let Ok(pdu) = serde_json::from_str::<PduEvent>(
						&serde_json::to_string(raw_pdu).unwrap_or_default(),
					) {
						let dominated = best.as_ref().is_none_or(|(_, b)| pdu.depth > b.depth);
						if dominated {
							best = Some((event_id, pdu));
						}
					}
				}
			}
			let (remote_tip_id, _) =
				best.ok_or_else(|| err!("No valid PDUs found from {server}"))?;
			self.write_str(&format!("Remote tip: {remote_tip_id}\n"))
				.await?;
			remote_tip_id
		},
	};

	// Check both events exist locally
	let pdu_a =
		get_pdu_any!(&event_a).ok_or_else(|| err!("Event A not found locally: {event_a}"))?;
	let pdu_b = get_pdu_any!(&event_b).ok_or_else(|| {
		err!(
			"Event B not found locally: {event_b}. You may need to fetch it first with `debug \
			 fetch-pdu`."
		)
	})?;

	self.write_str(&format!(
		"Walking DAG backwards from:\n  A (local): {} (depth {}, type {})\n  B (remote): {} \
		 (depth {}, type {})\n\nMax depth: {max_depth}\n",
		event_a, pdu_a.depth, pdu_a.kind, event_b, pdu_b.depth, pdu_b.kind,
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

	while (!queue_a.is_empty() || !queue_b.is_empty()) && steps < max_depth {
		// Expand one step from A
		if let Some((current, depth)) = queue_a.pop_front() {
			if ancestors_b.contains_key(&current) {
				if !merge_bases.contains(&current) {
					merge_bases.push(current.clone());
				}
				// Don't stop — find all merge bases at this depth
			}

			if let Some(pdu) = get_pdu_any!(&current) {
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

			if let Some(pdu) = get_pdu_any!(&current) {
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
		"Walked {} steps, visited {} (A) + {} (B) events, {} missing\n",
		steps,
		ancestors_a.len(),
		ancestors_b.len(),
		missing_events,
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
			|p| format!("depth {}, type {}", p.depth, p.kind),
		);

		self.write_str(&format!("\n### Merge base: `{mb}` ({mb_info})\n"))
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
					writeln!(graph, "      depth={}, ts={}", p.depth, p.origin_server_ts).ok();
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
