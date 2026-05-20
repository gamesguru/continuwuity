use ruma::{CanonicalJsonObject, OwnedEventId, RoomVersionId};
use serde_json::value::RawValue as RawJsonValue;

use crate::{Result, err};

/// Generates a correct eventId for the incoming pdu.
///
/// Returns a tuple of the new `EventId` and the PDU as a `BTreeMap<String,
/// CanonicalJsonValue>`.
pub fn gen_event_id_canonical_json(
	pdu: &RawJsonValue,
	room_version_id: &RoomVersionId,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	let mut value: CanonicalJsonObject = serde_json::from_str(pdu.get())
		.map_err(|e| err!(BadServerResponse(warn!("Error parsing incoming event: {e:?}"))))?;

	// Strip the `origin` field for Room Versions >= 3. Some servers (e.g. older
	// Synapse) erroneously inject `origin` into V3+ events when serving them over
	// federation, which breaks the canonical JSON hash and signature verification.
	if room_version_id != &RoomVersionId::V1 && room_version_id != &RoomVersionId::V2 {
		value.remove("origin");
	}

	let is_v12_or_later = !matches!(
		room_version_id,
		RoomVersionId::V1
			| RoomVersionId::V2
			| RoomVersionId::V3
			| RoomVersionId::V4
			| RoomVersionId::V5
			| RoomVersionId::V6
			| RoomVersionId::V7
			| RoomVersionId::V8
			| RoomVersionId::V9
			| RoomVersionId::V10
			| RoomVersionId::V11
	);

	let is_create =
		value.get("type").and_then(ruma::CanonicalJsonValue::as_str) == Some("m.room.create");

	// V12+: strips room_id ONLY from create events
	if is_v12_or_later && is_create {
		value.remove("room_id");
	}

	let event_id = gen_event_id(&value, room_version_id)?;

	Ok((event_id, value))
}

#[cfg(test)]
mod tests {
	use ruma::RoomVersionId;
	use serde_json::json;

	use super::*;

	#[test]
	fn test_v11_strips_nothing() {
		let raw_json = json!({
			"type": "m.room.message",
			"room_id": "!test:example.com",
			"content": {},
			"sender": "@alice:example.com",
		});
		let raw = RawJsonValue::from_string(raw_json.to_string()).unwrap();

		let (_, canonical) = gen_event_id_canonical_json(&raw, &RoomVersionId::V11).unwrap();
		assert!(
			canonical.contains_key("room_id"),
			"V11 should NOT strip room_id for non-create events"
		);
	}

	#[test]
	fn test_v11_create_retains_room_id() {
		let raw_json = json!({
			"type": "m.room.create",
			"room_id": "!test:example.com",
			"content": {},
			"sender": "@alice:example.com",
		});
		let raw = RawJsonValue::from_string(raw_json.to_string()).unwrap();

		let (_, canonical) = gen_event_id_canonical_json(&raw, &RoomVersionId::V11).unwrap();
		assert!(
			canonical.contains_key("room_id"),
			"V11 should NOT strip room_id for create events"
		);
	}

	#[test]
	fn test_v12_strips_only_create() {
		// Non-create event: MUST retain room_id
		let raw_json_msg = json!({
			"type": "m.room.message",
			"room_id": "!test:example.com",
			"content": {},
			"sender": "@alice:example.com",
		});
		let raw_msg = RawJsonValue::from_string(raw_json_msg.to_string()).unwrap();
		let (_, canonical_msg) =
			gen_event_id_canonical_json(&raw_msg, &RoomVersionId::V12).unwrap();
		assert!(
			canonical_msg.contains_key("room_id"),
			"V12 should NOT strip room_id for non-create events"
		);

		// Create event: MUST strip room_id
		let raw_json_create = json!({
			"type": "m.room.create",
			"room_id": "!test:example.com",
			"content": {},
			"sender": "@alice:example.com",
		});
		let raw_create = RawJsonValue::from_string(raw_json_create.to_string()).unwrap();
		let (_, canonical_create) =
			gen_event_id_canonical_json(&raw_create, &RoomVersionId::V12).unwrap();
		assert!(
			!canonical_create.contains_key("room_id"),
			"V12 MUST strip room_id for create events"
		);
	}
}

/// Generates a correct eventId for the incoming pdu.
pub fn gen_event_id(
	value: &CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result<OwnedEventId> {
	let reference_hash = ruma::signatures::reference_hash(value, room_version_id)?;
	let event_id: OwnedEventId = format!("${reference_hash}").try_into()?;

	Ok(event_id)
}
