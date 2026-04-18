use conduwuit::{
	Err, Event, Result, RoomVersion, err, implement, matrix::event::gen_event_id_canonical_json,
	result::FlatOk,
};
use ruma::{CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedRoomId, RoomVersionId};
use serde_json::value::RawValue as RawJsonValue;

type Parsed = (OwnedRoomId, OwnedEventId, CanonicalJsonObject);

const MAX_AUTH_EVENTS_ROOM_ID_FALLBACK: usize = 10;

#[implement(super::Service)]
pub async fn parse_incoming_pdu(&self, pdu: &RawJsonValue) -> Result<Parsed> {
	let value = serde_json::from_str::<CanonicalJsonObject>(pdu.get()).map_err(|e| {
		err!(BadServerResponse(debug_warn!("Error parsing incoming event {e:?}")))
	})?;
	let event_type = value
		.get("type")
		.and_then(CanonicalJsonValue::as_str)
		.ok_or_else(|| err!(Request(InvalidParam("Missing or invalid type in pdu"))))?;

	let room_id: OwnedRoomId = if let Some(room_id_val) = value.get("room_id") {
		room_id_val
			.as_str()
			.map(OwnedRoomId::parse)
			.flat_ok_or(err!(Request(InvalidParam("Invalid room_id in pdu"))))?
	} else if event_type == "m.room.create" {
		// v12 rooms might have no room_id in the create event. We'll need to check the
		// content.room_version
		let content = value
			.get("content")
			.and_then(CanonicalJsonValue::as_object)
			.ok_or_else(|| err!(Request(InvalidParam("Missing or invalid content in pdu"))))?;
		let room_version = content
			.get("room_version")
			.and_then(CanonicalJsonValue::as_str)
			.unwrap_or("1");
		let vi = RoomVersionId::try_from(room_version).unwrap_or(RoomVersionId::V1);
		let vf = RoomVersion::new(&vi).expect("supported room version");
		if vf.room_ids_as_hashes {
			let (event_id, _) = gen_event_id_canonical_json(pdu, &vi).map_err(|e| {
				err!(Request(InvalidParam("Could not convert event to canonical json: {e}")))
			})?;
			OwnedRoomId::parse(event_id.as_str().replace('$', "!")).expect("valid room ID")
		} else {
			// V11 or below room, room_id must be present
			return Err!(Request(InvalidParam("Invalid or missing room_id in pdu")));
		}
	} else {
		// V12 non-create event without room_id
		// Try to find it from auth_events, but do not allow untrusted input to
		// trigger an unbounded number of sequential DB lookups.
		let auth_events = value
			.get("auth_events")
			.and_then(|v| v.as_array())
			.ok_or_else(|| err!(Request(InvalidParam("Missing room_id in PDU"))))?;

		if auth_events.len() > MAX_AUTH_EVENTS_ROOM_ID_FALLBACK {
			return Err!(Request(InvalidParam("Missing room_id in PDU")));
		}

		let mut found_room_id = None;
		for auth_event_id in auth_events {
			if let Some(auth_event_id) = auth_event_id.as_str() {
				if let Ok(auth_event_id) = OwnedEventId::parse(auth_event_id) {
					if let Ok(pdu) = self.services.timeline.get_pdu(&auth_event_id).await {
						found_room_id = pdu.room_id().map(ToOwned::to_owned);
						if found_room_id.is_some() {
							if found_room_id.is_some() {
								break;
							}
						}
					}
				}
			}
		}

		found_room_id.ok_or_else(|| err!(Request(InvalidParam("Missing room_id in PDU"))))?
	};

	let room_version_id = self
		.services
		.state
		.get_room_version(&room_id)
		.await
		.unwrap_or(RoomVersionId::V1);
	let (event_id, value) = gen_event_id_canonical_json(pdu, &room_version_id).map_err(|e| {
		err!(Request(InvalidParam("Could not convert event to canonical json: {e}")))
	})?;
	Ok((room_id, event_id, value))
}
