//! Adapter layer bridging continuwuity's `PduEvent` types to rezzy's
//! `LeanEvent` and `StateProvider` interfaces.
//!
//! This enables using `rezzy::auth::check_auth` as a drop-in replacement
//! for ruma's `state_res::event_auth::auth_check` throughout the codebase.
//!
//! `Pdu` implements `rezzy::RawEvent` directly (in `pdu.rs`), so
//! `ParsedEvent::new(&pdu)` gives you `DagNode + EventLike` with zero
//! boilerplate.

use std::collections::HashMap;

use conduwuit_core::matrix::{Event, PduEvent, state_key::StateKey, state_res::RoomVersion};
use rezzy::{LeanEvent, StateResVersion, auth::StateProvider};
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
pub fn to_state_res_version(room_version_id: &RoomVersionId) -> StateResVersion {
	let rv = RoomVersion::new(room_version_id).expect("unsupported room version");

	if rv.state_res == RoomVersion::V1.state_res {
		StateResVersion::V1
	} else if rv.state_res == RoomVersion::V12_1.state_res {
		StateResVersion::V2_1_1
	} else if rv.state_res == RoomVersion::V12.state_res {
		StateResVersion::V2_1
	} else {
		StateResVersion::V2
	}
}

/// Convert any `Event` into a `LeanEvent<String>` suitable for rezzy's
/// auth checking and state resolution APIs.
#[must_use]
pub fn pdu_to_lean<E: Event>(pdu: &E) -> LeanEvent<String> {
	LeanEvent {
		event_id: pdu.event_id().to_string(),
		event_type: pdu.kind().to_string(),
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

	/// Add the `m.room.create` event explicitly to the state provider.
	///
	/// This is necessary for Room Versions 11/12 (v12+), where `m.room.create`
	/// is omitted from the event's `auth_events` list but is still required by
	/// state resolution and auth checks.
	#[must_use]
	pub fn with_create_event<E: Event>(mut self, create_event: Option<&E>) -> Self {
		if let Some(pdu) = create_event {
			let key = (StateEventType::RoomCreate.to_string(), Some(String::new()));
			self.events.insert(key, pdu_to_lean(pdu));
		}
		self
	}

	/// Build a state provider from the current room state by fetching key
	/// auth events from the database.
	///
	/// Fetches `m.room.create`, `m.room.power_levels`, and `m.room.join_rules`
	/// from the room's current state snapshot. This is useful for query-style
	/// checks (e.g. `rezzy::auth::user::user_can_invite`) where a pre-built
	/// auth events map is not available.
	pub async fn from_room_state(
		room_id: &ruma::RoomId,
		state_accessor: &crate::rooms::state_accessor::Service,
	) -> Self {
		let mut events = HashMap::new();

		// Fetch power levels (most important for PL queries)
		if let Ok(pdu) = state_accessor
			.room_state_get(room_id, &StateEventType::RoomPowerLevels, "")
			.await
		{
			let key = (StateEventType::RoomPowerLevels.to_string(), Some(String::new()));
			events.insert(key, pdu_to_lean(&pdu));
		}

		// Fetch create event (needed for V12+ implicit creator PL)
		if let Ok(pdu) = state_accessor
			.room_state_get(room_id, &StateEventType::RoomCreate, "")
			.await
		{
			let key = (StateEventType::RoomCreate.to_string(), Some(String::new()));
			events.insert(key, pdu_to_lean(&pdu));
		}

		// Fetch join rules (needed for restricted join checks)
		if let Ok(pdu) = state_accessor
			.room_state_get(room_id, &StateEventType::RoomJoinRules, "")
			.await
		{
			let key = (StateEventType::RoomJoinRules.to_string(), Some(String::new()));
			events.insert(key, pdu_to_lean(&pdu));
		}

		Self { events }
	}
}

impl StateProvider<String> for PduStateProvider {
	fn get_event(&self, event_type: &str, state_key: &str) -> Option<&LeanEvent<String>> {
		let key_owned = (event_type.to_owned(), Some(state_key.to_owned()));
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
	version: StateResVersion,
) -> bool {
	let lean = pdu_to_lean(pdu);
	match rezzy::auth::check_auth(&lean, state, version, None) {
		| Ok(()) => true,
		| Err(e) => {
			tracing::warn!("rezzy auth check failed: {e}");
			false
		},
	}
}

/// Convenience wrapper bundling a [`PduStateProvider`] with the resolved
/// [`StateResVersion`] for a room.  Eliminates boilerplate at call sites
/// that need both.
pub struct RoomStateProvider {
	pub provider: PduStateProvider,
	pub version: StateResVersion,
}

impl RoomStateProvider {
	/// Build from the current room state.
	///
	/// # Errors
	///
	/// Returns an error if the room version cannot be determined (missing
	/// create event, DB corruption, or nonexistent room).
	pub async fn new(
		room_id: &ruma::RoomId,
		state_accessor: &crate::rooms::state_accessor::Service,
	) -> conduwuit_core::Result<Self> {
		let provider = PduStateProvider::from_room_state(room_id, state_accessor).await;

		let version = state_accessor.get_state_res_version(room_id).await?;

		Ok(Self { provider, version })
	}
}
