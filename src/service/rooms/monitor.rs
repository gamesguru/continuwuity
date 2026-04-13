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

impl Service {
	pub async fn worker(self: Arc<Self>) -> Result<()> {
		let mut interval = tokio::time::interval(Duration::from_secs(3600)); // Run every hour
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		loop {
			interval.tick().await;
			if !self.services.server.config.allow_federation {
				continue;
			}

			debug!("Auto-Thumper: Scanning rooms for staleness...");
			let rooms = self.services.metadata.iter_ids();
			let mut room_stream = rooms.boxed();

			while let Some(room_id) = room_stream.next().await {
				if let Err(e) = self.check_room(room_id).boxed().await {
					debug!("Auto-Thumper: Error checking room {room_id}: {e}");
				}
				tokio::task::yield_now().await;
			}
		}
	}

	async fn check_room(&self, room_id: &ruma::RoomId) -> Result<()> {
		let latest_pdu = self.services.timeline.latest_pdu_in_room(room_id).await?;
		let now: u64 = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.expect("time went backwards")
			.as_millis()
			.try_into()
			.expect("time overflow");

		let stale_threshold = 4 * 3600 * 1000; // 4 hours
		if now.saturating_sub(latest_pdu.origin_server_ts().get().into()) < stale_threshold {
			return Ok(());
		}

		// Determine the best server to query: prefer the room's homeserver
		// (authoritative), then fall back to any known remote participant.
		let target_server: OwnedServerName = if let Some(hs) = room_id
			.server_name()
			.filter(|s| *s != self.services.globals.server_name())
			.filter(|_| {
				// We check participation synchronously below; use a placeholder
				true
			}) {
			// Verify the room homeserver is actually participating
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
			"Room {room_id} is stagnant (latest PDU was {}ms ago). Checking for missing events via {target_server}...",
			now.saturating_sub(latest_pdu.origin_server_ts().get().into())
		);

		// Collect our current state event IDs. These are events both sides of
		// the federation know about. We pass them as `latest_events` so the
		// remote walks backwards through the DAG from those anchor points and
		// returns any timeline events that exist between them and our timeline
		// tip (`earliest_events`) that we may have missed due to dropped
		// federation transactions.
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

		let request = get_missing_events::v1::Request {
			room_id: room_id.to_owned(),
			limit: uint!(20),
			min_depth: uint!(0),
			earliest_events: vec![latest_pdu.event_id().to_owned()],
			latest_events: state_event_ids,
		};

		let response = self
			.services
			.sending
			.send_federation_request(&target_server, request)
			.await?;

		if response.events.is_empty() {
			debug!(target: "forwardfill", "No missing events returned for {room_id}");
			return Ok(());
		}

		info!(
			target: "forwardfill",
			"{} missing events returned for {room_id} from {target_server}",
			response.events.len()
		);

		for pdu in response.events {
			let (parsed_room_id, event_id, value) =
				match self.services.event_handler.parse_incoming_pdu(&pdu).await {
					| Ok(v) => v,
					| Err(e) => {
						debug!(target: "forwardfill", "Failed to parse PDU from {target_server}: {e}");
						continue;
					},
				};

			if parsed_room_id != *room_id {
				info!(
					target: "forwardfill",
					"{target_server} returned PDU for room {parsed_room_id} while filling {room_id}"
				);
				continue;
			}

			if let Err(e) = self
				.services
				.event_handler
				.handle_incoming_pdu(&target_server, room_id, &event_id, value, false)
				.await
			{
				debug!(target: "forwardfill", "Failed to handle PDU {event_id} from {target_server}: {e}");
			}
		}

		Ok(())
	}
}
