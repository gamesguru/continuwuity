use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use conduwuit::{Event, Result, debug, info, utils::ReadyExt, warn};
use futures::{FutureExt, StreamExt};
use ruma::{OwnedEventId, OwnedServerName, api::federation::event::get_missing_events, uint};

use crate::service::Dep;

pub struct Service {
	services: InnerServices,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: InnerServices {
				server: args.server.clone(),
				globals: args.depend::<crate::globals::Service>("globals"),
				metadata: args.depend::<crate::rooms::metadata::Service>("rooms::metadata"),
				timeline: args.depend::<crate::rooms::timeline::Service>("rooms::timeline"),
				state: args.depend::<crate::rooms::state::Service>("rooms::state"),
				state_accessor: args
					.depend::<crate::rooms::state_accessor::Service>("rooms::state_accessor"),
				state_cache: args
					.depend::<crate::rooms::state_cache::Service>("rooms::state_cache"),
				event_handler: args
					.depend::<crate::rooms::event_handler::Service>("rooms::event_handler"),
				sending: args.depend::<crate::sending::Service>("sending"),
			},
		}))
	}

	async fn worker(self: Arc<Self>) -> Result<()> { self.worker().await }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

struct InnerServices {
	server: Arc<conduwuit::Server>,
	globals: Dep<crate::globals::Service>,
	metadata: Dep<crate::rooms::metadata::Service>,
	timeline: Dep<crate::rooms::timeline::Service>,
	state: Dep<crate::rooms::state::Service>,
	state_accessor: Dep<crate::rooms::state_accessor::Service>,
	state_cache: Dep<crate::rooms::state_cache::Service>,
	event_handler: Dep<crate::rooms::event_handler::Service>,
	sending: Dep<crate::sending::Service>,
}

/// How long a room must be idle before being considered stale during the
/// periodic background sweep.
const PERIODIC_STALE_THRESHOLD_MS: u64 = 4 * 3600 * 1000; // 4 hours

/// How often the periodic sweep runs.
const SWEEP_INTERVAL_SECS: u64 = 3600; // every hour

/// Maximum events per get_missing_events request.
const BATCH_LIMIT: usize = 100;

impl Service {
	pub async fn worker(self: Arc<Self>) -> Result<()> {
		if !self.services.server.config.allow_federation {
			return Ok(());
		}

		// --- Startup scan ---
		// On boot, check every federated room unconditionally. This covers
		// missed events from downtime, restarts, or network outages.
		info!(target: "forwardfill", "Running startup forward-fill scan (all federated rooms)...");
		self.scan_all_rooms(0).await;
		info!(target: "forwardfill", "Startup forward-fill scan complete.");

		// --- Periodic sweep ---
		let mut interval = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
		// consume the immediate first tick so we don't double-scan on startup
		interval.tick().await;

		loop {
			interval.tick().await;

			if !self.services.server.config.allow_federation {
				continue;
			}

			debug!(target: "forwardfill", "Starting periodic forward-fill sweep...");
			self.scan_all_rooms(PERIODIC_STALE_THRESHOLD_MS).await;
		}
	}

	/// Scans all known rooms and fetches missing events for any that have been
	/// idle longer than `stale_threshold_ms`.
	async fn scan_all_rooms(&self, stale_threshold_ms: u64) {
		let rooms = self.services.metadata.iter_ids();
		let mut room_stream = rooms.boxed();

		while let Some(room_id) = room_stream.next().await {
			if let Err(e) = self.check_room(room_id, stale_threshold_ms).boxed().await {
				debug!(target: "forwardfill", "Error checking room {room_id}: {e}");
			}
			// yield so we don't starve other tasks
			tokio::task::yield_now().await;
		}
	}

	async fn check_room(&self, room_id: &ruma::RoomId, stale_threshold_ms: u64) -> Result<()> {
		let latest_pdu = self.services.timeline.latest_pdu_in_room(room_id).await?;
		let now: u64 = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.expect("time went backwards")
			.as_millis()
			.try_into()
			.expect("time overflow");

		if now.saturating_sub(latest_pdu.origin_server_ts().get().into()) < stale_threshold_ms {
			return Ok(());
		}

		// Determine the best server to query: prefer the room's homeserver
		// (authoritative), then fall back to any known remote participant.
		let target_server: OwnedServerName = if let Some(hs) = room_id
			.server_name()
			.filter(|s| *s != self.services.globals.server_name())
		{
			if self.services.state_cache.server_in_room(hs, room_id).await {
				hs.to_owned()
			} else {
				self.services
					.state_cache
					.room_servers(room_id)
					.ready_filter(|&s| s != self.services.globals.server_name())
					.next()
					.await
					.map(ToOwned::to_owned)
					.ok_or_else(|| conduwuit::err!("No remote servers in room {room_id}"))?
			}
		} else {
			let Some(server) = self
				.services
				.state_cache
				.room_servers(room_id)
				.ready_filter(|&s| s != self.services.globals.server_name())
				.next()
				.await
			else {
				return Ok(()); // Local-only room, nothing to forward-fill
			};
			server.to_owned()
		};

		warn!(
			target: "forwardfill",
			"Room {room_id} is stagnant (latest PDU was {}ms ago). Checking for missing \
			 events via {target_server}...",
			now.saturating_sub(latest_pdu.origin_server_ts().get().into())
		);

		// Collect our current state event IDs as the DAG anchors.
		let shortstatehash = self.services.state.get_room_shortstatehash(room_id).await?;
		let state_event_ids: Vec<OwnedEventId> = self
			.services
			.state_accessor
			.state_full_ids(shortstatehash)
			.map(|(_, event_id): (_, OwnedEventId)| event_id)
			.collect()
			.await;

		if state_event_ids.is_empty() {
			return Ok(());
		}

		// Use our timeline tip as the earliest_events anchor, updated after
		// each batch so we paginate forward through the gap.
		let mut earliest = vec![latest_pdu.event_id().to_owned()];
		let mut total_fetched: usize = 0;

		for batch in 0_usize.. {
			let request = get_missing_events::v1::Request {
				room_id: room_id.to_owned(),
				limit: uint!(100),
				min_depth: uint!(0),
				earliest_events: earliest.clone(),
				latest_events: state_event_ids.clone(),
			};

			let response = match self
				.services
				.sending
				.send_federation_request(&target_server, request)
				.await
			{
				| Ok(r) => r,
				| Err(e) => {
					warn!(
						target: "forwardfill",
						"Federation request failed for {room_id} via {target_server} \
						 (batch {batch}): {e}"
					);
					break;
				},
			};

			let count = response.events.len();
			if count == 0 {
				if batch == 0 {
					debug!(
						target: "forwardfill",
						"No missing events returned for {room_id}"
					);
				}
				break;
			}

			info!(
				target: "forwardfill",
				"{count} missing events returned for {room_id} from {target_server} \
				 (batch {batch})"
			);

			let mut batch_handled: usize = 0;
			for pdu in response.events {
				let (parsed_room_id, event_id, value) =
					match self.services.event_handler.parse_incoming_pdu(&pdu).await {
						| Ok(v) => v,
						| Err(e) => {
							info!(
								target: "forwardfill",
								"Failed to parse PDU from {target_server}: {e}"
							);
							continue;
						},
					};

				if parsed_room_id != *room_id {
					info!(
						target: "forwardfill",
						"{target_server} returned PDU for room {parsed_room_id} while \
						 filling {room_id}"
					);
					continue;
				}

				match self
					.services
					.event_handler
					.handle_incoming_pdu(&target_server, room_id, &event_id, value, true)
					.await
				{
					| Ok(_) => {
						batch_handled = batch_handled.saturating_add(1);
						// advance the pagination anchor
						earliest = vec![event_id];
					},
					| Err(e) => {
						info!(
							target: "forwardfill",
							"Failed to handle PDU {event_id} from {target_server}: {e}"
						);
					},
				}
			}

			total_fetched = total_fetched.saturating_add(batch_handled);

			// If we got fewer events than the limit, we're caught up
			if count < BATCH_LIMIT {
				break;
			}

			// yield between batches
			tokio::task::yield_now().await;
		}

		if total_fetched > 0 {
			info!(
				target: "forwardfill",
				"Forward-filled {total_fetched} events into {room_id} from {target_server}"
			);
		}

		Ok(())
	}
}
