use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use conduwuit::{Event, Result, debug, info, utils::ReadyExt, warn};
use futures::{FutureExt, StreamExt};
use ruma::OwnedServerName;

use crate::service::Dep;

pub struct Service {
	pub(crate) services: InnerServices,
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

pub(crate) struct InnerServices {
	pub(crate) server: Arc<conduwuit::Server>,
	pub(crate) globals: Dep<crate::globals::Service>,
	pub(crate) timeline: Dep<crate::rooms::timeline::Service>,
	pub(crate) state_cache: Dep<crate::rooms::state_cache::Service>,
	pub(crate) event_handler: Dep<crate::rooms::event_handler::Service>,
	pub(crate) sending: Dep<crate::sending::Service>,
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
		// On boot, check every federated room that hasn't had an event in the
		// last 5 minutes. This covers missed events from downtime while avoiding
		// immediate probes for rooms that were just active.
		info!(target: "forwardfill", "Running startup forward-fill scan (all federated rooms)...");
		self.scan_all_rooms_startup(5 * 60 * 1000).await;
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
		self.scan_all_rooms_inner(stale_threshold_ms, 10).await;
	}

	/// Startup variant with reduced concurrency to avoid stack overflow
	/// on cold boot (deep event_handler call chains with empty caches).
	async fn scan_all_rooms_startup(&self, stale_threshold_ms: u64) {
		self.scan_all_rooms_inner(stale_threshold_ms, 1).await;
	}

	async fn scan_all_rooms_inner(&self, stale_threshold_ms: u64, concurrency: usize) {
		let ours = self.services.globals.server_name();

		self.services
			.state_cache
			.server_rooms(ours)
			.for_each_concurrent(concurrency, |room_id| async move {
				if let Err(e) = self.check_room(room_id, stale_threshold_ms).boxed().await {
					debug!(target: "forwardfill", "Error checking room {room_id}: {e}");
				}
			})
			.await;
	}

	pub async fn check_room(
		&self,
		room_id: &ruma::RoomId,

		stale_threshold_ms: u64,
	) -> Result<()> {
		let room_str = room_id.as_str();
		if !room_str.is_ascii()
			|| <&ruma::RoomId>::try_from(room_str).is_err()
			|| room_id.server_name().is_none()
		{
			warn!(
				target: "forwardfill",
				"Skipping room with invalid/corrupt ID ({} bytes)",
				room_str.len()
			);
			return Ok(());
		}

		// Ensure we are actually participating in the room before we start
		// probes that could lead to unauthorized make_join requests.
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

		// Build candidate server list: prefer room homeserver, then any remote
		// participants. We try multiple servers instead of giving up on one.
		let mut candidate_servers: Vec<OwnedServerName> = Vec::new();

		// Prefer the room's homeserver if it's remote and in the room
		if let Some(hs) = room_id
			.server_name()
			.filter(|s| !self.services.globals.server_is_ours(s))
		{
			if self.services.state_cache.server_in_room(hs, room_id).await {
				candidate_servers.push(hs.to_owned());
			}
		}

		// Add other remote servers (up to 5 candidates total)
		let other_servers: Vec<OwnedServerName> = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|&s| {
				!self.services.globals.server_is_ours(s)
					&& !self
						.services
						.server
						.config
						.forbidden_remote_server_names
						.is_match(s.host())
			})
			.map(ToOwned::to_owned)
			.collect()
			.await;

		for server in other_servers {
			if candidate_servers.len() >= 5 {
				break;
			}
			if !candidate_servers.contains(&server) {
				candidate_servers.push(server);
			}
		}

		if candidate_servers.is_empty() {
			return Ok(()); // Local-only room, nothing to forward-fill
		}

		// Grab an active local user to use for the probe.
		let user_id = {
			let mut users = self
				.services
				.state_cache
				.active_local_users_in_room(room_id)
				.boxed();
			match users.next().await {
				| Some(u) => u.to_owned(),
				| None => {
					info!(
						target: "forwardfill",
						"Skipping stagnant room {room_id} due to no joined local users."
					);
					return Ok(());
				},
			}
		};

		// Try each candidate server for the probe + fetch
		for target_server in &candidate_servers {
			info!(
				target: "forwardfill",
				"Room {room_id} is stagnant (latest PDU was {}ms ago). Probing {target_server} for extremities via {user_id}...",
				now.saturating_sub(latest_pdu.origin_server_ts().get().into())
			);

			let make_join_request =
				ruma::api::federation::membership::prepare_join_event::v1::Request {
					room_id: room_id.to_owned(),
					user_id: user_id.clone(),
					ver: self.services.server.supported_room_versions().collect(),
				};

			let probe_response = match self
				.services
				.sending
				.send_federation_request(target_server, make_join_request)
				.await
			{
				| Ok(r) => r.event,
				| Err(e) => {
					warn!(
						target: "forwardfill",
						"Probe failed for {room_id} via {target_server} (user {user_id}): {e}"
					);
					continue; // Try next server
				},
			};

			let event_stub =
				match serde_json::from_str::<ruma::CanonicalJsonObject>(probe_response.get()) {
					| Ok(s) => s,
					| Err(e) => {
						warn!(
							target: "forwardfill",
							"Invalid probe template from {target_server}: {e}"
						);
						continue; // Try next server
					},
				};

			let remote_latest_events: Vec<ruma::OwnedEventId> = event_stub
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
				continue; // Try next server
			}

			// Filter to only events we don't have
			let mut missing_latest = Vec::new();
			for event_id in remote_latest_events {
				if !self.services.timeline.pdu_exists(&event_id).await {
					missing_latest.push(event_id);
				}
			}

			if missing_latest.is_empty() {
				debug!(target: "forwardfill", "Room {room_id} is not actually stale; we have all forward extremities from {target_server}.");
				return Ok(());
			}

			info!(
				target: "forwardfill",
				"Room {room_id} is actually stale! Discovered {} missing forward extremities from {target_server}.",
				missing_latest.len()
			);

			// Fetch the missing extremities
			let mut handled = 0_usize;
			for event_id in missing_latest {
				let request = ruma::api::federation::event::get_event::v1::Request {
					event_id: event_id.clone(),
					include_unredacted_content: None,
				};

				let response = match self
					.services
					.sending
					.send_federation_request(target_server, request)
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
					.handle_incoming_pdu(target_server, room_id, &parsed_event_id, value, true)
					.await
				{
					warn!(target: "forwardfill", "Failed to handle extremity {event_id}: {e}");
				} else {
					handled = handled.saturating_add(1);
				}
			}

			if handled > 0 {
				info!(target: "forwardfill", "Successfully forward-filled {room_id} via {handled} extremities from {target_server}");
			}

			// We got a valid probe from this server, done with this room
			return Ok(());
		}

		Ok(())
	}
}
