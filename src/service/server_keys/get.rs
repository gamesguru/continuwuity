use std::borrow::Borrow;

use conduwuit::{Err, Result, debug_error, debug_warn, implement, trace};
use database::Deserialized;
use ruma::{
	CanonicalJsonObject, MilliSecondsSinceUnixEpoch, RoomVersionId, ServerName,
	ServerSigningKeyId, api::federation::discovery::VerifyKey,
};

use super::{PubKeyMap, PubKeys, extract_key};

#[implement(super::Service)]
pub async fn get_event_keys(
	&self,
	object: &CanonicalJsonObject,
	version: &RoomVersionId,
) -> Result<PubKeyMap> {
	use ruma::signatures::required_keys;

	let required = match required_keys(object, version) {
		| Ok(required) => required,
		| Err(e) => {
			debug_error!("Failed to determine keys required to verify: {e}");
			return Err!(BadServerResponse("Failed to determine keys required to verify: {e}"));
		},
	};
	trace!(?required, "Keys required to verify event");

	// Extract origin_server_ts to enforce expired key rejection per MSC4499.
	// Events signed by a key whose expired_ts <= origin_server_ts must be rejected.
	let origin_server_ts = object.get("origin_server_ts").and_then(|v| match v {
		| ruma::CanonicalJsonValue::Integer(ts) => {
			let uint = ruma::UInt::new(u64::try_from(i128::from(*ts)).ok()?)?;
			Some(MilliSecondsSinceUnixEpoch(uint))
		},
		| _ => None,
	});

	let mut keys = PubKeyMap::new();
	for (server, key_ids) in &required {
		let pubkeys = self
			.get_pubkeys_for_event(
				server.borrow(),
				key_ids.iter().map(Borrow::borrow),
				origin_server_ts,
			)
			.await;
		keys.insert(server.to_string(), pubkeys);
	}

	Ok(keys)
}

#[implement(super::Service)]
pub async fn get_pubkeys<'a, S, K>(&self, batch: S) -> PubKeyMap
where
	S: Iterator<Item = (&'a ServerName, K)> + Send,
	K: Iterator<Item = &'a ServerSigningKeyId> + Send,
{
	let mut keys = PubKeyMap::new();
	for (server, key_ids) in batch {
		let pubkeys = self.get_pubkeys_for(server, key_ids).await;
		keys.insert(server.into(), pubkeys);
	}

	keys
}

#[implement(super::Service)]
pub async fn get_pubkeys_for<'a, I>(&self, origin: &ServerName, key_ids: I) -> PubKeys
where
	I: Iterator<Item = &'a ServerSigningKeyId> + Send,
{
	let mut keys = PubKeys::new();
	for key_id in key_ids {
		if let Ok(verify_key) = self.get_verify_key(origin, key_id).await {
			keys.insert(key_id.into(), verify_key.key);
		}
	}

	keys
}

/// Like `get_pubkeys_for`, but filters out expired keys when an event timestamp
/// is provided. Per MSC4499: an event signed at time T is valid iff T <
/// expired_ts. Keys in `old_verify_keys` whose `expired_ts` <=
/// `origin_server_ts` are excluded.
#[implement(super::Service)]
pub async fn get_pubkeys_for_event<'a, I>(
	&self,
	origin: &ServerName,
	key_ids: I,
	origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
) -> PubKeys
where
	I: Iterator<Item = &'a ServerSigningKeyId> + Send,
{
	let mut keys = PubKeys::new();

	for key_id in key_ids {
		if let Ok(verify_key) = self.get_verify_key(origin, key_id).await {
			// Check if this key is expired relative to the event's timestamp
			if let Some(event_ts) = origin_server_ts {
				if self
					.is_key_expired_for_event(origin, key_id, event_ts)
					.await
				{
					debug_warn!(
						%origin, %key_id,
						"Rejecting expired key for event verification \
						 (key expired before event origin_server_ts)"
					);
					continue;
				}
			}
			keys.insert(key_id.into(), verify_key.key);
		}
	}

	keys
}

/// Checks if a key from old_verify_keys has an expired_ts that is at or before
/// the given event timestamp, meaning it should not be used to verify that
/// event.
#[implement(super::Service)]
async fn is_key_expired_for_event(
	&self,
	origin: &ServerName,
	key_id: &ServerSigningKeyId,
	event_ts: MilliSecondsSinceUnixEpoch,
) -> bool {
	// Check the origin key record
	if let Ok(server_keys) = self.signing_keys_for(origin).await {
		if let Some(old_key) = server_keys.old_verify_keys.get(key_id) {
			return old_key.expired_ts <= event_ts;
		}
	}

	// Check the historical key record
	let mut historical_key = origin.as_bytes().to_vec();
	historical_key.extend_from_slice(b"\0historical");

	if let Ok(historical_keys) =
		self.db
			.server_signingkeys
			.get(&historical_key)
			.await
			.deserialized::<ruma::api::federation::discovery::ServerSigningKeys>()
	{
		if let Some(old_key) = historical_keys.old_verify_keys.get(key_id) {
			return old_key.expired_ts <= event_ts;
		}
	}

	false
}

#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn get_verify_key(
	&self,
	origin: &ServerName,
	key_id: &ServerSigningKeyId,
) -> Result<VerifyKey> {
	let notary_first = self.services.server.config.query_trusted_key_servers_first;
	let notary_only = self.services.server.config.only_query_trusted_key_servers;

	if let Some(result) = self.verify_keys_for(origin).await.remove(key_id) {
		trace!("Found key in cache");
		return Ok(result);
	}

	if notary_first {
		if let Ok(result) = self.get_verify_key_from_notaries(origin, key_id).await {
			return Ok(result);
		}
	}

	if !notary_only {
		if let Ok(result) = self.get_verify_key_from_origin(origin, key_id).await {
			return Ok(result);
		}
	}

	if !notary_first {
		if let Ok(result) = self.get_verify_key_from_notaries(origin, key_id).await {
			return Ok(result);
		}
	}

	Err!(BadServerResponse(debug_error!(
		%key_id,
		%origin,
		"Failed to fetch federation signing-key"
	)))
}

#[implement(super::Service)]
async fn get_verify_key_from_notaries(
	&self,
	origin: &ServerName,
	key_id: &ServerSigningKeyId,
) -> Result<VerifyKey> {
	for notary in self.services.globals.trusted_servers() {
		if let Ok(server_keys) = self.notary_request(notary, origin).await {
			for server_key in server_keys.clone() {
				if let Err(e) = self.add_signing_keys(server_key).await {
					debug_error!("Failed to add signing keys: {e}");
				}
			}

			for server_key in server_keys {
				if let Some(result) = extract_key(server_key, key_id) {
					return Ok(result);
				}
			}
		}
	}

	Err!(Request(NotFound("Failed to fetch signing-key from notaries")))
}

#[implement(super::Service)]
async fn get_verify_key_from_origin(
	&self,
	origin: &ServerName,
	key_id: &ServerSigningKeyId,
) -> Result<VerifyKey> {
	if let Ok(server_key) = self.server_request(origin).await {
		if let Err(e) = self.add_signing_keys(server_key.clone()).await {
			debug_error!("Failed to add signing keys: {e}");
		}
		if let Some(result) = extract_key(server_key, key_id) {
			return Ok(result);
		}
	}

	Err!(Request(NotFound("Failed to fetch signing-key from origin")))
}
