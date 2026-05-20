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
	tracing_subscriber::fmt::init();
	println!("Loading events...");

	let start_time = std::time::Instant::now();

	let mut events_map: HashMap<OwnedEventId, Arc<Pdu>> = HashMap::new();

	let dags_dir =
		"/run/media/shane/shane4tb-ent/dags/\
		 merged-sM2LwqNHGQOgLf35gqxPMy9D7oYde2q9ADg8HPBM3kE-unredacted-lounge-v12-d1-84135.jsonl";
	println!("Loading merged.jsonl...");
	let file = File::open(dags_dir)?;
	let reader = BufReader::new(file);
	use std::io::BufRead;
	for line in reader.lines().map_while(Result::ok) {
		if line.trim().is_empty() {
			continue;
		}
		if let Ok(pdu) = serde_json::from_str::<Pdu>(&line) {
			events_map.insert(pdu.event_id.clone(), Arc::new(pdu));
		}
	}

	println!("Loaded {} total events.", events_map.len());

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
			if visited.insert(ev_id.clone())
				&& let Some(ev) = events_map.get(&ev_id)
			{
				reachable.push(ev.clone());
				for pe in &ev.prev_events {
					stack.push(pe.clone());
				}
			}
		}

		reachable.sort_by(|a, b| a.depth.cmp(&b.depth).then(a.event_id.cmp(&b.event_id)));

		let mut state_map = HashMap::new();
		for ev in reachable {
			let key = (ev.kind.clone().into(), ev.state_key.as_deref().unwrap_or("").into());
			state_map.insert(key, ev.event_id.clone());
		}
		println!("State set for head {} has {} events.", head_id, state_map.len());
		state_sets.push(state_map);
	}
	println!("Using all {} heads", heads.len());

	let mut auth_chain_sets = Vec::new();
	for map in &state_sets {
		let mut auth_chain = HashSet::new();
		let mut stack: Vec<OwnedEventId> = map.values().cloned().collect();
		let mut visited = HashSet::new();
		while let Some(ev_id) = stack.pop() {
			if visited.insert(ev_id.clone())
				&& let Some(ev) = events_map.get(&ev_id)
			{
				auth_chain.insert(ev_id.clone());
				for auth_id in &ev.auth_events {
					stack.push(auth_id.clone());
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

	println!("State sets: {}", state_sets.len());
	for (i, s) in state_sets.iter().enumerate() {
		println!("  state_set[{}]: {} entries", i, s.len());
	}
	println!("Auth chain sets: {}", auth_chain_sets.len());
	for (i, a) in auth_chain_sets.iter().enumerate() {
		println!("  auth_chain[{}]: {} entries", i, a.len());
	}

	// Check how many state keys are conflicted (appear in >1 set with different
	// values)
	let mut all_keys: HashMap<
		(ruma::events::StateEventType, conduwuit_core::matrix::StateKey),
		Vec<OwnedEventId>,
	> = HashMap::new();
	for s in &state_sets {
		for (k, v) in s {
			all_keys.entry(k.clone()).or_default().push(v.clone());
		}
	}
	let conflicted_keys: Vec<_> = all_keys
		.iter()
		.filter(|(_, vs)| {
			let first = &vs[0];
			vs.iter().any(|v| v != first)
		})
		.collect();
	println!("Conflicted state keys: {} / {} total", conflicted_keys.len(), all_keys.len());

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

	let mut joined = 0;
	let mut left = 0;
	let mut banned = 0;
	let mut invite = 0;
	let mut knock = 0;

	for id in conduwuit_resolved.values() {
		if let Some(ev) = events_map.get(id)
			&& ev.kind == ruma::events::TimelineEventType::RoomMember
			&& let Ok(member) = serde_json::from_str::<
				ruma::events::room::member::RoomMemberEventContent,
			>(ev.content.get())
		{
			match member.membership {
				| ruma::events::room::member::MembershipState::Join => joined += 1,
				| ruma::events::room::member::MembershipState::Leave => left += 1,
				| ruma::events::room::member::MembershipState::Ban => banned += 1,
				| ruma::events::room::member::MembershipState::Invite => invite += 1,
				| ruma::events::room::member::MembershipState::Knock => knock += 1,
				| _ => {},
			}
		}
	}
	println!(
		"CONDUWUIT COUNTS: {} joined, {} left, {} banned (invite: {}, knock: {})",
		joined, left, banned, invite, knock
	);

	// Load baseline output
	let output_file = File::open("/tmp/state-res-output.json")?;
	let output_reader = BufReader::new(output_file);
	let baseline_ids: Vec<OwnedEventId> = serde_json::from_reader(output_reader)?;

	let mut b_joined = 0;
	let mut b_left = 0;
	let mut b_banned = 0;

	for ev_id in &baseline_ids {
		if let Some(ev) = events_map.get(ev_id)
			&& ev.kind == ruma::events::TimelineEventType::RoomMember
			&& let Ok(member) = serde_json::from_str::<
				ruma::events::room::member::RoomMemberEventContent,
			>(ev.content.get())
		{
			match member.membership {
				| ruma::events::room::member::MembershipState::Join => b_joined += 1,
				| ruma::events::room::member::MembershipState::Leave => b_left += 1,
				| ruma::events::room::member::MembershipState::Ban => b_banned += 1,
				| _ => {},
			}
		}
	}
	println!("BASELINE COUNTS: {} joined, {} left, {} banned", b_joined, b_left, b_banned);

	let baseline_set: HashSet<OwnedEventId> = baseline_ids.into_iter().collect();

	let conduwuit_set: HashSet<OwnedEventId> = conduwuit_resolved.values().cloned().collect();

	let missing_in_conduwuit: Vec<_> = baseline_set.difference(&conduwuit_set).collect();
	let extra_in_conduwuit: Vec<_> = conduwuit_set.difference(&baseline_set).collect();

	let mut conduwuit_pdus: Vec<Arc<Pdu>> = conduwuit_set
		.iter()
		.filter_map(|id| events_map.get(id).cloned())
		.collect();

	// Sort by (TimelineEventType String, StateKey String) to match ruma-lean's
	// BTreeMap StateMap sorting
	conduwuit_pdus.sort_by(|a, b| {
		let a_key = (a.kind.to_string(), a.state_key.as_deref().unwrap_or(""));
		let b_key = (b.kind.to_string(), b.state_key.as_deref().unwrap_or(""));
		a_key.cmp(&b_key)
	});

	let sorted_ids: Vec<String> = conduwuit_pdus
		.into_iter()
		.map(|p| p.event_id.to_string())
		.collect();

	let mut final_auth_chain = HashSet::new();
	let mut stack: Vec<OwnedEventId> = conduwuit_set.iter().cloned().collect();
	let mut visited = HashSet::new();
	while let Some(ev_id) = stack.pop() {
		if visited.insert(ev_id.clone())
			&& let Some(ev) = events_map.get(&ev_id)
		{
			for auth_id in &ev.auth_events {
				if final_auth_chain.insert(auth_id.clone()) {
					stack.push(auth_id.clone());
				}
			}
		}
	}

	let duration = start_time.elapsed();

	let output_json = serde_json::json!({
		"auth_chain_size": final_auth_chain.len(),
		"duration_ms": duration.as_millis(),
		"resolved_state_size": sorted_ids.len(),
		"state_event_ids": sorted_ids,
		"status": "success",
		"version": "V2_1"
	});

	let mut out_file = File::create("/tmp/conduwuit-output.json")?;
	serde_json::to_writer_pretty(&mut out_file, &output_json)?;
	println!("Exported Conduwuit's sorted state event IDs to /tmp/conduwuit-output.json");

	if missing_in_conduwuit.is_empty() && extra_in_conduwuit.is_empty() {
		println!("SUCCESS: Conduwuit state resolution perfectly matches baseline!");
	} else {
		println!("DIVERGENCE FOUND!");
		println!("Missing in Conduwuit (Baseline has these):");
		for id in &missing_in_conduwuit {
			if let Some(ev) = events_map.get(*id) {
				println!(
					"  - {}: type={}, state_key={}, sender={}",
					id,
					ev.kind,
					ev.state_key.as_deref().unwrap_or(""),
					ev.sender
				);
			} else {
				println!("  - {}: (NOT IN EVENTS MAP!)", id);
			}
		}
		println!("Extra in Conduwuit (Baseline DOES NOT have these): {:#?}", extra_in_conduwuit);
	}

	Ok(())
}
