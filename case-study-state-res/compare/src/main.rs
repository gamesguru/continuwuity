use std::{
	collections::{HashMap, HashSet},
	fs::File,
	io::BufReader,
	sync::Arc,
};

use conduwuit_core::matrix::{Pdu, state_res};
use ruma::{OwnedEventId, RoomVersionId};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	println!("Loading events...");
	let file = File::open("../../state-res-events.json")?;
	let reader = BufReader::new(file);
	let pdus: Vec<Pdu> = serde_json::from_reader(reader)?;

	let mut events_map: HashMap<OwnedEventId, Arc<Pdu>> = HashMap::new();
	for pdu in pdus {
		events_map.insert(pdu.event_id.clone(), Arc::new(pdu));
	}

	println!("Loaded {} events.", events_map.len());

	let mut referenced = HashSet::new();
	for pdu in events_map.values() {
		for pe in &pdu.prev_events {
			referenced.insert(pe.clone());
		}
	}
	let all_ids: HashSet<OwnedEventId> = events_map.keys().cloned().collect();
	let mut heads: Vec<OwnedEventId> = all_ids.difference(&referenced).cloned().collect();
	heads.sort();

	println!("Found {} heads: {:?}", heads.len(), heads);

	let mut state_sets = Vec::new();
	for head_id in &heads {
		let mut reachable = Vec::new();
		let mut visited = HashSet::new();
		let mut stack = vec![head_id.clone()];
		while let Some(ev_id) = stack.pop() {
			if visited.insert(ev_id.clone()) {
				if let Some(ev) = events_map.get(&ev_id) {
					reachable.push(ev.clone());
					for pe in &ev.prev_events {
						stack.push(pe.clone());
					}
				}
			}
		}

		reachable.sort_by(|a, b| {
			a.depth
				.cmp(&b.depth)
				.then(a.event_id.cmp(&b.event_id))
		});
		let mut state_map = HashMap::new();
		for ev in reachable {
			let key = (ev.kind.clone().into(), ev.state_key.as_deref().unwrap_or("").into());
			state_map.insert(key, ev.event_id.clone());
		}
		println!("State set for head {} has {} events.", head_id, state_map.len());
		state_sets.push(state_map);
	}

	let mut auth_chain_sets = Vec::new();
	for map in &state_sets {
		let mut auth_chain = HashSet::new();
		let mut stack: Vec<OwnedEventId> = map.values().cloned().collect();
		let mut visited = HashSet::new();
		while let Some(ev_id) = stack.pop() {
			if visited.insert(ev_id.clone()) {
				if let Some(ev) = events_map.get(&ev_id) {
					auth_chain.insert(ev_id.clone());
					for auth_id in &ev.auth_events {
						stack.push(auth_id.clone());
					}
				}
			}
		}
		auth_chain_sets.push(auth_chain);
	}

	let room_version = RoomVersionId::V12;

	let fetch_event = |event_id: OwnedEventId| {
		std::future::ready(events_map.get(&event_id).map(|p| (**p).clone()))
	};
	let event_rejected = |_: OwnedEventId| std::future::ready(false);

	println!("Resolving state using conduwuit_state_res...");
	let conduwuit_resolved = state_res::resolve(
		&room_version,
		&state_sets,
		&auth_chain_sets,
		&fetch_event,
		&event_rejected,
		None::<&fn(Vec<OwnedEventId>)>,
	)
	.await
	.expect("Conduwuit failed to resolve");

	println!("Conduwuit resolved {} events", conduwuit_resolved.len());

	// Load baseline output
	let output_file = File::open("../../state-res-output.json")?;
	let output_reader = BufReader::new(output_file);
	let output_json: serde_json::Value = serde_json::from_reader(output_reader)?;
	let baseline_ids: Vec<OwnedEventId> =
		serde_json::from_value(output_json["state_event_ids"].clone())?;
	let baseline_set: HashSet<OwnedEventId> = baseline_ids.into_iter().collect();

	let conduwuit_set: HashSet<OwnedEventId> = conduwuit_resolved.values().cloned().collect();

	let missing_in_conduwuit: Vec<_> = baseline_set.difference(&conduwuit_set).collect();
	let extra_in_conduwuit: Vec<_> = conduwuit_set.difference(&baseline_set).collect();

	if missing_in_conduwuit.is_empty() && extra_in_conduwuit.is_empty() {
		println!("SUCCESS: Conduwuit state resolution perfectly matches baseline!");
	} else {
		println!("DIVERGENCE FOUND!");
		println!("Missing in Conduwuit (Baseline has these): {:#?}", missing_in_conduwuit);
		println!("Extra in Conduwuit (Baseline DOES NOT have these): {:#?}", extra_in_conduwuit);
	}

	Ok(())
}
