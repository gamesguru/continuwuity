mod acquire;
mod get;
mod keypair;
mod request;
mod sign;
mod verify;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use conduwuit::{
	Result, Server, debug_error, debug_warn, implement, trace,
	utils::{IterStream, timepoint_from_now},
};
use database::{Deserialized, Json, Map};
use futures::StreamExt;
use ruma::{
	CanonicalJsonObject, MilliSecondsSinceUnixEpoch, OwnedServerSigningKeyId, RoomVersionId,
	ServerName, ServerSigningKeyId,
	api::federation::discovery::{ServerSigningKeys, VerifyKey},
	serde::Raw,
	signatures::{Ed25519KeyPair, PublicKeyMap, PublicKeySet},
};
use serde_json::value::RawValue as RawJsonValue;

use crate::{Dep, globals, sending};

pub struct Service {
	keypair: Box<Ed25519KeyPair>,
	verify_keys: VerifyKeys,
	minimum_valid: Duration,
	services: Services,
	db: Data,
}

struct Services {
	globals: Dep<globals::Service>,
	sending: Dep<sending::Service>,
	server: Arc<Server>,
}

struct Data {
	server_signingkeys: Arc<Map>,
}

pub type VerifyKeys = BTreeMap<OwnedServerSigningKeyId, VerifyKey>;
pub type PubKeyMap = PublicKeyMap;
pub type PubKeys = PublicKeySet;

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let minimum_valid = Duration::from_secs(3600);

		let (keypair, verify_keys) = keypair::init(args.db)?;
		debug_assert!(verify_keys.len() == 1, "only one active verify_key supported");

		Ok(Arc::new(Self {
			keypair,
			verify_keys,
			minimum_valid,
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				sending: args.depend::<sending::Service>("sending"),
				server: args.server.clone(),
			},
			db: Data {
				server_signingkeys: args.db["server_signingkeys"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
#[inline]
pub fn keypair(&self) -> &Ed25519KeyPair { &self.keypair }

#[implement(Service)]
#[inline]
pub fn active_key_id(&self) -> &ServerSigningKeyId { self.active_verify_key().0 }

#[implement(Service)]
#[inline]
pub fn active_verify_key(&self) -> (&ServerSigningKeyId, &VerifyKey) {
	debug_assert!(self.verify_keys.len() <= 1, "more than one active verify_key");
	self.verify_keys
		.iter()
		.next()
		.map(|(id, key)| (id.as_ref(), key))
		.expect("missing active verify_key")
}

#[implement(Service)]
pub async fn add_signing_keys(&self, new_keys: ServerSigningKeys) {
	use conduwuit::info;
	use ruma::api::federation::discovery::OldVerifyKey;

	let origin = &new_keys.server_name;

	// (timo) Not atomic, but this is not critical
	let mut keys: ServerSigningKeys = self
		.db
		.server_signingkeys
		.get(origin)
		.await
		.deserialized()
		.unwrap_or_else(|_| {
			// Just insert "now", it doesn't matter
			ServerSigningKeys::new(origin.to_owned(), MilliSecondsSinceUnixEpoch::now())
		});

	// Preserve displaced keys: if a new key has the same ID but different
	// key material, move the old key into old_verify_keys before overwriting.
	// This prevents key loss when servers reuse key IDs after regeneration.
	for (key_id, new_key) in &new_keys.verify_keys {
		if let Some(existing_key) = keys.verify_keys.get(key_id) {
			if existing_key.key != new_key.key {
				let old_fp = key_fingerprint(&existing_key.key);
				let new_fp = key_fingerprint(&new_key.key);
				info!(
					"Preserving displaced key {key_id} for {origin}: {old_fp} -> \
					 old_verify_keys, replaced by {new_fp}"
				);
				keys.old_verify_keys.insert(key_id.clone(), OldVerifyKey {
					expired_ts: keys.valid_until_ts,
					key: existing_key.key.clone(),
				});
			}
		}
	}

	keys.verify_keys.extend(new_keys.verify_keys);
	keys.old_verify_keys.extend(new_keys.old_verify_keys);
	self.db.server_signingkeys.raw_put(origin, Json(&keys));
}

#[implement(Service)]
#[tracing::instrument(skip(self, object), level = "debug")]
pub async fn required_keys_exist(
	&self,
	object: &CanonicalJsonObject,
	version: &RoomVersionId,
) -> bool {
	use ruma::signatures::required_keys;

	trace!(?object, "Checking required keys exist");
	let Ok(required_keys) = required_keys(object, version) else {
		debug_error!("Failed to determine required keys");
		return false;
	};
	trace!(?required_keys, "Required keys to verify event");
	required_keys
		.iter()
		.flat_map(|(server, key_ids)| key_ids.iter().map(move |key_id| (server, key_id)))
		.stream()
		.all(|(server, key_id)| self.verify_key_exists(server, key_id))
		.await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn verify_key_exists(&self, origin: &ServerName, key_id: &ServerSigningKeyId) -> bool {
	type KeysMap<'a> = BTreeMap<&'a ServerSigningKeyId, &'a RawJsonValue>;

	let Ok(keys) = self
		.db
		.server_signingkeys
		.get(origin)
		.await
		.deserialized::<Raw<ServerSigningKeys>>()
	else {
		debug_warn!("No known signing keys found for {origin}");
		return false;
	};

	if let Ok(Some(verify_keys)) = keys.get_field::<KeysMap<'_>>("verify_keys") {
		if verify_keys.contains_key(key_id) {
			return true;
		}
	}

	if let Ok(Some(old_verify_keys)) = keys.get_field::<KeysMap<'_>>("old_verify_keys") {
		if old_verify_keys.contains_key(key_id) {
			return true;
		}
	}

	debug_warn!("Key {key_id} not found for {origin}");
	false
}

#[implement(Service)]
pub async fn verify_keys_for(&self, origin: &ServerName) -> VerifyKeys {
	let mut keys = self
		.signing_keys_for(origin)
		.await
		.map(|keys| merge_old_keys(keys).verify_keys)
		.unwrap_or(BTreeMap::new());

	if self.services.globals.server_is_ours(origin) {
		keys.extend(self.verify_keys.clone().into_iter());
	}

	keys
}

#[implement(Service)]
pub async fn signing_keys_for(&self, origin: &ServerName) -> Result<ServerSigningKeys> {
	self.db.server_signingkeys.get(origin).await.deserialized()
}

#[implement(Service)]
fn minimum_valid_ts(&self) -> MilliSecondsSinceUnixEpoch {
	let timepoint =
		timepoint_from_now(self.minimum_valid).expect("SystemTime should not overflow");
	MilliSecondsSinceUnixEpoch::from_system_time(timepoint).expect("UInt should not overflow")
}

fn merge_old_keys(mut keys: ServerSigningKeys) -> ServerSigningKeys {
	// Merge old keys into verify_keys, but do NOT overwrite existing entries.
	// When a server reuses a key ID (e.g., ed25519:1) with different material,
	// we want the current key to take priority for new events while still
	// having the old key available under a suffixed ID for verification.
	//
	// TODO: ruma's verify_json uses PublicKeyMap (BTreeMap<String, Base64>)
	// which only supports one key per key_id. The `:old` suffix below makes
	// the key available in the map, but PDUs reference the original key_id
	// (e.g., "ed25519:1") so ruma won't try "ed25519:1:old" automatically.
	// To fully support verification of old PDUs signed under reused key IDs,
	// we'd need a custom verification wrapper that retries with all known
	// key materials for a given key_id when the primary key fails.
	for (key_id, old) in &keys.old_verify_keys {
		use std::collections::btree_map::Entry;
		match keys.verify_keys.entry(key_id.clone()) {
			| Entry::Vacant(entry) => {
				entry.insert(VerifyKey::new(old.key.clone()));
			},
			| Entry::Occupied(entry) => {
				// Same key ID but potentially different material — keep active key,
				// add old one under a suffixed ID so both are available
				if entry.get().key != old.key {
					let alt_id = format!("{key_id}:old");
					if let Ok(alt_key_id) = alt_id.try_into() {
						keys.verify_keys
							.entry(alt_key_id)
							.or_insert_with(|| VerifyKey::new(old.key.clone()));
					}
				}
			},
		}
	}

	keys
}

fn extract_key(mut keys: ServerSigningKeys, key_id: &ServerSigningKeyId) -> Option<VerifyKey> {
	keys.verify_keys.remove(key_id).or_else(|| {
		keys.old_verify_keys
			.remove(key_id)
			.map(|old| VerifyKey::new(old.key))
	})
}

fn key_exists(keys: &ServerSigningKeys, key_id: &ServerSigningKeyId) -> bool {
	keys.verify_keys.contains_key(key_id) || keys.old_verify_keys.contains_key(key_id)
}

/// Produce a truncated SHA-256 fingerprint of a key ID + material.
/// Format: `sha256:abcdef012345` (first 12 hex chars / 6 bytes).
#[must_use]
pub fn key_fingerprint(key: &ruma::serde::Base64) -> String {
	use conduwuit::utils::hash::sha256::hash;

	let digest = hash(key.as_bytes());
	format!(
		"sha256:{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
		digest[0], digest[1], digest[2], digest[3], digest[4], digest[5]
	)
}
