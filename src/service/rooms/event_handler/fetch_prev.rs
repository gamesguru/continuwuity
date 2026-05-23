use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque},
	iter::once,
	time::{Duration, Instant},
};

use conduwuit::{
	Event, PduEvent, Result, err, implement, info,
	state_res::{self},
};
use futures::{
	FutureExt, future,
	stream::{FuturesUnordered, StreamExt},
};
use ruma::{
	CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, ServerName,
	int, uint,
};

use super::check_room_id;

#[implement(super::Service)]
#[tracing::instrument(
    level = "debug",
	skip_all,
	fields(%origin),
)]
#[allow(clippy::type_complexity)]
pub(super) async fn fetch_prev<'a, Pdu, Events>(
	&self,
	origin: &ServerName,
	create_event: &Pdu,
	room_id: &RoomId,
	first_ts_in_room: MilliSecondsSinceUnixEpoch,
	initial_set: Events,
) -> Result<(
	Vec<OwnedEventId>,
	HashMap<OwnedEventId, (PduEvent, BTreeMap<String, CanonicalJsonValue>)>,
)>
where
	Pdu: Event + Send + Sync,
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let num_ids = initial_set.clone().count();
	let mut eventid_info = HashMap::new();
	let mut graph: HashMap<OwnedEventId, _> = HashMap::with_capacity(num_ids);
	let limit = self.services.server.config.max_fetch_prev_events;
	let mut active_fetches = FuturesUnordered::new();
	let mut fetching: HashSet<OwnedEventId> = HashSet::new();
	let mut todo: VecDeque<OwnedEventId> = VecDeque::new();

	for id in initial_set {
		todo.push_back(id.to_owned());
	}

	let mut amount: u64 = 0;
	let started = Instant::now();
	let budget = Duration::from_secs(self.services.server.config.fetch_prev_timeout);
	loop {
		if started.elapsed() > budget {
			info!(
				elapsed = ?started.elapsed(),
				fetched = amount,
				remaining = todo.len(),
				"fetch_prev: wall-clock budget exhausted, proceeding with partial results"
			);
			for id in todo {
				graph.insert(id, HashSet::new());
			}
			break;
		}
		// Fill active_fetches from todo up to concurrency limit and total budget
		let fetch_width = self.services.server.concurrency_scaled(2);
		while active_fetches.len() < fetch_width
			&& !todo.is_empty()
			&& amount.saturating_add(u64::try_from(active_fetches.len()).unwrap_or(u64::MAX))
				< limit.into()
		{
			let id = todo.pop_front().expect("not empty");
			if self.services.pdu_metadata.is_event_soft_failed(&id).await {
				info!(target: "backfill", "Skipping known soft-failed event: {id}");
				graph.insert(id, HashSet::new());
				continue;
			}

			if fetching.contains(&id) {
				continue;
			}

			fetching.insert(id.clone());
			active_fetches.push(
				async move {
					let res = self
						.fetch_and_handle_outliers(
							origin,
							once(id.as_ref()),
							Some(create_event),
							room_id,
							false,
						)
						.await;
					(id, res)
				}
				.boxed(),
			);
		}

		if active_fetches.is_empty() {
			// If we still have todo items but active_fetches is empty, we hit the limit
			for id in todo {
				graph.insert(id, HashSet::new());
			}
			break;
		}

		let Some((prev_event_id, mut fetched)) = active_fetches.next().await else {
			break;
		};

		self.services.server.check_running()?;

		match fetched.pop() {
			| Some((pdu, mut json_opt)) => {
				check_room_id(room_id, &pdu)?;

				if json_opt.is_none() {
					json_opt = self
						.services
						.outlier
						.get_outlier_pdu_json(&prev_event_id)
						.await
						.ok();
				}

				if let Some(json) = json_opt {
					if pdu.origin_server_ts() > first_ts_in_room {
						amount = amount.saturating_add(1);
						for prev_prev in pdu.prev_events() {
							if !graph.contains_key(prev_prev) && !fetching.contains(prev_prev) {
								todo.push_back(prev_prev.to_owned());
							}
						}

						graph.insert(
							prev_event_id.clone(),
							pdu.prev_events().map(ToOwned::to_owned).collect(),
						);
					} else {
						// Time based check failed
						graph.insert(prev_event_id.clone(), HashSet::new());
					}

					eventid_info.insert(prev_event_id, (pdu, json));
				} else {
					// Get json failed, so this was not fetched over federation
					graph.insert(prev_event_id, HashSet::new());
				}
			},
			| _ => {
				// Fetch and handle failed
				graph.insert(prev_event_id, HashSet::new());
			},
		}
		tokio::task::yield_now().await;
	}

	let event_fetch = |event_id| {
		let origin_server_ts = eventid_info
			.get(&event_id)
			.map_or_else(|| uint!(0), |info| info.0.origin_server_ts().get());

		// This return value is the key used for sorting events,
		// events are then sorted by power level, time,
		// and lexically by event_id.
		future::ok((int!(0), MilliSecondsSinceUnixEpoch(origin_server_ts)))
	};

	let sorted = state_res::lexicographical_topological_sort(&graph, &event_fetch)
		.await
		.map_err(|e| err!(Database(error!("Error sorting prev events: {e}"))))?;

	Ok((sorted, eventid_info))
}
