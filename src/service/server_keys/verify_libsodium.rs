use conduwuit::{Err, Result, debug, debug_warn};
use ruma::{CanonicalJsonObject, CanonicalJsonValue, RoomVersionId, signatures::Verified};

use super::PubKeyMap;

/// Verify an event's ed25519 signatures using libsodium as a fallback
/// when dalek-based verification fails. This handles interop with
/// Synapse/PyNaCl which uses libsodium's more permissive ed25519
/// implementation.
pub(super) fn verify_event_libsodium(
	event: &CanonicalJsonObject,
	keys: &PubKeyMap,
	room_version: &RoomVersionId,
) -> Result<Verified> {
	use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

	// Extract signatures from the event
	let Some(CanonicalJsonValue::Object(signatures)) = event.get("signatures") else {
		return Err!(BadServerResponse("Event has no signatures object"));
	};

	// Build the signing content: event without "signatures" and "unsigned"
	let mut signing_object = event.clone();
	signing_object.remove("signatures");
	signing_object.remove("unsigned");

	// For room versions >= v4, also remove event_id
	// (v1, v2, v3 include event_id in the signed content; v4+ do not)
	let has_event_id_in_signature =
		matches!(room_version, RoomVersionId::V1 | RoomVersionId::V2 | RoomVersionId::V3);
	if !has_event_id_in_signature {
		signing_object.remove("event_id");
	}

	// Serialize to canonical JSON bytes
	let canonical_bytes = serde_json::to_vec(&signing_object)
		.map_err(|e| conduwuit::err!("JSON serialize: {e}"))?;

	let msg_len = u64::try_from(canonical_bytes.len())
		.map_err(|e| conduwuit::err!("message length overflow: {e}"))?;

	let mut any_verified = false;

	// For each server's signatures
	for (server_name, server_sigs) in signatures {
		let CanonicalJsonValue::Object(server_sigs) = server_sigs else {
			continue;
		};

		// Get the public keys for this server
		let Some(server_keys) = keys.get(server_name.as_str()) else {
			debug_warn!("libsodium fallback: no keys for server {server_name}");
			continue;
		};

		// For each key_id: signature pair
		for (key_id, sig_value) in server_sigs {
			let CanonicalJsonValue::String(sig_b64) = sig_value else {
				continue;
			};

			// Only handle ed25519 keys
			if !key_id.starts_with("ed25519:") {
				continue;
			}

			// Decode the signature (base64, unpadded)
			let sig_bytes = BASE64
				.decode(sig_b64.as_bytes())
				.or_else(|_| {
					use base64::engine::general_purpose::STANDARD_NO_PAD;
					STANDARD_NO_PAD.decode(sig_b64.as_bytes())
				})
				.map_err(|e| conduwuit::err!("base64 decode sig: {e}"))?;

			if sig_bytes.len() != 64 {
				debug_warn!(
					"libsodium fallback: signature length {} != 64 for {server_name}/{key_id}",
					sig_bytes.len()
				);
				continue;
			}

			// Get the public key for this key_id
			let Some(pk_b64) = server_keys.get(key_id.as_str()) else {
				debug_warn!("libsodium fallback: no public key for {server_name}/{key_id}");
				continue;
			};

			let pk_bytes = BASE64
				.decode(pk_b64.as_bytes())
				.or_else(|_| {
					use base64::engine::general_purpose::STANDARD_NO_PAD;
					STANDARD_NO_PAD.decode(pk_b64.as_bytes())
				})
				.map_err(|e| conduwuit::err!("base64 decode pk: {e}"))?;

			if pk_bytes.len() != 32 {
				debug_warn!(
					"libsodium fallback: public key length {} != 32 for {server_name}/{key_id}",
					pk_bytes.len()
				);
				continue;
			}

			// SAFETY: libsodium has been initialized via sodium_init() during
			// service build. All pointer/length arguments are valid:
			// - sig_bytes: exactly 64 bytes (checked above)
			// - canonical_bytes: valid JSON bytes with known length
			// - pk_bytes: exactly 32 bytes (checked above)
			let result = unsafe {
				libsodium_sys::crypto_sign_ed25519_verify_detached(
					sig_bytes.as_ptr(),
					canonical_bytes.as_ptr(),
					msg_len,
					pk_bytes.as_ptr(),
				)
			};

			if result == 0 {
				debug!("libsodium fallback SUCCEEDED for {server_name}/{key_id}");
				any_verified = true;
			} else {
				debug_warn!("libsodium fallback also FAILED for {server_name}/{key_id}");
				return Err!(BadServerResponse(
					"Signature verification failed (both dalek and libsodium)"
				));
			}
		}
	}

	if any_verified {
		// We verified signatures but haven't checked content hash.
		// Return Signatures to indicate sigs OK but content hash unchecked.
		// This is conservative — the event may still be a valid redaction.
		Ok(Verified::Signatures)
	} else {
		Err!(BadServerResponse("No signatures could be verified"))
	}
}
