//! Adapter layer bridging continuwuity's `PduEvent` types to rezzy's
//! `LeanEvent` and `StateProvider` interfaces.
//!
//! This enables using `rezzy::auth::check_auth` as a drop-in replacement
//! for ruma's `state_res::event_auth::auth_check` throughout the codebase.

use std::collections::HashMap;

use conduwuit_core::matrix::{Event, PduEvent, state_key::StateKey, state_res::RoomVersion};
use rezzy::{auth::StateProvider, types::LeanEvent};
use ruma::{RoomVersionId, events::StateEventType};

/// Map a ruma `RoomVersionId` to rezzy's `StateResVersion`.
///
/// Delegates to `RoomVersion::new()` which already has the canonical
/// version→state_res mapping, then converts to rezzy's enum.
///
/// # Panics
///
/// Panics on unrecognized room versions (via `RoomVersion::new()`).
#[must_use]
pub fn to_state_res_version(room_version_id: &RoomVersionId) -> rezzy::types::StateResVersion {
	let rv = RoomVersion::new(room_version_id).expect("unsupported room version");

	if rv.state_res == RoomVersion::V1.state_res {
		rezzy::types::StateResVersion::V1
	} else if rv.state_res == RoomVersion::V12_1.state_res {
		rezzy::types::StateResVersion::V2_1_1
	} else if rv.state_res == RoomVersion::V12.state_res {
		rezzy::types::StateResVersion::V2_1
	} else {
		rezzy::types::StateResVersion::V2
	}
}

/// Convert a `PduEvent` into a `LeanEvent<String>` suitable for rezzy's
/// auth checking and state resolution APIs.
#[must_use]
pub fn pdu_to_lean(pdu: &PduEvent) -> LeanEvent<String> {
	LeanEvent {
		event_id: pdu.event_id().to_string(),
		event_type: pdu.kind.to_string(),
		sender: pdu.sender().to_string(),
		state_key: pdu.state_key().map(str::to_owned),
		content: pdu.get_content_as_value(),
		prev_events: pdu.prev_events().map(ToString::to_string).collect(),
		auth_events: pdu.auth_events().map(ToString::to_string).collect(),
		origin_server_ts: pdu.origin_server_ts().get().into(),
		depth: pdu.depth().into(),
		..Default::default()
	}
}

/// A `StateProvider` backed by a pre-built HashMap of auth events,
/// keyed by `(event_type, state_key)`.
///
/// This is the primary adapter for all auth check callsites that already
/// have their auth events collected (handle_outlier_pdu, upgrade_outlier_pdu,
/// create.rs).
pub struct PduStateProvider {
	/// Auth events converted to LeanEvent, keyed by (type, state_key).
	events: HashMap<(String, Option<String>), LeanEvent<String>>,
}

impl PduStateProvider {
	/// Build a state provider from a HashMap keyed by `(StateEventType,
	/// StateKey)`.
	///
	/// This is the format used by ruma's auth_check callsites.
	#[must_use]
	pub fn from_ruma_map(auth_events: &HashMap<(StateEventType, StateKey), PduEvent>) -> Self {
		let events = auth_events
			.iter()
			.map(|((ty, sk), pdu)| {
				let key = (ty.to_string(), Some(sk.to_string()));
				(key, pdu_to_lean(pdu))
			})
			.collect();

		Self { events }
	}

	/// Build a state provider from a HashMap keyed by
	/// `(StateEventType, SmallString)` (the format used in create.rs).
	#[must_use]
	pub fn from_smallstr_map(
		auth_events: &HashMap<
			(StateEventType, conduwuit_core::smallstr::SmallString<[u8; 48]>),
			PduEvent,
		>,
	) -> Self {
		let events = auth_events
			.iter()
			.map(|((ty, sk), pdu)| {
				let key = (ty.to_string(), Some(sk.to_string()));
				(key, pdu_to_lean(pdu))
			})
			.collect();

		Self { events }
	}
}

impl StateProvider<String> for PduStateProvider {
	fn get_event(&self, event_type: &str, state_key: Option<&str>) -> Option<&LeanEvent<String>> {
		let key_owned = (event_type.to_owned(), state_key.map(str::to_owned));
		self.events.get(&key_owned)
	}
}

/// Run rezzy's auth check on a PduEvent, converting the result to a
/// continuwuity-compatible `Result<bool>`.
///
/// Returns `Ok(true)` if the event passes auth, `Ok(false)` if it fails.
/// This matches ruma's `auth_check` return signature for drop-in
/// compatibility.
pub fn rezzy_auth_check<S: StateProvider<String>>(
	pdu: &PduEvent,
	state: &S,
	version: rezzy::types::StateResVersion,
) -> bool {
	let lean = pdu_to_lean(pdu);
	rezzy::auth::check_auth(&lean, state, version).is_ok()
}
