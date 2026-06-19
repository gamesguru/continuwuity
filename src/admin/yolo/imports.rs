use std::sync::atomic::{AtomicUsize, Ordering};

use conduwuit::{Err, Result, err, info, warn};
use futures::{StreamExt, stream::FuturesUnordered};
use ruma::{
	CanonicalJsonObject, OwnedEventId, OwnedRoomId, RoomVersionId, events::StateEventType,
};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::admin_command;

#[admin_command]
pub(super) async fn import_pdus(
	&self,
	room_id: OwnedRoomId,
	path: String,
	skip_auth: bool,
	skip_sig_verify: bool,
	force: bool,
	room_version: Option<RoomVersionId>,
) -> Result {
	self.bail_restricted()?;

	let room_version = match room_version {
		| Some(v) => {
			self.services.rooms.short.set_room_version(&room_id, &v);
			v
		},
		| None => match self.services.rooms.state.get_room_version(&room_id).await {
			| Ok(v) => v,
			| Err(_) => {
				return Err!(Request(InvalidParam(
					"Local room version unknown. You must specify --room-version explicitly \
					 when importing to an empty room."
				)));
			},
		},
	};

	let file = tokio::fs::File::open(&path)
		.await
		.map_err(|e| err!("Failed to open file {path}: {e:?}"))?;
	let mut lines = BufReader::new(file).lines();
	let origin = room_id
		.server_name()
		.filter(|s| !self.services.globals.server_is_ours(s))
		.unwrap_or_else(|| self.services.globals.server_name())
		.to_owned();

	// Cork database writes to batch and sync efficiently on drop
	let _cork = self.services.db.cork();

	let inserted = AtomicUsize::new(0);
	let failed = AtomicUsize::new(0);
	let total = AtomicUsize::new(0);
	let rejected = AtomicUsize::new(0);

	let mode = match (skip_auth, skip_sig_verify) {
		| (true, _) => "force-insert (skip-auth)",
		| (_, true) => "auth-checked (skip-sig-verify)",
		| _ => "full pipeline",
	};

	self.write_str(&format!(
		"Importing PDUs from {path} into {room_id} [{mode}] (streaming)...\n"
	))
	.await?;

	let create_event = std::sync::Arc::new(
		self.services
			.rooms
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomCreate, "")
			.await
			.ok(),
	);

	let mut futures = FuturesUnordered::new();

	while let Ok(Some(line)) = lines.next_line().await {
		if line.trim().is_empty() {
			continue;
		}

		let create_event = std::sync::Arc::clone(&create_event);
		let room_id_ref = room_id.clone();
		let room_version_ref = room_version.clone();
		let origin = origin.clone();
		
		let server_keys = self.services.server_keys.clone();
		let event_handler = self.services.rooms.event_handler.clone();

		futures.push(tokio::spawn(async move {
			let value: CanonicalJsonObject =
				match tokio::task::spawn_blocking(move || serde_json::from_str(&line))
					.await
					.unwrap()
				{
					| Ok(v) => v,
					| Err(e) => return Err(e.to_string()),
				};

			let is_outlier = value
				.get("__outlier")
				.and_then(|v| match v {
					| ruma::CanonicalJsonValue::Bool(b) => Some(*b),
					| _ => None,
				})
				.unwrap_or(false);
			let is_soft_failed = value
				.get("__soft_failed")
				.and_then(|v| match v {
					| ruma::CanonicalJsonValue::Bool(b) => Some(*b),
					| _ => None,
				})
				.unwrap_or(false);
			let is_rejected = value
				.get("__rejected")
				.and_then(|v| match v {
					| ruma::CanonicalJsonValue::Bool(b) => Some(*b),
					| _ => None,
				})
				.unwrap_or(false);

			let (eid, value, pdu) = tokio::task::spawn_blocking({
				let room_id_ref = room_id_ref.clone();
				let room_version_ref = room_version_ref.clone();
				move || {
					conduwuit::utils::pdu_parser::parse_and_clean_pdu(
						value,
						room_id_ref.as_ref(),
						&room_version_ref,
					)
				}
			})
			.await
			.unwrap().map_err(|e| e.to_string())?;

			if is_outlier {
				return Ok((eid, value, pdu, true, is_soft_failed, is_rejected));
			}

			if force {
				return Ok((eid, value, pdu, false, is_soft_failed, is_rejected));
			}

			if skip_auth {
				Ok((eid, value, pdu, false, is_soft_failed, is_rejected))
			} else {
				let (eid, val) = if skip_sig_verify {
					(eid, value.clone())
				} else {
					let mut raw_val = value.clone();
					if room_version_ref != RoomVersionId::V1
						&& room_version_ref != RoomVersionId::V2
					{
						raw_val.remove("event_id");
					}
					let raw = serde_json::value::RawValue::from_string(
						serde_json::to_string(&raw_val).map_err(|e| e.to_string())?,
					)
					.map_err(|e| e.to_string())?;

					match server_keys
						.validate_and_add_event_id(&raw, &room_version_ref)
						.await
					{
						| Ok(result) => result,
						| Err(_) => {
							return Ok((eid, value, pdu, true, true, is_rejected));
						},
					}
				};

				let mut pdu_val = val;
				if room_version_ref != RoomVersionId::V1
					&& room_version_ref != RoomVersionId::V2
				{
					pdu_val.remove("event_id");
				}

				let (pdu, _parsed) = event_handler
					.handle_outlier_pdu(
						&origin,
						create_event.as_ref().as_ref(),
						&eid,
						&room_id_ref,
						pdu_val,
						true,
						skip_sig_verify,
						Some(&room_version_ref),
					)
					.await.map_err(|e| e.to_string())?;

				Ok((eid, value, pdu, false, is_soft_failed, is_rejected))
			}
		}));

		while futures.len() >= 200 {
			if let Some(res) = futures.next().await {
				total.fetch_add(1, Ordering::Relaxed);
				let (eid, value, pdu, is_outlier, is_soft_failed, is_rejected) = match res {
					Ok(Ok(data)) => data,
					Ok(Err(e)) => { warn!("import_pdus: Failed to parse line: {e}"); failed.fetch_add(1, Ordering::Relaxed); continue; },
					Err(e) => { warn!("import_pdus loop task panic: {e}"); failed.fetch_add(1, Ordering::Relaxed); continue; }
				};

				let insert_result: Result<(OwnedEventId, bool)> = async {
					if is_outlier {
						self.services.rooms.outlier.add_pdu_outlier(&eid, &value, Some(&room_id));
						return Ok((eid, true));
					}
					if force {
						if let Ok(pdu_id) = self.services.rooms.timeline.get_pdu_id(&eid).await {
							self.services.rooms.timeline.replace_pdu(&pdu_id, &value, &eid).await?;
							return Ok((eid, true));
						}
					}
					if skip_auth {
						self.services.rooms.timeline.force_insert_pdu(&room_id, &eid, &pdu, &value, true).await?;
						Ok((eid, true))
					} else {
						self.services.rooms.timeline.promote_outlier(&room_id, &eid).await?;
						Ok((eid, true))
					}
				}.await;

				match insert_result {
					Ok((eid, true)) => {
						inserted.fetch_add(1, Ordering::Relaxed);
						if is_soft_failed { self.services.rooms.pdu_metadata.mark_event_soft_failed(&eid); }
						if is_rejected { self.services.rooms.pdu_metadata.mark_event_rejected(&eid); }
					},
					Ok((_eid, false)) => { rejected.fetch_add(1, Ordering::Relaxed); },
					Err(e) => { warn!("import_pdus insert err: {e}"); failed.fetch_add(1, Ordering::Relaxed); },
				}

				let done = inserted.load(Ordering::Relaxed).saturating_add(failed.load(Ordering::Relaxed)).saturating_add(rejected.load(Ordering::Relaxed));
				if done.is_multiple_of(1000) {
					info!("import_pdus: {}/{} ({} ok, {} rejected, {} err)", done, total.load(Ordering::Relaxed), inserted.load(Ordering::Relaxed), rejected.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
				}
			}
		}
	}

	while let Some(res) = futures.next().await {
		total.fetch_add(1, Ordering::Relaxed);
		let (eid, value, pdu, is_outlier, is_soft_failed, is_rejected) = match res {
			Ok(Ok(data)) => data,
			Ok(Err(e)) => { warn!("import_pdus: Failed to parse line: {e}"); failed.fetch_add(1, Ordering::Relaxed); continue; },
			Err(e) => { warn!("import_pdus loop task panic: {e}"); failed.fetch_add(1, Ordering::Relaxed); continue; }
		};

		let insert_result: Result<(OwnedEventId, bool)> = async {
			if is_outlier {
				self.services.rooms.outlier.add_pdu_outlier(&eid, &value, Some(&room_id));
				return Ok((eid, true));
			}
			if force {
				if let Ok(pdu_id) = self.services.rooms.timeline.get_pdu_id(&eid).await {
					self.services.rooms.timeline.replace_pdu(&pdu_id, &value, &eid).await?;
					return Ok((eid, true));
				}
			}
			if skip_auth {
				self.services.rooms.timeline.force_insert_pdu(&room_id, &eid, &pdu, &value, true).await?;
				Ok((eid, true))
			} else {
				self.services.rooms.timeline.promote_outlier(&room_id, &eid).await?;
				Ok((eid, true))
			}
		}.await;

		match insert_result {
			Ok((eid, true)) => {
				inserted.fetch_add(1, Ordering::Relaxed);
				if is_soft_failed { self.services.rooms.pdu_metadata.mark_event_soft_failed(&eid); }
				if is_rejected { self.services.rooms.pdu_metadata.mark_event_rejected(&eid); }
			},
			Ok((_eid, false)) => { rejected.fetch_add(1, Ordering::Relaxed); },
			Err(e) => { warn!("import_pdus insert err: {e}"); failed.fetch_add(1, Ordering::Relaxed); },
		}

		let done = inserted.load(Ordering::Relaxed).saturating_add(failed.load(Ordering::Relaxed)).saturating_add(rejected.load(Ordering::Relaxed));
		if done.is_multiple_of(1000) {
			info!("import_pdus: {}/{} ({} ok, {} rejected, {} err)", done, total.load(Ordering::Relaxed), inserted.load(Ordering::Relaxed), rejected.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
		}
	}


	let (_, num_true) = self
		.services
		.rooms
		.timeline
		.recalculate_extremities(&room_id, 500, true)
		.await?;

	self.write_str(&format!(
		"\nImported {} PDUs, {} stored as rejected outliers, {} errors out of {} total for \
		 {room_id}. DAG Extremities recalculated (now {num_true} tips). Run `yolo rebuild-state \
		 {room_id}` if you are finished importing.",
		inserted.load(Ordering::Relaxed),
		rejected.load(Ordering::Relaxed),
		failed.load(Ordering::Relaxed),
		total.load(Ordering::Relaxed)
	))
	.await
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
