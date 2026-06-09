use conduwuit::{Err, Result, err, info, warn};
use ruma::{
	CanonicalJsonObject, OwnedEventId, OwnedRoomId, RoomVersionId, events::StateEventType,
};

use crate::admin_command;

#[admin_command]
pub(super) async fn import_pdus(
	&self,
	room_id: OwnedRoomId,
	path: String,
	skip_auth: bool,
	skip_sig_verify: bool,
	room_version: Option<RoomVersionId>,
) -> Result {
	use tokio::io::{AsyncBufReadExt, BufReader};

	self.bail_restricted()?;

	let room_version = match room_version {
		| Some(v) => v,
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
		.unwrap_or_else(|| self.services.globals.server_name());

	// Cork database writes to batch and sync efficiently on drop
	let _cork = self.services.db.cork();

	let mut inserted = 0_usize;
	let mut rejected = 0_usize;
	let mut failed = 0_usize;
	let mut total = 0_usize;

	let mode = match (skip_auth, skip_sig_verify) {
		| (true, _) => "force-insert (skip-auth)",
		| (_, true) => "auth-checked (skip-sig-verify)",
		| _ => "full pipeline",
	};

	self.write_str(&format!(
		"Importing PDUs from {path} into {room_id} [{mode}] (streaming)...\n"
	))
	.await?;

	let create_event = self
		.services
		.rooms
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await
		.ok();

	while let Ok(Some(line)) = lines.next_line().await {
		if line.trim().is_empty() {
			continue;
		}
		total = total.saturating_add(1);

		let value: CanonicalJsonObject = match serde_json::from_str(&line) {
			| Ok(v) => v,
			| Err(e) => {
				warn!("import_pdus: Failed to parse line as JSON: {e}");
				failed = failed.saturating_add(1);
				continue;
			},
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

		let result: Result<(OwnedEventId, bool)> = async {
			let (eid, value, pdu) = conduwuit::utils::pdu_parser::parse_and_clean_pdu(
				value,
				room_id.as_ref(),
				&room_version,
			)?;

			if is_outlier {
				self.services
					.rooms
					.outlier
					.add_pdu_outlier(&eid, &value, Some(&room_id));
				return Ok((eid, true));
			}

			if skip_auth {
				self.services
					.rooms
					.timeline
					.force_insert_pdu(&room_id, &eid, &pdu, &value, true)
					.await
					.map(|_| (eid.clone(), true))
			} else {
				let (eid, val) = if skip_sig_verify {
					(eid, value)
				} else {
					// Build RawValue for sig verification from the canonical object.
					// Strip event_id for v3+ rooms (not part of signed content).
					// V1/V2 rooms require event_id for sig verification.
					let mut raw_val = value.clone();
					if room_version != RoomVersionId::V1 && room_version != RoomVersionId::V2 {
						raw_val.remove("event_id");
					}
					let raw = serde_json::value::RawValue::from_string(serde_json::to_string(
						&raw_val,
					)?)
					.map_err(|e| err!("raw value: {e}"))?;

					match self
						.services
						.server_keys
						.validate_and_add_event_id(&raw, &room_version)
						.await
					{
						| Ok(result) => result,
						| Err(e) => {
							// Sig verification failed — persist as soft-failed outlier so the
							// event is available for auth chain lookups and state context
							let _eid_clone = eid.clone();

							warn!(
								"import_pdus: Event {eid} failed verification: {e}\n  PDU: {}",
								serde_json::to_string_pretty(&value).unwrap_or_default(),
							);

							// Store as outlier
							self.services.rooms.outlier.add_pdu_outlier(
								&eid,
								&value,
								Some(&room_id),
							);

							// Mark as soft-failed (unverifiable, not proven fraudulent)
							self.services
								.rooms
								.pdu_metadata
								.mark_event_soft_failed(&eid);

							return Ok((eid, false));
						},
					}
				};

				let mut pdu_val = val;
				if room_version != RoomVersionId::V1 && room_version != RoomVersionId::V2 {
					pdu_val.remove("event_id");
				}

				// Local-only auth: handle_outlier_pdu checks auth_events from local DB,
				// runs auth_check, and persists as outlier. auth_events_known=true skips
				// federation fetches for missing auth events.
				let (pdu, _parsed) = self
					.services
					.rooms
					.event_handler
					.handle_outlier_pdu(
						origin,
						create_event.as_ref(),
						&eid,
						&room_id,
						pdu_val,
						true,
						skip_sig_verify,
						Some(&room_version),
					)
					.await?;

				// Promote from outlier to timeline
				self.services
					.rooms
					.timeline
					.promote_outlier(&room_id, &eid)
					.await?;
				let _ = pdu; // used by handle_outlier_pdu internally
				Ok((eid, true))
			}
		}
		.await;

		match result {
			| Ok((eid, true)) => {
				inserted = inserted.saturating_add(1);
				if is_soft_failed {
					self.services
						.rooms
						.pdu_metadata
						.mark_event_soft_failed(&eid);
				}
				if is_rejected {
					self.services.rooms.pdu_metadata.mark_event_rejected(&eid);
				}
			},
			| Ok((_eid, false)) => rejected = rejected.saturating_add(1),
			| Err(e) => {
				warn!("import_pdus: {e}");
				failed = failed.saturating_add(1);
			},
		}

		let done = inserted.saturating_add(failed).saturating_add(rejected);
		if done.is_multiple_of(1000) {
			info!(
				"import_pdus: {done}/{total} ({inserted} ok, {rejected} rejected, {failed} err)"
			);
		}
	}

	let (_, num_true) = self
		.services
		.rooms
		.timeline
		.recalculate_extremities(&room_id, 500, true)
		.await?;

	self.write_str(&format!(
		"\nImported {inserted} PDUs, {rejected} stored as rejected outliers, {failed} errors \
		 out of {total} total for {room_id}. DAG Extremities recalculated (now {num_true} \
		 tips). Run `yolo rebuild-state {room_id}` if you are finished importing."
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
