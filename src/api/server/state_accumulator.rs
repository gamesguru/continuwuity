use axum::extract::State;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use conduwuit::{err, info, Err, Result};
use ruma::{api::ruma_api, OwnedEventId, OwnedRoomId};

use super::AccessCheck;

ruma_api! {
	metadata: {
		description: "Get the MSC4500 state accumulator for a room at a specific event",
		method: GET,
		name: "get_state_accumulator",
		unstable_path: "/_matrix/federation/unstable/tk.nutra.msc4500/state_accumulator/:room_id",
		rate_limited: false,
		authentication: ServerSignatures,
	}

	request: {
		#[ruma_api(path)]
		pub room_id: OwnedRoomId,

		#[ruma_api(query)]
		pub event_id: OwnedEventId,
	}

	response: {
		pub event_id: OwnedEventId,
		pub algorithm: String,
		pub lattice: String,
		pub n_state_events: ruma::UInt,
		pub digest: String,
	}
}

pub(crate) async fn get_state_accumulator_route(
	State(services): State<crate::State>,
	body: crate::Ruma<Request>,
) -> Result<Response> {
	AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: Some(&body.event_id),
	}
	.check()
	.await?;

	info!(
		origin = body.origin().as_str(),
		room_id = %body.room_id,
		event_id = %body.event_id,
		"Serving MSC4500 state accumulator request"
	);

	let shortstatehash = services
		.rooms
		.state_accessor
		.pdu_shortstatehash(&body.event_id)
		.await
		.map_err(|_| err!(Request(NotFound("Event not found or has no state."))))?
		.ok_or_else(|| err!(Request(NotFound("Event has no state hash."))))?;

	let lthash = services
		.rooms
		.state_compressor
		.get_lthash(shortstatehash)
		.await?;

	let mut bytes = vec![0_u8; 2048];
	for (i, val) in lthash.0.iter().enumerate() {
		let le = val.to_le_bytes();
		bytes[i * 2] = le[0];
		bytes[i * 2 + 1] = le[1];
	}
	let lattice = URL_SAFE_NO_PAD.encode(&bytes);

	let mut digest = String::with_capacity(64);
	for b in lthash.checksum() {
		use std::fmt::Write;
		write!(&mut digest, "{:02x}", b).unwrap();
	}

	let n_state_events = services
		.rooms
		.state_accessor
		.state_full_shortids(shortstatehash)
		.await?
		.len();

	Ok(Response {
		event_id: body.event_id.clone(),
		algorithm: "lthash16".to_owned(),
		lattice,
		n_state_events: n_state_events.try_into().unwrap_or_default(),
		digest,
	})
}
