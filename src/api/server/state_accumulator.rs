use axum::extract::State;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use conduwuit::{Err, Result, err, info};
use ruma::{OwnedEventId, OwnedRoomId};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub(crate) struct StateAccumulatorQuery {
	pub event_id: OwnedEventId,
}

#[derive(Serialize)]
pub(crate) struct StateAccumulatorResponse {
	pub event_id: OwnedEventId,
	pub algorithm: String,
	pub lattice: String,
	pub n_state_events: u64,
	pub digest: String,
}

pub(crate) async fn get_state_accumulator_route(
	State(services): State<crate::State>,
	axum::extract::Path(room_id_str): axum::extract::Path<String>,
	axum::extract::Query(query): axum::extract::Query<StateAccumulatorQuery>,
) -> Result<impl axum::response::IntoResponse> {
	use futures::StreamExt;

	let room_id = OwnedRoomId::try_from(room_id_str)
		.map_err(|_| err!(Request(InvalidParam("Invalid room ID."))))?;

	// Verify we participate in this room
	if !services
		.rooms
		.state_cache
		.server_is_participant(services.globals.server_name(), &room_id)
		.await
	{
		return Err!(Request(Forbidden("This server is not participating in that room.")));
	}

	info!(
		room_id = %room_id,
		event_id = %query.event_id,
		"Serving MSC4500 state accumulator request"
	);

	// TODO: This endpoint lacks federation request signature verification
	// and ACL checks. It should use AccessCheck like state/state_ids endpoints
	// once we have a way to extract the federation origin from custom routes.

	// Verify the event belongs to the requested room
	let pdu = services
		.rooms
		.timeline
		.get_pdu(&query.event_id)
		.await
		.map_err(|_| err!(Request(NotFound("Event not found."))))?;

	if pdu.room_id != Some(room_id.clone()) {
		return Err!(Request(NotFound("Event does not belong to the requested room.")));
	}

	let shortstatehash = services
		.rooms
		.state_accessor
		.pdu_shortstatehash(&query.event_id)
		.await
		.map_err(|_| err!(Request(NotFound("Event not found or has no state."))))?;

	let lthash = services
		.rooms
		.state_compressor
		.get_lthash(shortstatehash)
		.await?;

	let (lattice, digest) = serialize_lthash(&lthash);

	let n_state_events = services
		.rooms
		.state_accessor
		.state_full_shortids(shortstatehash)
		.count()
		.await;

	let response = StateAccumulatorResponse {
		event_id: query.event_id,
		algorithm: "lthash16".to_owned(),
		lattice,
		n_state_events: n_state_events.try_into().unwrap_or_default(),
		digest,
	};

	Ok(axum::Json(response))
}

pub(crate) fn serialize_lthash(lthash: &rezzy::LtHash) -> (String, String) {
	let mut bytes = vec![0_u8; 2048];
	for (i, val) in lthash.0.iter().enumerate() {
		let le = val.to_le_bytes();
		bytes[i.saturating_mul(2)] = le[0];
		bytes[i.saturating_mul(2).saturating_add(1)] = le[1];
	}
	let lattice = URL_SAFE_NO_PAD.encode(&bytes);

	let mut digest = String::with_capacity(64);
	for b in lthash.checksum() {
		use std::fmt::Write;
		write!(&mut digest, "{b:02x}").unwrap();
	}

	(lattice, digest)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_serialize_empty_lthash() {
		let empty_lthash = rezzy::LtHash::ZERO;
		let (lattice, digest) = serialize_lthash(&empty_lthash);

		// The lattice for an empty LtHash is 2048 null bytes.
		// 2048 bytes of 0s encoded in base64url without padding:
		let expected_lattice = "A".repeat(2731);
		assert_eq!(
			lattice, expected_lattice,
			"Lattice encoding must be deterministic URL-safe base64"
		);

		// Checksum format must be 64-character lowercase hex (32 bytes)
		assert_eq!(digest.len(), 64);
		assert!(
			digest
				.chars()
				.all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
		);
	}

	#[test]
	fn test_serialize_populated_lthash() {
		let mut lthash = rezzy::LtHash::ZERO;
		// Add some dummy data to manipulate the lthash state
		let event_id1: OwnedEventId = "$abc:example.com".try_into().unwrap();
		let event_id2: OwnedEventId = "$def:example.com".try_into().unwrap();
		lthash.insert("m.room.name", "", &event_id1);
		lthash.insert("m.room.topic", "", &event_id2);

		let (lattice, digest) = serialize_lthash(&lthash);

		// Lattice must remain exactly 2731 base64url-encoded characters long (2048
		// bytes without padding)
		assert_eq!(lattice.len(), 2731);

		// Ensure checksum is also stable length and lowercase hex format
		assert_eq!(digest.len(), 64);
		assert!(
			digest
				.chars()
				.all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
		);

		// The digest and lattice should no longer be the empty one
		let empty_lthash = rezzy::LtHash::ZERO;
		let (empty_lattice, empty_digest) = serialize_lthash(&empty_lthash);
		assert_ne!(lattice, empty_lattice);
		assert_ne!(digest, empty_digest);
	}
}
