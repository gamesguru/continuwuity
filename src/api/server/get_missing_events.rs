use std::collections::{HashSet, VecDeque};

use axum::extract::State;
use conduwuit::{Err, Event, Result, debug, info, trace, utils::to_canonical_object, warn};
use ruma::{OwnedEventId, api::federation::event::get_missing_events};
use serde_json::{json, value::RawValue};

use super::AccessCheck;
use crate::Ruma;

/// arbitrary number but synapse's is 20 and we can handle lots of these anyways
const LIMIT_MAX: usize = 50;
/// spec says default is 10
const LIMIT_DEFAULT: usize = 10;

/// # `POST /_matrix/federation/v1/get_missing_events/{roomId}`
///
/// Retrieves events that the sender is missing.
pub(crate) async fn get_missing_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_missing_events::v1::Request>,
) -> Result<get_missing_events::v1::Response> {
	AccessCheck {
		services: &services,
		origin: body.origin(),
		room_id: &body.room_id,
		event_id: None,
	}
	.check()
	.await?;

	if !services
		.rooms
		.state_cache
		.server_is_participant(services.globals.server_name(), &body.room_id)
		.await
	{
		info!(
			origin = body.origin().as_str(),
			"Refusing to serve state for room we aren't participating in"
		);
		return Err!(Request(NotFound("This server is not participating in that room.")));
	}

	let limit = body
		.limit
		.try_into()
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	info!(
		origin = body.origin().as_str(),
		room_id = %body.room_id,
		limit,
		latest = body.latest_events.len(),
		earliest = body.earliest_events.len(),
		"Serving get_missing_events request"
	);

	let room_version = services.rooms.state.get_room_version(&body.room_id).await?;

	let mut queue: VecDeque<OwnedEventId> = VecDeque::from(body.latest_events.clone());
	let mut results: Vec<(OwnedEventId, Vec<OwnedEventId>, ruma::UInt, Box<RawValue>)> =
		Vec::with_capacity(limit);
	let mut seen: HashSet<OwnedEventId> = HashSet::from_iter(body.earliest_events.clone());

	while let Some(next_event_id) = queue.pop_front() {
		if seen.contains(&next_event_id) {
			trace!(%next_event_id, "already seen event, skipping");
			continue;
		}

		if results.len() >= limit {
			debug!(%next_event_id, "reached limit of events to return, breaking");
			break;
		}

		let mut pdu = match services.rooms.timeline.get_pdu(&next_event_id).await {
			| Ok(pdu) => pdu,
			| Err(e) => {
				warn!("could not find event {next_event_id} while walking missing events: {e}");
				continue;
			},
		};
		if pdu.room_id_or_hash().as_deref() != Some(body.room_id.as_ref()) {
			return Err!(Request(Unknown(
				"Event {next_event_id} is not in room {}",
				body.room_id
			)));
		}

		if !services
			.rooms
			.state_accessor
			.server_can_see_event(
				body.origin().to_owned(),
				body.room_id.clone(),
				pdu.event_id().to_owned(),
			)
			.await
		{
			debug!(%next_event_id, origin = %body.origin(), "redacting event origin cannot see");
			pdu.redact(&room_version, json!({}))?;
		}

		trace!(
			%next_event_id,
			prev_events = ?pdu.prev_events().collect::<Vec<_>>(),
			"adding event to results and queueing prev events"
		);
		queue.extend(pdu.prev_events.clone());
		seen.insert(next_event_id.clone());
		if body.latest_events.contains(&next_event_id) {
			continue; // Don't include latest_events in results,
			// but do include their prev_events in the queue
		}
		results.push((
			next_event_id.clone(),
			pdu.prev_events.clone(),
			pdu.depth,
			services
				.sending
				.convert_to_outgoing_federation_event(to_canonical_object(pdu)?)
				.await,
		));
		trace!(
			%next_event_id,
			queue_len = queue.len(),
			seen_len = seen.len(),
			results_len = results.len(),
			"event added to results"
		);
	}

	if !queue.is_empty() {
		debug!("limit reached before queue was empty");
	}
	use std::collections::BTreeMap;
	let mut in_degree: BTreeMap<OwnedEventId, usize> = BTreeMap::new();
	let mut graph: BTreeMap<OwnedEventId, Vec<OwnedEventId>> = BTreeMap::new();
	let mut event_map: BTreeMap<OwnedEventId, Box<RawValue>> = BTreeMap::new();
	let mut depth_map: BTreeMap<OwnedEventId, ruma::UInt> = BTreeMap::new();

	for (id, prevs, depth, raw) in results {
		in_degree.insert(id.clone(), 0);
		event_map.insert(id.clone(), raw);
		depth_map.insert(id.clone(), depth);
		for prev in prevs {
			graph.entry(prev).or_default().push(id.clone());
		}
	}

	for edges in graph.values() {
		for to in edges {
			if let Some(deg) = in_degree.get_mut(to) {
				*deg = deg.saturating_add(1);
			}
		}
	}

	let mut zero_in_degree: Vec<OwnedEventId> = in_degree
		.iter()
		.filter(|(_, deg)| **deg == 0)
		.map(|(id, _)| id.clone())
		.collect();

	zero_in_degree.sort_by(|a, b| {
		let depth_a = depth_map
			.get(a)
			.copied()
			.unwrap_or_else(|| ruma::UInt::new(0).unwrap());
		let depth_b = depth_map
			.get(b)
			.copied()
			.unwrap_or_else(|| ruma::UInt::new(0).unwrap());
		depth_b.cmp(&depth_a).then_with(|| b.cmp(a))
	});

	let mut events = Vec::with_capacity(event_map.len());
	while let Some(node) = zero_in_degree.pop() {
		if let Some(raw) = event_map.remove(&node) {
			events.push(raw);
		}
		if let Some(edges) = graph.get(&node) {
			for to in edges {
				if let Some(deg) = in_degree.get_mut(to) {
					*deg = deg.saturating_sub(1);
					if *deg == 0 {
						zero_in_degree.push(to.clone());
					}
				}
			}
		}
		zero_in_degree.sort_by(|a, b| {
			let depth_a = depth_map
				.get(a)
				.copied()
				.unwrap_or_else(|| ruma::UInt::new(0).unwrap());
			let depth_b = depth_map
				.get(b)
				.copied()
				.unwrap_or_else(|| ruma::UInt::new(0).unwrap());
			depth_b.cmp(&depth_a).then_with(|| b.cmp(a))
		});
	}
	Ok(get_missing_events::v1::Response { events })
}
