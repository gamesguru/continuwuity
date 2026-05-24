use std::{
	collections::{BTreeSet, HashMap, HashSet},
	fs,
};

use conduwuit_core::matrix::{Event, Pdu, StateKey, StateMap, state_res};
use ruma::{
	EventId, OwnedEventId, RoomVersionId,
	events::{StateEventType, TimelineEventType},
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue as RawJsonValue;

#[derive(Deserialize)]
struct CreateContent {
	#[serde(default = "default_room_version")]
	room_version: RoomVersionId,
}

fn default_room_version() -> RoomVersionId { RoomVersionId::V1 }

fn get_fixtures_dir() -> std::path::PathBuf {
	let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_owned());
	std::path::Path::new(&manifest_dir)
		.join("../../ruma-upstream/crates/ruma-state-res/tests/it/resolve/fixtures")
}

fn get_snapshots_dir() -> std::path::PathBuf {
	let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_owned());
	std::path::Path::new(&manifest_dir)
		.join("../../ruma-upstream/crates/ruma-state-res/tests/it/resolve/snapshots")
}

fn load_pdus(pdus_paths: &[&str]) -> Vec<Vec<Pdu>> {
	let fixtures_dir = get_fixtures_dir();
	pdus_paths
		.iter()
		.map(|path| {
			let full_path = fixtures_dir.join(path);
			let file_content = fs::read_to_string(&full_path)
				.unwrap_or_else(|e| panic!("failed to read {:?}: {}", full_path, e));
			let raw_events: Vec<serde_json::Value> = serde_json::from_str(&file_content)
				.unwrap_or_else(|e| panic!("failed to parse {:?}: {}", full_path, e));

			raw_events
				.into_iter()
				.filter_map(|mut ev| {
					// State response events may lack prev_events, depth, hashes
					if ev.get("prev_events").is_none() {
						ev.as_object_mut()
							.unwrap()
							.insert("prev_events".into(), serde_json::json!([]));
					}
					if ev.get("depth").is_none() {
						ev.as_object_mut()
							.unwrap()
							.insert("depth".into(), serde_json::json!(0));
					}
					if ev.get("hashes").is_none() {
						ev.as_object_mut()
							.unwrap()
							.insert("hashes".into(), serde_json::json!({"sha256": ""}));
					}
					if ev.get("auth_events").is_none() {
						ev.as_object_mut()
							.unwrap()
							.insert("auth_events".into(), serde_json::json!([]));
					}
					match serde_json::from_value::<Pdu>(ev) {
						| Ok(pdu) => Some(pdu),
						| Err(e) => {
							eprintln!("Warning: skipping event: {e}");
							None
						},
					}
				})
				.collect()
		})
		.collect()
}

fn get_room_version(pdus: &[Vec<Pdu>]) -> RoomVersionId {
	let first_batch = pdus.first().expect("no batches");
	let first_pdu = first_batch.first().expect("no pdus in first batch");
	assert_eq!(
		first_pdu.kind,
		TimelineEventType::RoomCreate,
		"first event must be m.room.create"
	);
	let content: CreateContent = serde_json::from_str(first_pdu.content.get())
		.expect("failed to deserialize creator content");
	content.room_version
}

fn pdu_auth_chain(pdu: &Pdu, pdus_map: &HashMap<OwnedEventId, Pdu>) -> HashSet<OwnedEventId> {
	let mut auth_chain = HashSet::new();
	let mut stack = pdu.auth_events.clone();

	while let Some(event_id) = stack.pop() {
		if auth_chain.contains(&event_id) {
			continue;
		}

		let Some(pdu) = pdus_map.get(&event_id) else {
			panic!("missing required PDU in auth chain: {event_id}");
		};

		stack.extend(pdu.auth_events.clone());
		auth_chain.insert(event_id);
	}

	auth_chain
}

async fn resolve_iteratively(
	pdus: &[Pdu],
	room_version: &RoomVersionId,
) -> Result<StateMap<OwnedEventId>, conduwuit_core::state_res::Error> {
	let mut forward_prev_events_graph: HashMap<OwnedEventId, Vec<OwnedEventId>> = HashMap::new();
	let mut stack = Vec::new();

	for pdu in pdus {
		let mut has_prev_events = false;
		for prev_event in &pdu.prev_events {
			forward_prev_events_graph
				.entry(prev_event.clone())
				.or_default()
				.push(pdu.event_id.clone());
			has_prev_events = true;
		}
		if pdu.kind == TimelineEventType::RoomCreate && !has_prev_events {
			stack.push(pdu.event_id.clone());
		}
	}

	let pdus_map: HashMap<OwnedEventId, Pdu> = pdus
		.iter()
		.map(|pdu| (pdu.event_id.clone(), pdu.clone()))
		.collect();

	let auth_chain_from_state_map = |state_map: &StateMap<OwnedEventId>| {
		let mut auth_chain_set = HashSet::new();
		for event_id in state_map.values() {
			if let Some(pdu) = pdus_map.get(event_id) {
				auth_chain_set.extend(pdu_auth_chain(pdu, &pdus_map));
			}
		}
		auth_chain_set
	};

	let mut state_at_events: HashMap<OwnedEventId, StateMap<OwnedEventId>> = HashMap::new();
	let mut leaves = Vec::new();

	'outer: while let Some(event_id) = stack.pop() {
		let mut states_before_event = Vec::new();
		let mut auth_chains_before_event = Vec::new();

		let current_pdu = pdus_map
			.get(&event_id)
			.expect("every pdu should be available");

		for prev_event in &current_pdu.prev_events {
			let Some(state_at_event) = state_at_events.get(prev_event) else {
				continue 'outer;
			};
			let auth_chain_at_event = auth_chain_from_state_map(state_at_event);

			states_before_event.push(state_at_event);
			auth_chains_before_event.push(auth_chain_at_event);
		}

		let fetch = |id: OwnedEventId| {
			let pdu = pdus_map.get(&id).cloned();
			async move { pdu }
		};

		let state_before_event = if states_before_event.is_empty() {
			HashMap::new()
		} else if states_before_event.len() == 1 {
			states_before_event[0].clone()
		} else {
			state_res::resolve(
				room_version,
				states_before_event.iter().copied(),
				&auth_chains_before_event,
				&fetch,
				None::<&fn(Vec<OwnedEventId>) -> std::future::Ready<Vec<Pdu>>>,
				None::<&fn(Vec<OwnedEventId>)>,
			)
			.await?
		};

		let auth_chain_before_event = auth_chain_from_state_map(&state_before_event);

		let mut proposed_state_at_event = state_before_event.clone();
		let sk = current_pdu.state_key.clone().unwrap_or_else(StateKey::new);
		proposed_state_at_event.insert((current_pdu.kind.clone().into(), sk), event_id.clone());

		let mut auth_chain_at_event = auth_chain_before_event.clone();
		auth_chain_at_event.extend(pdu_auth_chain(current_pdu, &pdus_map));

		let state_at_event = state_res::resolve(
			room_version,
			&[state_before_event, proposed_state_at_event],
			&[auth_chain_before_event, auth_chain_at_event],
			&fetch,
			None::<&fn(Vec<OwnedEventId>) -> std::future::Ready<Vec<Pdu>>>,
			None::<&fn(Vec<OwnedEventId>)>,
		)
		.await?;

		state_at_events.insert(event_id.clone(), state_at_event);

		if let Some(prev_events) = forward_prev_events_graph.get(&event_id) {
			stack.extend(prev_events.clone());
		} else {
			leaves.push(event_id);
		}
	}

	if state_at_events.len() != pdus_map.len() {
		panic!("Not all events have a state calculated!");
	}

	let mut leaf_states = Vec::new();
	let mut auth_chain_sets = Vec::new();

	for leaf in leaves {
		let state_at_event = state_at_events
			.get(&leaf)
			.expect("states at all events are known");
		let auth_chain_at_event = auth_chain_from_state_map(state_at_event);

		leaf_states.push(state_at_event);
		auth_chain_sets.push(auth_chain_at_event);
	}

	let fetch = |id: OwnedEventId| {
		let pdu = pdus_map.get(&id).cloned();
		async move { pdu }
	};

	state_res::resolve(
		room_version,
		leaf_states.iter().copied(),
		&auth_chain_sets,
		&fetch,
		None::<&fn(Vec<OwnedEventId>) -> std::future::Ready<Vec<Pdu>>>,
		None::<&fn(Vec<OwnedEventId>)>,
	)
	.await
}

async fn resolve_batch<'a, I>(
	pdus: I,
	room_version: &RoomVersionId,
	pdus_map: &mut HashMap<OwnedEventId, Pdu>,
	prev_state: Option<StateMap<OwnedEventId>>,
) -> Result<StateMap<OwnedEventId>, conduwuit_core::state_res::Error>
where
	I: IntoIterator<Item = &'a Pdu> + Clone,
{
	let mut state_maps = prev_state.into_iter().collect::<Vec<_>>();

	for pdu in pdus.clone() {
		let mut state_map = StateMap::new();
		let sk = pdu
			.state_key
			.clone()
			.expect("all PDUs should be state events");
		state_map.insert((pdu.kind.clone().into(), sk), pdu.event_id.clone());
		state_maps.push(state_map);
	}

	pdus_map.extend(
		pdus.clone()
			.into_iter()
			.map(|pdu| (pdu.event_id.clone(), pdu.clone())),
	);

	let mut auth_chain_sets = Vec::new();
	for pdu in pdus {
		auth_chain_sets.push(pdu_auth_chain(pdu, pdus_map));
	}

	let fetch = |id: OwnedEventId| {
		let pdu = pdus_map.get(&id).cloned();
		async move { pdu }
	};

	state_res::resolve(
		room_version,
		&state_maps,
		&auth_chain_sets,
		&fetch,
		None::<&fn(Vec<OwnedEventId>) -> std::future::Ready<Vec<Pdu>>>,
		None::<&fn(Vec<OwnedEventId>)>,
	)
	.await
}

async fn test_resolve_batches(pdus_paths: &[&str]) -> String {
	let pdu_batches = load_pdus(pdus_paths);
	let room_version = get_room_version(&pdu_batches);

	let all_pdus: Vec<Pdu> = pdu_batches.iter().flatten().cloned().collect();

	let iteratively_resolved_state = resolve_iteratively(&all_pdus, &room_version)
		.await
		.expect("iterative resolution should succeed");

	let mut pdus_map = HashMap::new();
	let mut batched_resolved_state = None;
	for pdus in &pdu_batches {
		batched_resolved_state = Some(
			resolve_batch(pdus, &room_version, &mut pdus_map, batched_resolved_state)
				.await
				.expect("batched resolution step should succeed"),
		);
	}
	let batched_resolved_state =
		batched_resolved_state.expect("batched resolution should have run");

	let mut atomic_pdus_map = HashMap::new();
	let atomic_resolved_state =
		resolve_batch(&all_pdus, &room_version, &mut atomic_pdus_map, None)
			.await
			.expect("atomic resolution should succeed");

	let iter_json = state_map_to_json_string(iteratively_resolved_state, &pdus_map);
	let batch_json = state_map_to_json_string(batched_resolved_state, &pdus_map);
	let atomic_json = state_map_to_json_string(atomic_resolved_state, &atomic_pdus_map);

	assert_eq!(iter_json, batch_json, "iterative resolved state does not match batched");
	assert_eq!(batch_json, atomic_json, "batched resolved state does not match atomic");

	iter_json
}

fn load_state_maps(
	state_maps_paths: &[&str],
	pdus_map: &HashMap<OwnedEventId, Pdu>,
) -> Vec<StateMap<OwnedEventId>> {
	let fixtures_dir = get_fixtures_dir();
	state_maps_paths
		.iter()
		.map(|path| {
			let full_path = fixtures_dir.join(path);
			let file_content = fs::read_to_string(&full_path)
				.unwrap_or_else(|e| panic!("failed to read state map at {:?}: {}", full_path, e));
			let event_ids: Vec<OwnedEventId> = serde_json::from_str(&file_content)
				.unwrap_or_else(|e| {
					panic!("failed to deserialize event IDs from {:?}: {}", full_path, e)
				});

			event_ids
				.into_iter()
				.map(|event_id| {
					let pdu = pdus_map
						.get(&event_id)
						.unwrap_or_else(|| panic!("Event ID {} not found in PDUs map", event_id));
					let sk = pdu
						.state_key
						.clone()
						.expect("All PDUs must be state events");
					((pdu.kind.clone().into(), sk), event_id)
				})
				.collect()
		})
		.collect()
}

async fn test_resolve_state_maps(state_maps_paths: &[&str], pdus_paths: &[&str]) -> String {
	let pdu_batches = load_pdus(pdus_paths);
	let room_version = get_room_version(&pdu_batches);

	let pdus: Vec<Pdu> = pdu_batches.into_iter().flatten().collect();
	let pdus_map: HashMap<OwnedEventId, Pdu> = pdus
		.iter()
		.map(|pdu| (pdu.event_id.clone(), pdu.clone()))
		.collect();

	let state_maps = load_state_maps(state_maps_paths, &pdus_map);

	let mut auth_chain_sets = Vec::new();
	for state_map in &state_maps {
		let mut auth_chain = HashSet::new();
		for event_id in state_map.values() {
			if let Some(pdu) = pdus_map.get(event_id) {
				auth_chain.extend(pdu_auth_chain(pdu, &pdus_map));
			}
		}
		auth_chain_sets.push(auth_chain);
	}

	let fetch = |id: OwnedEventId| {
		let pdu = pdus_map.get(&id).cloned();
		async move { pdu }
	};

	let resolved_state = state_res::resolve(
		&room_version,
		&state_maps,
		&auth_chain_sets,
		&fetch,
		None::<&fn(Vec<OwnedEventId>) -> std::future::Ready<Vec<Pdu>>>,
		None::<&fn(Vec<OwnedEventId>)>,
	)
	.await
	.expect("resolve_state_maps resolution should succeed");

	state_map_to_json_string(resolved_state, &pdus_map)
}

#[derive(Serialize)]
struct ResolvedStateEvent<'a> {
	#[serde(rename = "type")]
	event_type: &'a StateEventType,
	state_key: &'a str,
	event_id: &'a EventId,
	content: &'a RawJsonValue,
}

impl PartialEq for ResolvedStateEvent<'_> {
	fn eq(&self, other: &Self) -> bool {
		self.event_type == other.event_type && self.state_key == other.state_key
	}
}

impl Eq for ResolvedStateEvent<'_> {}

impl std::cmp::Ord for ResolvedStateEvent<'_> {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.event_type
			.cmp(other.event_type)
			.then_with(|| self.state_key.cmp(other.state_key))
	}
}

impl std::cmp::PartialOrd for ResolvedStateEvent<'_> {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

fn state_map_to_json_string(
	state_map: StateMap<OwnedEventId>,
	pdus_map: &HashMap<OwnedEventId, Pdu>,
) -> String {
	let resolved_state = state_map
		.iter()
		.map(|((event_type, state_key), event_id)| {
			let pdu = pdus_map
				.get(event_id)
				.unwrap_or_else(|| panic!("event id {} not in pdus map", event_id));
			ResolvedStateEvent {
				event_type,
				state_key: state_key.as_str(),
				content: &pdu.content,
				event_id,
			}
		})
		.collect::<BTreeSet<_>>();

	serde_json::to_string_pretty(&resolved_state)
		.expect("resolved state serialization should succeed")
}

fn read_snapshot(snapshot_filename: &str) -> String {
	let snapshots_dir = get_snapshots_dir();
	let full_path = snapshots_dir.join(snapshot_filename);
	let file_content = fs::read_to_string(&full_path)
		.unwrap_or_else(|e| panic!("failed to read snapshot at {:?}: {}", full_path, e));

	let json_start = file_content.find('[').unwrap_or_else(|| {
		panic!("failed to find start of JSON array '[' in snapshot {:?}", full_path)
	});

	file_content[json_start..].trim_end().to_owned()
}

fn assert_snapshot_eq(actual: &str, snapshot_filename: &str) {
	let expected = read_snapshot(snapshot_filename);
	let actual_val: serde_json::Value =
		serde_json::from_str(actual).expect("failed to parse actual resolved state JSON");
	let expected_val: serde_json::Value =
		serde_json::from_str(&expected).expect("failed to parse expected snapshot JSON");

	if actual_val != expected_val {
		let actual_pretty = serde_json::to_string_pretty(&actual_val).unwrap();
		let expected_pretty = serde_json::to_string_pretty(&expected_val).unwrap();
		panic!(
			"Snapshot mismatch for {}!\n\nExpected:\n{}\n\nActual:\n{}",
			snapshot_filename, expected_pretty, actual_pretty
		);
	}
}

#[tokio::test]
async fn test_minimal_private_chat() {
	let actual = test_resolve_batches(&["bootstrap-private-chat.json"]).await;
	assert_snapshot_eq(&actual, "minimal_private_chat@resolved_state.snap");
}

#[tokio::test]
async fn test_minimal_public_chat() {
	let actual = test_resolve_batches(&["bootstrap-public-chat.json"]).await;
	assert_snapshot_eq(&actual, "minimal_public_chat@resolved_state.snap");
}

#[tokio::test]
async fn test_origin_server_ts_tiebreak() {
	let actual =
		test_resolve_batches(&["bootstrap-private-chat.json", "origin-server-ts-tiebreak.json"])
			.await;
	assert_snapshot_eq(&actual, "origin_server_ts_tiebreak@resolved_state.snap");
}

#[tokio::test]
async fn test_msc4297_problem_a_state_res_v2_0() {
	let actual = test_resolve_state_maps(
		&["MSC4297-problem-A/state-bob.json", "MSC4297-problem-A/state-charlie.json"],
		&["MSC4297-problem-A/pdus-v11.json"],
	)
	.await;
	assert_snapshot_eq(&actual, "msc4297_problem_a_state_res_v2_0@resolved_state.snap");
}

#[tokio::test]
async fn test_msc4297_problem_a_state_res_v2_1() {
	let actual = test_resolve_state_maps(
		&["MSC4297-problem-A/state-bob.json", "MSC4297-problem-A/state-charlie.json"],
		&["MSC4297-problem-A/pdus-v12.json"],
	)
	.await;
	assert_snapshot_eq(&actual, "msc4297_problem_a_state_res_v2_1@resolved_state.snap");
}

#[tokio::test]
async fn test_msc4297_problem_b_state_res_v2_0() {
	let actual = test_resolve_state_maps(
		&["MSC4297-problem-B/state-eve.json", "MSC4297-problem-B/state-zara.json"],
		&["MSC4297-problem-B/pdus-v11.json"],
	)
	.await;
	assert_snapshot_eq(&actual, "msc4297_problem_b_state_res_v2_0@resolved_state.snap");
}

#[tokio::test]
async fn test_msc4297_problem_b_state_res_v2_1() {
	let actual = test_resolve_state_maps(
		&["MSC4297-problem-B/state-eve.json", "MSC4297-problem-B/state-zara.json"],
		&["MSC4297-problem-B/pdus-v12.json"],
	)
	.await;
	assert_snapshot_eq(&actual, "msc4297_problem_b_state_res_v2_1@resolved_state.snap");
}

#[tokio::test]
async fn test_ban_vs_power_levels() {
	let actual = test_resolve_batches(&[
		"bootstrap-public-chat.json",
		"ban-vs-power-levels-alice.json",
		"ban-vs-power-levels-bob.json",
	])
	.await;
	assert_snapshot_eq(&actual, "ban_vs_power_levels@resolved_state.snap");
}

#[tokio::test]
async fn test_topic_vs_power_levels() {
	let actual = test_resolve_batches(&[
		"bootstrap-public-chat.json",
		"topic-vs-power-levels-alice.json",
		"topic-vs-power-levels-bob.json",
	])
	.await;
	assert_snapshot_eq(&actual, "topic_vs_power_levels@resolved_state.snap");
}

#[tokio::test]
async fn test_power_levels_admin_vs_mod() {
	let actual = test_resolve_batches(&[
		"bootstrap-public-chat.json",
		"power-levels-admin-vs-mod-alice.json",
		"power-levels-admin-vs-mod-bob.json",
	])
	.await;
	assert_snapshot_eq(&actual, "power_levels_admin_vs_mod@resolved_state.snap");
}

#[tokio::test]
async fn test_topic_vs_ban() {
	let actual = test_resolve_batches(&[
		"bootstrap-public-chat.json",
		"topic-vs-ban-common.json",
		"topic-vs-ban-alice.json",
		"topic-vs-ban-bob.json",
	])
	.await;
	assert_snapshot_eq(&actual, "topic_vs_ban@resolved_state.snap");
}

#[tokio::test]
async fn test_join_rules_vs_join() {
	let actual = test_resolve_batches(&[
		"bootstrap-public-chat.json",
		"join-rules-vs-join-common.json",
		"join-rules-vs-join-alice.json",
		"join-rules-vs-join-ella.json",
	])
	.await;
	assert_snapshot_eq(&actual, "join_rules_vs_join@resolved_state.snap");
}

#[tokio::test]
async fn test_concurrent_joins() {
	let actual = test_resolve_batches(&[
		"bootstrap-public-chat.json",
		"concurrent-joins-charlie.json",
		"concurrent-joins-ella.json",
	])
	.await;
	assert_snapshot_eq(&actual, "concurrent_joins@resolved_state.snap");
}
