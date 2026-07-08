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
