use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use conduwuit::{Event, Result, debug, info, utils::ReadyExt, warn};
use futures::{FutureExt, StreamExt};
use ruma::OwnedServerName;

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
				timeline: args.depend::<crate::rooms::timeline::Service>("rooms::timeline"),
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
	timeline: Dep<crate::rooms::timeline::Service>,
	state_cache: Dep<crate::rooms::state_cache::Service>,
	event_handler: Dep<crate::rooms::event_handler::Service>,
	sending: Dep<crate::sending::Service>,
}

/// How long a room must be idle before being considered stale during the
/// periodic background sweep.
const PERIODIC_STALE_THRESHOLD_MS: u64 = 4 * 3600 * 1000; // 4 hours

/// How often the periodic sweep runs.
const SWEEP_INTERVAL_SECS: u64 = 3600; // every hour

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
		let ours = self.services.globals.server_name();
		let rooms = self.services.state_cache.server_rooms(ours);
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
		// Ensure we are actually participating in the room before we start
		// probes that could lead to unauthorized make_leave requests.
		let ours = self.services.globals.server_name();
		if !self
			.services
			.state_cache
			.server_in_room(ours, room_id)
			.await
		{
			return Ok(());
		}

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
			.filter(|s| !self.services.globals.server_is_ours(s))
		{
			if self.services.state_cache.server_in_room(hs, room_id).await {
				hs.to_owned()
			} else {
				self.services
					.state_cache
					.room_servers(room_id)
					.ready_filter(|&s| !self.services.globals.server_is_ours(s))
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
				.ready_filter(|&s| !self.services.globals.server_is_ours(s))
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

		// 1. Probe the remote server for its forward extremities via a make_leave
		//    template. We MUST use an active local user, or Synapse will return 403.
		let Some(user_id) = self
			.services
			.state_cache
			.active_local_users_in_room(room_id)
			.boxed()
			.next()
			.await
			.map(ToOwned::to_owned)
		else {
			return Ok(()); // We have no active local users, can't probe
		};

		let make_leave_request =
			ruma::api::federation::membership::prepare_leave_event::v1::Request {
				room_id: room_id.to_owned(),
				user_id,
			};

		let make_leave_response = match self
			.services
			.sending
			.send_federation_request(&target_server, make_leave_request)
			.await
		{
			| Ok(r) => r,
			| Err(e) => {
				warn!(
					target: "forwardfill",
					"make_leave probe failed for {room_id} via {target_server}: {e}"
				);
				return Ok(()); // abort if we can't get the forward extremities
			},
		};

		let leave_event_stub = match serde_json::from_str::<ruma::CanonicalJsonObject>(
			make_leave_response.event.get(),
		) {
			| Ok(s) => s,
			| Err(e) => {
				warn!(
					target: "forwardfill",
					"Invalid make_leave template from {target_server}: {e}"
				);
				return Ok(());
			},
		};

		let remote_latest_events: Vec<ruma::OwnedEventId> = leave_event_stub
			.get("prev_events")
			.and_then(|v| v.as_array())
			.map(|arr| {
				arr.iter()
					.filter_map(|v| {
						v.as_str().and_then(|s| {
							<&ruma::EventId>::try_from(s).ok().map(ToOwned::to_owned)
						})
					})
					.collect()
			})
			.unwrap_or_default();

		if remote_latest_events.is_empty() {
			return Ok(()); // nothing to do
		}

		// 2. Filter remote_latest_events to only those we DON'T know.
		// If we know all of them, the room isn't actually stale.
		let mut missing_latest = Vec::new();
		for event_id in remote_latest_events {
			if !self.services.timeline.pdu_exists(&event_id).await {
				missing_latest.push(event_id);
			}
		}

		if missing_latest.is_empty() {
			debug!(
				target: "forwardfill",
				"Room {room_id} is not actually stale; we have all forward extremities from \
				 {target_server}."
			);
			return Ok(());
		}

		info!(
			target: "forwardfill",
			"Room {room_id} is actually stale! Discovered {} missing forward extremities from \
			 {target_server}.",
			missing_latest.len()
		);

		// 3. Fetch the missing extremities and feed them to handle_incoming_pdu.
		// Passing `true` for fetch_prev tells Conduwuit to automatically use its own
		// robust native fetch_prev engine to stitch the DAG backwards!
		let mut handled = 0_usize;
		for event_id in missing_latest {
			let request = ruma::api::federation::event::get_event::v1::Request {
				event_id: event_id.clone(),
				include_unredacted_content: None,
			};

			let response = match self
				.services
				.sending
				.send_federation_request(&target_server, request)
				.await
			{
				| Ok(r) => r,
				| Err(e) => {
					warn!(target: "forwardfill", "Failed to fetch missing extremity {event_id}: {e}");
					continue;
				},
			};

			let (parsed_room_id, parsed_event_id, value) = match self
				.services
				.event_handler
				.parse_incoming_pdu(&response.pdu)
				.await
			{
				| Ok(v) => v,
				| Err(e) => {
					warn!(target: "forwardfill", "Failed to parse extremity {event_id}: {e}");
					continue;
				},
			};

			if parsed_room_id != *room_id {
				continue;
			}

			if let Err(e) = self
				.services
				.event_handler
				.handle_incoming_pdu(&target_server, room_id, &parsed_event_id, value, true)
				.await
			{
				warn!(target: "forwardfill", "Failed to handle extremity {event_id}: {e}");
			} else {
				handled = handled.saturating_add(1);
			}
		}

		if handled > 0 {
			info!(target: "forwardfill", "Successfully forward-filled {room_id} via {handled} extremities");
		}

		Ok(())
	}
}
