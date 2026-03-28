use std::{
	collections::{BTreeMap, HashMap, HashSet},
	iter::once,
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
	let mut active_fetches = FuturesUnordered::new();
	let mut fetching: HashSet<OwnedEventId> =
		initial_set.clone().map(ToOwned::to_owned).collect();

	for id in initial_set {
		if self.services.pdu_metadata.is_event_soft_failed(id).await {
			info!(target: "backfill", "Skipping known soft-failed event: {id}");
			graph.insert(id.to_owned(), HashSet::new());
			continue;
		}

		let id = id.to_owned();
		active_fetches.push(
			async move {
				let res = self
					.fetch_and_handle_outliers(origin, once(id.as_ref()), create_event, room_id)
					.await;
				(id, res)
			}
			.boxed(),
		);
	}

	let mut amount = 0;

	while let Some((prev_event_id, mut fetched)) = active_fetches.next().await {
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
								if self
									.services
									.pdu_metadata
									.is_event_soft_failed(prev_prev)
									.await
								{
									info!(target: "backfill", "Skipping known soft-failed prev event: {prev_prev}");
									graph.insert(prev_prev.to_owned(), HashSet::new());
									continue;
								}

								if amount >= limit {
									info!(target: "backfill", "Max prev event limit reached! Limit: {limit}");
									graph.insert(prev_prev.to_owned(), HashSet::new());
									continue;
								}

								let prev_prev = prev_prev.to_owned();
								fetching.insert(prev_prev.clone());
								active_fetches.push(
									async move {
										let res = self
											.fetch_and_handle_outliers(
												origin,
												once(prev_prev.as_ref()),
												create_event,
												room_id,
											)
											.await;
										(prev_prev, res)
									}
									.boxed(),
								);
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
