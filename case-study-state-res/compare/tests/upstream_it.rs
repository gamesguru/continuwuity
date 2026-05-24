//! Integration tests running upstream `ruma-state-res` fixtures against
//! Conduwuit's engine.

use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque},
	fs,
	path::PathBuf,
};

use conduwuit_core::{
	Result,
	matrix::{Event, event::PduEvent, state_res, state_res::StateMap},
};
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomVersionId};

// ==========================================
// JSON Pre-processor & Snapshot Parser
// ==========================================

fn parse_upstream_fixture(fixture_name: &str) -> Vec<PduEvent> {
	let path = PathBuf::from(format!(
		"../../ruma-upstream/crates/ruma-state-res/tests/it/resolve/fixtures/{}.json",
		fixture_name
	));

	let content = fs::read_to_string(&path)
		.unwrap_or_else(|_| panic!("Failed to read fixture: {:?}", path));

	let json_array: Vec<CanonicalJsonValue> =
		serde_json::from_str(&content).expect("Fixture is not valid JSON");

	json_array
		.into_iter()
		.map(|v| {
			let mut obj = match v {
				| CanonicalJsonValue::Object(o) => o,
				| _ => panic!("Event is not a JSON object"),
			};

			// Inject a dummy room_id if missing (upstream tests often omit it)
			if !obj.contains_key("room_id") {
				obj.insert(
					"room_id".to_string(),
					CanonicalJsonValue::String("!test_room:conduwuit.local".to_string()),
				);
			}

			let event_id_str = obj.get("event_id").unwrap().as_str().unwrap();
			let event_id = <&EventId>::try_from(event_id_str).unwrap();

			PduEvent::from_id_val(event_id, obj, None).expect("Failed to parse PduEvent")
		})
		.collect()
}

fn extract_upstream_snapshot(fixture_name: &str) -> String {
	let path = PathBuf::from(format!(
		"../../ruma-upstream/crates/ruma-state-res/tests/it/resolve/snapshots/it__resolve__{}.\
		 snap",
		fixture_name
	));

	let content = fs::read_to_string(&path)
		.unwrap_or_else(|_| panic!("Failed to read snapshot: {:?}", path));

	// Insta snapshots contain YAML headers separated by "---\n". We just want the
	// JSON.
	let parts: Vec<&str> = content.split("---\n").collect();
	parts.last().unwrap().trim().to_string()
}

fn format_state_map(state: &StateMap<OwnedEventId>) -> String {
	// Upstream snaps group by event_type, then state_key
	let mut map: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

	for ((ev_type, state_key), ev_id) in state {
		map.entry(ev_type.to_string())
			.or_default()
			.insert(state_key.to_string(), ev_id.to_string());
	}

	serde_json::to_string_pretty(&map).unwrap()
}

// ==========================================
// Mock Store for State Resolution
// ==========================================

struct MockStore {
	events: HashMap<OwnedEventId, PduEvent>,
}

impl MockStore {
	fn new(events: &[PduEvent]) -> Self {
		let mut map = HashMap::new();
		for ev in events {
			map.insert(ev.event_id().to_owned(), ev.clone());
		}
		Self { events: map }
	}

	async fn fetch_event(&self, id: OwnedEventId) -> Option<PduEvent> {
		self.events.get(&id).cloned()
	}

	// Helper to generate the auth_chain set for a given list of state events
	async fn get_auth_chain(&self, state: &StateMap<OwnedEventId>) -> HashSet<OwnedEventId> {
		let mut chain = HashSet::new();
		let mut queue: VecDeque<OwnedEventId> = state.values().cloned().collect();

		while let Some(id) = queue.pop_front() {
			if !chain.insert(id.clone()) {
				continue;
			}
			if let Some(ev) = self.fetch_event(id).await {
				queue.extend(ev.auth_events().map(|e| e.to_owned()));
			}
		}
		chain
	}
}

// ==========================================
// Test Modes (Iterative & Atomic)
// ==========================================

async fn resolve_atomic(
	events: &[PduEvent],
	store: &MockStore,
	room_version: &RoomVersionId,
) -> StateMap<OwnedEventId> {
	// Find the leaves (extremities) of the provided DAG fixture
	let mut has_children = HashSet::new();
	for ev in events {
		has_children.extend(ev.prev_events().map(|id| id.to_owned()));
	}

	let extremities: Vec<&PduEvent> = events
		.iter()
		.filter(|ev| !has_children.contains(ev.event_id()))
		.collect();

	// In a real environment, you'd calculate the state AT these extremities.
	// For atomic test bounds, we treat the entire fixture as the conflicted set.
	let mut full_state = HashMap::new();
	for ev in events {
		if let Some(sk) = ev.state_key() {
			full_state
				.insert((ev.event_type().clone(), sk.to_string()), ev.event_id().to_owned());
		}
	}

	// For MSC4297 tests, atomic resolution just dumps the states into the engine
	let auth_chains = vec![store.get_auth_chain(&full_state).await];

	state_res::resolve(
		room_version,
		vec![full_state].iter(),
		&auth_chains,
		&|id| store.fetch_event(id),
		None::<&fn(Vec<OwnedEventId>)>,
	)
	.await
	.unwrap()
}

async fn run_upstream_test(fixture_name: &str, room_version: RoomVersionId) {
	let events = parse_upstream_fixture(fixture_name);
	let store = MockStore::new(&events);

	// Run atomic resolution (mimics standard test harness mapping)
	let resolved_state = resolve_atomic(&events, &store, &room_version).await;

	let result_json = format_state_map(&resolved_state);
	let expected_snap = extract_upstream_snapshot(fixture_name);

	assert_eq!(
		result_json, expected_snap,
		"State Resolution mismatch for fixture: {}",
		fixture_name
	);
}

// ==========================================
// Test Registration (All 13 Upstream Tests)
// ==========================================

macro_rules! upstream_test {
	($name:ident, $fixture:expr, $version:expr) => {
		#[tokio::test]
		async fn $name() { run_upstream_test($fixture, $version).await; }
	};
}

upstream_test!(minimal_private_chat, "minimal_private_chat", RoomVersionId::V11);
upstream_test!(minimal_public_chat, "minimal_public_chat", RoomVersionId::V11);
upstream_test!(origin_server_ts_tiebreak, "origin_server_ts_tiebreak", RoomVersionId::V11);

// MSC4297 Specific Tests (V2.0 vs V2.1)
upstream_test!(msc4297_problem_a_state_res_v2_0, "msc4297_problem_a", RoomVersionId::V11);
upstream_test!(msc4297_problem_a_state_res_v2_1, "msc4297_problem_a", RoomVersionId::V12); // V2.1 rules
upstream_test!(msc4297_problem_b_state_res_v2_0, "msc4297_problem_b", RoomVersionId::V11);
upstream_test!(msc4297_problem_b_state_res_v2_1, "msc4297_problem_b", RoomVersionId::V12); // V2.1 rules

upstream_test!(ban_vs_power_levels, "ban_vs_power_levels", RoomVersionId::V11);
upstream_test!(topic_vs_power_levels, "topic_vs_power_levels", RoomVersionId::V11);
upstream_test!(power_levels_admin_vs_mod, "power_levels_admin_vs_mod", RoomVersionId::V11);
upstream_test!(topic_vs_ban, "topic_vs_ban", RoomVersionId::V11);
upstream_test!(join_rules_vs_join, "join_rules_vs_join", RoomVersionId::V11);
upstream_test!(concurrent_joins, "concurrent_joins", RoomVersionId::V11);
