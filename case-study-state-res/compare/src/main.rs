use std::{
	collections::{HashMap, HashSet},
	fs::File,
	io::BufReader,
	sync::Arc,
};

use conduwuit_core::matrix::{Pdu, state_res};
use ruma::{OwnedEventId, RoomVersionId};

struct DagConfig {
	path: String,
	room_version: RoomVersionId,
	baseline_path: Option<String>,
}

fn parse_args() -> DagConfig {
	let args: Vec<String> = std::env::args().collect();
	let path = args.get(1).cloned().unwrap_or_else(|| {
		"/run/media/shane/shane4tb-ent/dags/\
		 merged-sM2LwqNHGQOgLf35gqxPMy9D7oYde2q9ADg8HPBM3kE-unredacted-lounge-v12-d1-84135.jsonl"
			.to_string()
	});
	let version_str = args.get(2).map(|s| s.as_str()).unwrap_or("12");
	let room_version = match version_str {
		| "1" => RoomVersionId::V1,
		| "2" => RoomVersionId::V2,
		| "3" => RoomVersionId::V3,
		| "4" => RoomVersionId::V4,
		| "5" => RoomVersionId::V5,
		| "6" => RoomVersionId::V6,
		| "7" => RoomVersionId::V7,
		| "8" => RoomVersionId::V8,
		| "9" => RoomVersionId::V9,
		| "10" => RoomVersionId::V10,
		| "11" => RoomVersionId::V11,
		| "12" => RoomVersionId::V12,
		| v => panic!("Unknown room version: {v}"),
	};
	let baseline_path = args.get(3).cloned();
	DagConfig { path, room_version, baseline_path }
}

fn load_events(path: &str) -> HashMap<OwnedEventId, Arc<Pdu>> {
	use std::io::BufRead;
	let mut events_map = HashMap::new();
	let file = File::open(path).unwrap_or_else(|e| panic!("Failed to open {path}: {e}"));
	let reader = BufReader::new(file);
	for line in reader.lines().map_while(Result::ok) {
		if line.trim().is_empty() {
			continue;
		}
		if let Ok(pdu) = serde_json::from_str::<Pdu>(&line) {
			events_map.insert(pdu.event_id.clone(), Arc::new(pdu));
		}
	}
	events_map
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt::init();

	let config = parse_args();
	let start_time = std::time::Instant::now();

	println!("Loading {}...", config.path);
	let events_map = load_events(&config.path);
	println!("Loaded {} total events.", events_map.len());

	// Compute heads (forward extremities)
	let mut referenced = HashSet::new();
	for pdu in events_map.values() {
		for pe in &pdu.prev_events {
			referenced.insert(pe.clone());
		}
	}
	let all_ids: HashSet<OwnedEventId> = events_map.keys().cloned().collect();
	let mut heads: Vec<OwnedEventId> = all_ids.difference(&referenced).cloned().collect();
	heads.sort();

	println!("Found {} heads", heads.len());

	// Build state sets per head via backward walk
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
		state_sets.push(state_map);
	}

	// Build auth chain sets
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

	println!("State sets: {} heads", state_sets.len());
	for (i, s) in state_sets.iter().enumerate() {
		if state_sets.len() <= 10 || i < 3 || i >= state_sets.len() - 2 {
			println!("  state_set[{}]: {} entries", i, s.len());
		} else if i == 3 {
			println!("  ...");
		}
	}

	let fetch_event = |event_id: OwnedEventId| {
		std::future::ready(events_map.get(&event_id).map(|p| (**p).clone()))
	};
	let event_rejected = |_: OwnedEventId| std::future::ready(false);

	println!("Resolving state with room version {:?}...", config.room_version);
	let conduwuit_resolved = state_res::resolve(
		&config.room_version,
		&state_sets,
		&auth_chain_sets,
		&fetch_event,
		&event_rejected,
		None::<&fn(Vec<OwnedEventId>)>,
	)
	.await
	.expect("Conduwuit failed to resolve");

	let duration = start_time.elapsed();
	println!(
		"Conduwuit resolved {} events in {:.2}s",
		conduwuit_resolved.len(),
		duration.as_secs_f64()
	);

	// Membership counts
	let (mut joined, mut left, mut banned, mut invite, mut knock) = (0, 0, 0, 0, 0);
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
		"CONDUWUIT COUNTS: {} joined, {} left, {} banned, {} invite, {} knock",
		joined, left, banned, invite, knock
	);

	// Compare against baseline if provided
	if let Some(baseline_path) = &config.baseline_path {
		let output_file = File::open(baseline_path)?;
		let output_reader = BufReader::new(output_file);
		let baseline_ids: Vec<OwnedEventId> = serde_json::from_reader(output_reader)?;

		let baseline_set: HashSet<OwnedEventId> = baseline_ids.into_iter().collect();
		let conduwuit_set: HashSet<OwnedEventId> = conduwuit_resolved.values().cloned().collect();

		let missing: Vec<_> = baseline_set.difference(&conduwuit_set).collect();
		let extra: Vec<_> = conduwuit_set.difference(&baseline_set).collect();

		if missing.is_empty() && extra.is_empty() {
			println!("SUCCESS: Conduwuit perfectly matches baseline!");
		} else {
			println!(
				"DIVERGENCE: {} missing, {} extra (baseline={}, conduwuit={})",
				missing.len(),
				extra.len(),
				baseline_set.len(),
				conduwuit_set.len()
			);
			for id in missing.iter().take(10) {
				if let Some(ev) = events_map.get(*id) {
					println!(
						"  MISSING: {} type={}, state_key={}, sender={}",
						id,
						ev.kind,
						ev.state_key.as_deref().unwrap_or(""),
						ev.sender
					);
				}
			}
			for id in extra.iter().take(10) {
				if let Some(ev) = events_map.get(*id) {
					println!(
						"  EXTRA:   {} type={}, state_key={}, sender={}",
						id,
						ev.kind,
						ev.state_key.as_deref().unwrap_or(""),
						ev.sender
					);
				}
			}
		}
	} else {
		println!("No baseline provided — skipping comparison.");
	}

	// Export results
	let conduwuit_set: HashSet<OwnedEventId> = conduwuit_resolved.values().cloned().collect();
	let mut conduwuit_pdus: Vec<Arc<Pdu>> = conduwuit_set
		.iter()
		.filter_map(|id| events_map.get(id).cloned())
		.collect();
	conduwuit_pdus.sort_by(|a, b| {
		let a_key = (a.kind.to_string(), a.state_key.as_deref().unwrap_or(""));
		let b_key = (b.kind.to_string(), b.state_key.as_deref().unwrap_or(""));
		a_key.cmp(&b_key)
	});
	let sorted_ids: Vec<String> = conduwuit_pdus
		.into_iter()
		.map(|p| p.event_id.to_string())
		.collect();

	let output_json = serde_json::json!({
		"resolved_state_size": sorted_ids.len(),
		"state_event_ids": sorted_ids,
		"duration_ms": duration.as_millis(),
		"room_version": format!("{:?}", config.room_version),
	});

	let mut out_file = File::create("/tmp/conduwuit-output.json")?;
	serde_json::to_writer_pretty(&mut out_file, &output_json)?;
	println!("Exported to /tmp/conduwuit-output.json");

	Ok(())
}
