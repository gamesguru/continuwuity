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

	// For V12+, the `room_id` is derived from the `m.room.create` event's hash.
	// Therefore, the signed event content cannot contain `room_id`. If remote
	// servers erroneously inject it, we must strip it before hashing.
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

	if is_v12_or_later
		&& value.get("type").and_then(ruma::CanonicalJsonValue::as_str) == Some("m.room.create")
	{
		value.remove("room_id");
	}

	let event_id = gen_event_id(&value, room_version_id)?;

	Ok((event_id, value))
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
