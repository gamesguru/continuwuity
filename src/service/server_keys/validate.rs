use std::collections::HashSet;

use conduwuit::{Err, Result};

/// MSC4499: Scan raw JSON bytes for duplicate keys within `verify_keys` and
/// `old_verify_keys` objects. Returns Err if any duplicate keys are found.
///
/// This must run on the raw bytes BEFORE serde_json deserialization, because
/// serde_json silently deduplicates (last-key-wins). Without this pre-scan,
/// a payload with `{"verify_keys": {"ed25519:foo": ..., "ed25519:foo": ...}}`
/// would be silently accepted with the second value winning.
pub(super) fn check_no_duplicate_json_keys(raw: &str) -> Result {
	// Scan the raw bytes for duplicate keys and count limits FIRST, before
	// full deserialization. This prevents memory exhaustion if a rogue server
	// sends 100,000 keys, since we reject it before allocating a JSON tree.
	let bytes = raw.as_bytes();
	let vk_count = check_raw_duplicates(bytes, b"verify_keys")?;
	let ovk_count = check_raw_duplicates(bytes, b"old_verify_keys")?;

	// MSC4499: "If a single key response payload contains more than 50 keys in its
	// verify_keys dictionary, receiving servers MUST treat the entire response
	// payload as malformed/hostile and reject it."
	if vk_count > 50 {
		return Err!(BadServerResponse("Too many keys in verify_keys (limit: 50)"));
	}

	// MSC4499: "If a single key response payload contains more than 1000 keys in
	// its old_verify_keys dictionary, receiving servers SHOULD treat the entire
	// response payload as malformed/hostile and reject it."
	// Note: We updated our quota to 3,000 keys total to accommodate the "hostile"
	// active-key spillover behavior, so the old_verify_keys ceiling is also 3,000.
	if ovk_count > 3000 {
		return Err!(BadServerResponse("Too many keys in old_verify_keys (limit: 3000)"));
	}

	let value: serde_json::Value =
		serde_json::from_str(raw).map_err(|e| conduwuit::err!(BadServerResponse("{e}")))?;

	let Some(obj) = value.as_object() else {
		return Ok(());
	};

	// Cross-map collision: same key ID with different body across sections
	if let (Some(verify_keys), Some(old_verify_keys)) = (
		obj.get("verify_keys").and_then(|v| v.as_object()),
		obj.get("old_verify_keys").and_then(|v| v.as_object()),
	) {
		for (key_id, old_val) in old_verify_keys {
			if let Some(verify_val) = verify_keys.get(key_id) {
				let old_key = old_val.get("key").and_then(|v| v.as_str());
				let new_key = verify_val.get("key").and_then(|v| v.as_str());
				if old_key != new_key {
					return Err!(BadServerResponse(
						"Cross-map collision: key ID {key_id} has different bodies in \
						 verify_keys and old_verify_keys"
					));
				}
			}
		}
	}

	Ok(())
}

/// Scan raw JSON bytes for duplicate keys within a named top-level object.
/// Operates entirely on `&[u8]` with checked/saturating arithmetic.
fn check_raw_duplicates(bytes: &[u8], section_name: &[u8]) -> Result<usize> {
	// Build the search pattern: `"section_name"`
	let mut pattern = Vec::with_capacity(section_name.len().saturating_add(2));
	pattern.push(b'"');
	pattern.extend_from_slice(section_name);
	pattern.push(b'"');

	// Find the section in the raw JSON
	let Some(section_start) = find_subsequence(bytes, &pattern) else {
		return Ok(0);
	};

	// Advance past `"section_name"` and find ':'
	let past_key = section_start.saturating_add(pattern.len());
	let Some(colon_offset) = find_byte(&bytes[past_key..], b':') else {
		return Ok(0);
	};

	// Advance past ':' and find '{'
	let past_colon = past_key.saturating_add(colon_offset).saturating_add(1);
	let Some(brace_offset) = find_byte(&bytes[past_colon..], b'{') else {
		return Ok(0);
	};

	let obj_bytes = &bytes[past_colon.saturating_add(brace_offset)..];

	scan_object_for_duplicate_keys(obj_bytes, section_name)
}

/// Walk a JSON object's top-level keys (depth == 1) and detect duplicates.
fn scan_object_for_duplicate_keys(obj_bytes: &[u8], section_name: &[u8]) -> Result<usize> {
	let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();
	let mut depth = 0_u32;
	let mut i = 0_usize;
	let len = obj_bytes.len();

	while i < len {
		match obj_bytes[i] {
			| b'{' => {
				depth = depth.saturating_add(1);
				i = i.saturating_add(1);
			},
			| b'}' => {
				if depth <= 1 {
					break;
				}
				depth = depth.saturating_sub(1);
				i = i.saturating_add(1);
			},
			| b'"' if depth == 1 => {
				// At depth 1 inside the section object — potential key
				i = i.saturating_add(1); // skip opening quote
				let key_start = i;
				while i < len && obj_bytes[i] != b'"' {
					if obj_bytes[i] == b'\\' {
						i = i.saturating_add(1); // skip escaped char
					}
					i = i.saturating_add(1);
				}
				if i >= len {
					break;
				}
				let key = &obj_bytes[key_start..i];
				i = i.saturating_add(1); // skip closing quote

				// Check if followed by ':' (making it an object key, not a value)
				let mut j = i;
				while j < len && obj_bytes[j].is_ascii_whitespace() {
					j = j.saturating_add(1);
				}
				if j < len && obj_bytes[j] == b':' {
					if contains_escapes(key) {
						let section = std::str::from_utf8(section_name).unwrap_or("<invalid>");
						let key_str = std::str::from_utf8(key).unwrap_or("<invalid utf-8>");
						return Err!(BadServerResponse(
							"JSON key '{key_str}' in {section} contains illegal escape sequences"
						));
					}
					if !seen_keys.insert(key.to_vec()) {
						let section = std::str::from_utf8(section_name).unwrap_or("<invalid>");
						let key_str = std::str::from_utf8(key).unwrap_or("<invalid utf-8>");
						return Err!(BadServerResponse(
							"Duplicate JSON key '{key_str}' in {section}"
						));
					}
				}
			},
			| _ => {
				i = i.saturating_add(1);
			},
		}
	}

	Ok(seen_keys.len())
}

/// Find the first occurrence of `needle` in `haystack`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack
		.windows(needle.len())
		.position(|window| window == needle)
}

/// Find the first occurrence of a single byte in a slice.
fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
	haystack.iter().position(|&b| b == needle)
}

/// Check if a key contains any backslashes (JSON escapes).
/// Matrix key IDs should never contain escapes, and allowing them
/// makes duplicate detection vulnerable to semantic bypasses.
fn contains_escapes(bytes: &[u8]) -> bool { bytes.contains(&b'\\') }

#[cfg(test)]
mod tests {
	use super::check_no_duplicate_json_keys;

	#[test]
	fn no_duplicates() {
		let json =
			r#"{"verify_keys": {"ed25519:a": {"key": "AAA"}, "ed25519:b": {"key": "BBB"}}}"#;
		assert!(check_no_duplicate_json_keys(json).is_ok());
	}

	#[test]
	fn duplicate_in_verify_keys() {
		let json =
			r#"{"verify_keys": {"ed25519:a": {"key": "AAA"}, "ed25519:a": {"key": "BBB"}}}"#;
		assert!(check_no_duplicate_json_keys(json).is_err());
	}

	#[test]
	fn duplicate_in_old_verify_keys() {
		let json = r#"{"old_verify_keys": {"ed25519:a": {"key": "AAA", "expired_ts": 1}, "ed25519:a": {"key": "BBB", "expired_ts": 2}}}"#;
		assert!(check_no_duplicate_json_keys(json).is_err());
	}

	#[test]
	fn cross_map_collision() {
		let json = r#"{"verify_keys": {"ed25519:a": {"key": "AAA"}}, "old_verify_keys": {"ed25519:a": {"key": "BBB", "expired_ts": 1}}}"#;
		assert!(check_no_duplicate_json_keys(json).is_err());
	}

	#[test]
	fn cross_map_same_body_is_legal() {
		let json = r#"{"verify_keys": {"ed25519:a": {"key": "AAA"}}, "old_verify_keys": {"ed25519:a": {"key": "AAA", "expired_ts": 1}}}"#;
		assert!(check_no_duplicate_json_keys(json).is_ok());
	}

	#[test]
	fn rejects_escaped_key() {
		let json = r#"{"verify_keys": {"ed25519:\u0061": {"key": "BBB"}}}"#;
		assert!(check_no_duplicate_json_keys(json).is_err());
	}
}
