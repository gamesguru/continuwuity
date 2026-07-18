//! MSC4499 signing-key binding state machine.
//!
//! Storage is additive to the legacy `server_signingkeys` blob: this module
//! is the source of truth for which key body is bound to a given
//! `(server_name, algorithm, key_id)`, and `add_signing_keys` in `mod.rs`
//! rebuilds the legacy blob from the effective view returned here so that
//! existing readers (`verify_keys_for`, `signing_keys_for`,
//! `verify_key_exists`, the notary endpoint, and admin debug commands) keep
//! working unchanged.

use conduwuit::{Result, implement, utils::hash::sha256, warn};
use database::{Deserialized, Json};
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedServerSigningKeyId, ServerName, ServerSigningKeyId,
	api::federation::discovery::VerifyKey, serde::Base64,
};
use serde::{Deserialize, Serialize};

use super::Service;

/// Where a key observation was learned from. Only a direct fetch can
/// promote a provisional (notary-learned) binding to permanent, and only a
/// direct fetch establishes a permanent binding on first observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchSource {
	/// A direct fetch from the origin's `/_matrix/key/v2/server`.
	Direct,
	/// A relayed observation from a `/_matrix/key/v2/query` notary.
	Notary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum BindingStatus {
	/// Learned only via a notary; not yet confirmed by a direct fetch.
	Provisional,
	/// Confirmed by a direct fetch, or learned directly to begin with.
	Permanent,
}

/// A single input observation: one key ID, its body, and the validity
/// metadata that came with it in the response (`valid_until_ts` for
/// `verify_keys` entries, or the retiring `expired_ts` for `old_verify_keys`
/// entries).
pub struct Observation {
	pub key_id: OwnedServerSigningKeyId,
	pub key: Base64,
	pub valid_until_ts: MilliSecondsSinceUnixEpoch,
	pub expired_ts: Option<MilliSecondsSinceUnixEpoch>,
}

/// The permanent record of a single `(server_name, algorithm, key_id)`
/// binding, per MSC4499's Key ID uniqueness requirement.
///
/// `first_seen_key` is the First-Seen-Wins-mandated permanent binding and is
/// never overwritten by a rejected collision. `effective_key` is what
/// verification is actually performed against; the two diverge only while
/// `msc4499_strict_key_caching` is disabled and a collision has been
/// observed. In that observation mode, collisions are logged and recorded
/// but verification keeps following the newest body (matching pre-MSC4499
/// behavior), so turning on data-gathering does not itself cause
/// verification failures. Enabling strict mode makes `effective_key` follow
/// `first_seen_key` unconditionally.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct KeyBinding {
	first_seen_key: Base64,
	first_seen_ts: MilliSecondsSinceUnixEpoch,
	status: BindingStatus,

	effective_key: Base64,
	effective_since_ts: MilliSecondsSinceUnixEpoch,

	valid_until_ts: MilliSecondsSinceUnixEpoch,
	/// First-assignment-wins, independent of key-body collision handling:
	/// once set, later differing values are logged as suspicious and
	/// ignored (see MSC4499 "expired_ts is first-assignment-wins").
	expired_ts: Option<MilliSecondsSinceUnixEpoch>,

	last_observed_ts: MilliSecondsSinceUnixEpoch,
	collision_count: u32,
}

/// The effective (mode-aware) result of recording one key ID's observation,
/// enough for the caller to rebuild the legacy `ServerSigningKeys`
/// verify_keys/old_verify_keys split.
pub(super) struct EffectiveBinding {
	pub key_id: OwnedServerSigningKeyId,
	pub key: VerifyKey,
	pub valid_until_ts: MilliSecondsSinceUnixEpoch,
	pub expired_ts: Option<MilliSecondsSinceUnixEpoch>,
}

fn binding_db_key(server_name: &ServerName, key_id: &ServerSigningKeyId) -> Vec<u8> {
	let mut key = server_name.as_bytes().to_vec();
	key.push(database::SEP);
	key.extend_from_slice(key_id.as_str().as_bytes());
	key
}

fn fingerprint(key: &Base64) -> String {
	sha256::hash(key.as_bytes())
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect()
}

/// A future `expired_ts` beyond this clock-skew allowance is malformed for
/// that key entry, per MSC4499 "Historical event verification".
const EXPIRED_TS_SKEW_ALLOWANCE_MS: u64 = 5 * 60 * 1000;

fn expired_ts_is_sane(expired_ts: MilliSecondsSinceUnixEpoch, now: MilliSecondsSinceUnixEpoch) -> bool {
	let now_ms: u64 = now.get().into();
	let expired_ms: u64 = expired_ts.get().into();
	expired_ms <= now_ms.saturating_add(EXPIRED_TS_SKEW_ALLOWANCE_MS)
}

#[implement(Service)]
pub(super) fn strict_caching_enabled(&self) -> bool {
	self.services.server.config.msc4499_strict_key_caching
}

/// Records a batch of key observations from a single response for
/// `server_name`, applying the First Seen Wins state machine to each key ID
/// independently, and returns the currently-effective `VerifyKey` for every
/// key ID that was accepted (bound or refreshed; entries whose `expired_ts`
/// failed the sanity check are dropped, matching the MSC's "MUST NOT
/// poison the rest of the response payload").
#[implement(Service)]
pub(super) async fn record_observations(
	&self,
	server_name: &ServerName,
	source: FetchSource,
	observations: Vec<Observation>,
) -> Vec<EffectiveBinding> {
	let strict = self.strict_caching_enabled();
	let now = MilliSecondsSinceUnixEpoch::now();
	let mut effective = Vec::with_capacity(observations.len());

	for observation in observations {
		let Observation { key_id, key, valid_until_ts, expired_ts } = observation;

		let expired_ts = match expired_ts {
			| Some(ts) if !expired_ts_is_sane(ts, now) => {
				warn!(
					%server_name, %key_id, ?ts,
					"MSC4499: rejected old_verify_keys entry with expired_ts far in the \
					 future (beyond 5-minute clock-skew allowance); treating as malformed \
					 for this key entry only",
				);
				continue;
			},
			| other => other,
		};

		let db_key = binding_db_key(server_name, &key_id);
		let existing: Option<KeyBinding> = self
			.db
			.server_signingkey_bindings
			.get(&db_key)
			.await
			.deserialized()
			.ok();

		let binding = self.apply_observation(
			server_name,
			&key_id,
			existing,
			source,
			strict,
			now,
			key,
			valid_until_ts,
			expired_ts,
		);

		effective.push(EffectiveBinding {
			key_id,
			key: VerifyKey::new(binding.effective_key.clone()),
			valid_until_ts: binding.valid_until_ts,
			expired_ts: binding.expired_ts,
		});
		self.db.server_signingkey_bindings.raw_put(&db_key, Json(&binding));
	}

	effective
}

#[implement(Service)]
#[allow(clippy::too_many_arguments)]
fn apply_observation(
	&self,
	server_name: &ServerName,
	key_id: &ServerSigningKeyId,
	existing: Option<KeyBinding>,
	source: FetchSource,
	strict: bool,
	now: MilliSecondsSinceUnixEpoch,
	key: Base64,
	valid_until_ts: MilliSecondsSinceUnixEpoch,
	expired_ts: Option<MilliSecondsSinceUnixEpoch>,
) -> KeyBinding {
	let Some(mut binding) = existing else {
		// First-ever observation for this key ID: bind it. A notary
		// observation starts provisional; a direct observation starts (and
		// stays) permanent.
		let status = match source {
			| FetchSource::Direct => BindingStatus::Permanent,
			| FetchSource::Notary => BindingStatus::Provisional,
		};
		return KeyBinding {
			first_seen_key: key.clone(),
			first_seen_ts: now,
			status,
			effective_key: key,
			effective_since_ts: now,
			valid_until_ts,
			expired_ts,
			last_observed_ts: now,
			collision_count: 0,
		};
	};

	binding.last_observed_ts = now;

	if binding.first_seen_key == key {
		// Ordinary refresh of the permanent binding: update validity
		// bookkeeping only. Does not reconcile a prior observation-mode
		// divergence between first_seen_key and effective_key on its own —
		// that divergence is only ever resolved by a later observation of
		// the diverged (effective) body, or by a strict-mode flip.
		binding.valid_until_ts = valid_until_ts;
		apply_expired_ts(&mut binding, expired_ts, server_name, key_id);
		return binding;
	}

	if binding.effective_key == key {
		// Refresh of the currently-effective body (only possible in
		// observation mode, after a prior collision moved effective_key
		// away from first_seen_key).
		binding.valid_until_ts = valid_until_ts;
		return binding;
	}

	// A genuine collision: this body matches neither the permanent
	// first-seen binding nor the currently-served effective key.
	let promotable = binding.status == BindingStatus::Provisional
		&& source == FetchSource::Direct
		&& binding.expired_ts.is_none()
		&& binding.valid_until_ts > now;

	if promotable {
		// Two-tier binding: a direct fetch confirming (or superseding) a
		// still-live provisional notary binding is not a collision, it is
		// promotion. The direct fetch's body becomes the permanent binding.
		warn!(
			%server_name, %key_id,
			old_fingerprint = %fingerprint(&binding.first_seen_key),
			new_fingerprint = %fingerprint(&key),
			"MSC4499: direct fetch overrides provisional notary-learned key binding \
			 (promotion); this now becomes the permanent binding for this key ID",
		);
		binding.first_seen_key = key.clone();
		binding.first_seen_ts = now;
		binding.status = BindingStatus::Permanent;
		binding.effective_key = key;
		binding.effective_since_ts = now;
		binding.valid_until_ts = valid_until_ts;
		binding.expired_ts = expired_ts;
		return binding;
	}

	binding.collision_count = binding.collision_count.saturating_add(1);
	warn!(
		%server_name, %key_id,
		bound_fingerprint = %fingerprint(&binding.first_seen_key),
		conflicting_fingerprint = %fingerprint(&key),
		?source,
		strict,
		collision_count = binding.collision_count,
		"MSC4499: signing key ID collision rejected; the first-observed key body remains \
		 the permanent binding for this key ID",
	);

	if !strict {
		// Observation mode: keep verifying against the newest body so
		// enabling collision logging/recording does not itself cause
		// message loss. first_seen_key above is left untouched so a later
		// switch to strict mode has the correct permanent binding recorded.
		binding.effective_key = key;
		binding.effective_since_ts = now;
		binding.valid_until_ts = valid_until_ts;
	}

	binding
}

fn apply_expired_ts(
	binding: &mut KeyBinding,
	expired_ts: Option<MilliSecondsSinceUnixEpoch>,
	server_name: &ServerName,
	key_id: &ServerSigningKeyId,
) {
	let Some(new_ts) = expired_ts else { return };

	match binding.expired_ts {
		| None => binding.expired_ts = Some(new_ts),
		| Some(existing_ts) if existing_ts != new_ts => {
			warn!(
				%server_name, %key_id, ?existing_ts, ?new_ts,
				"MSC4499: ignoring differing expired_ts for an already-retired key ID; \
				 the first-observed expired_ts remains binding",
			);
		},
		| Some(_) => {},
	}
}
