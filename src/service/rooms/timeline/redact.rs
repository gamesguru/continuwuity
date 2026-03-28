use conduwuit_core::{
	Result, err, implement,
	matrix::event::Event,
	utils::{self},
};
use ruma::EventId;

use super::ExtractBody;
use crate::rooms::short::ShortRoomId;

/// Replace a PDU with the redacted form.
#[implement(super::Service)]
#[tracing::instrument(name = "redact", level = "debug", skip(self))]
pub async fn redact_pdu<Pdu: Event + Send + Sync>(
	&self,
	event_id: &EventId,
	reason: &Pdu,
	shortroomid: ShortRoomId,
) -> Result {
	// TODO: Don't reserialize, keep original json
	let Ok(pdu_id) = self.get_pdu_id(event_id).await else {
		// If event does not exist, just noop
		return Ok(());
	};

	let mut pdu = self
		.get_pdu_from_id(&pdu_id)
		.await
		.map(Event::into_pdu)
		.map_err(|e| {
			err!(Database(error!(?pdu_id, %event_id, ?e, "PDU ID points to invalid PDU.")))
		})?;

	if let Ok(content) = pdu.get_content::<ExtractBody>() {
		if let Some(body) = content.body {
			self.services
				.search
				.deindex_pdu(shortroomid, &pdu_id, &body);
		}
	}

	let room_version_id = self
		.services
		.state
		.get_room_version(&pdu.room_id_or_hash())
		.await?;

	// Capture pre-redaction JSON for the audit table if configured.
	let pre_redaction_obj = utils::to_canonical_object(&pdu).ok();

	pdu.redact(&room_version_id, reason.to_value())?;

	let obj = utils::to_canonical_object(&pdu).map_err(|e| {
		err!(Database(error!(%event_id, ?e, "Failed to convert PDU to canonical JSON")))
	})?;

	// Persist audit copy before overwriting the main record.
	if let Some(pre_obj) = pre_redaction_obj {
		use conduwuit_core::config::AuditCopies;
		let audit = &self.services.server.config.redactions_persist_audit_copies;
		let sender_server = pdu.sender().server_name();
		let local_server = self.services.globals.server_name();

		let should_backup = match audit {
			| AuditCopies::None => false,
			| AuditCopies::All => true,
			| AuditCopies::Local => sender_server == local_server,
			| AuditCopies::Servers(servers) => servers.iter().any(|s| s == sender_server),
		};

		if should_backup {
			self.backup_pdu(&pdu_id, &pre_obj);
		}
	}

	self.replace_pdu(&pdu_id, &obj).await
}
