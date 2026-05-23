use std::{collections::HashSet, sync::Arc, time::Duration};

use conduwuit::{debug, info, trace, warn};
use dashmap::DashMap;
use ruma::OwnedEventId;
use tokio::sync::mpsc;

use super::{HealRequest, Service};

pub(crate) async fn healer_worker(
	mut receiver: mpsc::UnboundedReceiver<HealRequest>,
	service: Arc<Service>,
) {
	info!("DAG Healer worker started");

	// Cache to prevent infinite fetch loops for missing events that are truly 404
	// on all fallback servers. Keys are EventIds, values are timestamp of last
	// attempt. For simplicity, we just keep a bounded or periodic-cleared cache.
	let failed_heals: DashMap<OwnedEventId, std::time::Instant> = DashMap::new();

	// Periodically clean up the cache
	let cache_cleanup_interval = Duration::from_secs(60 * 60); // 1 hour
	let mut last_cleanup = std::time::Instant::now();

	while let Some(request) = receiver.recv().await {
		let now = std::time::Instant::now();
		if now.duration_since(last_cleanup) > cache_cleanup_interval {
			failed_heals.retain(|_, v| now.duration_since(*v) < cache_cleanup_interval);
			last_cleanup = now;
		}

		match request {
			| HealRequest::MissingEvents { room_id, missing_events } => {
				// Deduplicate and filter out events we've recently failed to fetch
				let to_fetch: Vec<OwnedEventId> = missing_events
					.into_iter()
					.filter(|id| {
						if let Some(last_attempt) = failed_heals.get(id) {
							if now.duration_since(*last_attempt) < Duration::from_secs(60 * 15) {
								// Wait 15 minutes before retrying a 404'd event
								return false;
							}
						}
						true
					})
					.collect::<HashSet<_>>() // Deduplicate
					.into_iter()
					.collect();

				if to_fetch.is_empty() {
					continue;
				}

				debug!(
					room_id = ?room_id,
					count = to_fetch.len(),
					"DAG Healer attempting to fetch missing events"
				);

				let fallback_servers = service
					.build_federation_server_list(
						&room_id,
						service.services.globals.server_name(),
						8,
					)
					.await;

				let create_event = match service
					.services
					.state_accessor
					.room_state_get(&room_id, &ruma::events::StateEventType::RoomCreate, "")
					.await
				{
					| Ok(pdu) => pdu,
					| Err(e) => {
						warn!(room_id = ?room_id, error = ?e, "DAG Healer failed to get room create event");
						continue;
					},
				};

				let mut fetched_count = 0_usize;

				for chunk in to_fetch.chunks(20) {
					// fetch_and_handle_outliers automatically queries all fallback servers,
					// so we only need to invoke it once, using the first available server
					// (or our own) as the seed origin.
					let our_server = service.services.globals.server_name().to_owned();
					let seed_server = fallback_servers.first().unwrap_or(&our_server).clone();

					trace!(
						chunk_size = chunk.len(),
						server = ?seed_server,
						"DAG Healer fetching event chunk via fallback routing"
					);

					let fetched = service
						.fetch_and_handle_outliers(
							&seed_server,
							chunk.iter().map(AsRef::as_ref),
							Some(&create_event),
							&room_id,
						)
						.await;

					let fetched_set: HashSet<OwnedEventId> =
						fetched.into_iter().map(|(pdu, _)| pdu.event_id).collect();

					for event_id in chunk {
						if fetched_set.contains(event_id) {
							info!(event_id = ?event_id, "DAG Healer successfully fetched missing event");
							fetched_count = fetched_count.saturating_add(1);
						} else {
							info!(event_id = ?event_id, "DAG Healer failed to fetch event from all fallback servers, marking as 404");
							failed_heals.insert(event_id.clone(), std::time::Instant::now());
						}
					}

					// Yield to the executor between chunks to prevent starving
					// client request handling on resource-constrained boxes.
					tokio::task::yield_now().await;
				}

				let failed_count = to_fetch.len().saturating_sub(fetched_count);
				if fetched_count > 0 || failed_count > 0 {
					info!(
						room_id = ?room_id,
						fetched = fetched_count,
						failed = failed_count,
						total = to_fetch.len(),
						"DAG Healer batch complete"
					);
				}
			},
			| HealRequest::MissingState { room_id, event_id, origin } => {
				debug!(room_id = ?room_id, event_id = ?event_id, "DAG Healer fetching missing state for event");
				let create_event = match service
					.services
					.state_accessor
					.room_state_get(&room_id, &ruma::events::StateEventType::RoomCreate, "")
					.await
				{
					| Ok(pdu) => pdu,
					| Err(e) => {
						info!(
							target: "state_res",
							room_id = ?room_id, error = ?e, "DAG Healer failed to get room create event"
						);
						continue;
					},
				};
				let _ = service
					.fetch_state(&origin, &create_event, &room_id, &event_id)
					.await;
			},
		}

		// Small delay between heal requests to avoid monopolizing the
		// executor when the channel has a large backlog.
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}
