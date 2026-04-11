use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use conduwuit::{Event, Result, debug, info, utils::ReadyExt, warn};
use futures::{FutureExt, StreamExt};
use ruma::api::federation::event::get_room_state;

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

		// Check if room has remote members
		let Some(remote_server) = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|&s| s != self.services.globals.server_name())
			.next()
			.await
		else {
			return Ok(()); // Local-only or no other servers
		};

		warn!(
			target: "forwardfill",
			"Auto-Thumper: Room {room_id} is stagnant (latest PDU was {}ms ago). Thumping via {remote_server}...",
			now.saturating_sub(latest_pdu.origin_server_ts().get().into())
		);

		// Trigger catchup by requesting remote state at our latest point.
		// If we are behind, this will pull in missing PDUs.
		let request = get_room_state::v1::Request {
			room_id: room_id.to_owned(),
			event_id: latest_pdu.event_id().to_owned(),
		};

		let response = self
			.services
			.sending
			.send_federation_request(remote_server, request)
			.await?;

		for pdu in response.pdus {
			let (parsed_room_id, event_id, value) =
				self.services.event_handler.parse_incoming_pdu(&pdu).await?;

			if parsed_room_id != *room_id {
				info!(
					target: "forwardfill",
					"Auto-Thumper: Server {remote_server} returned PDU for room {parsed_room_id} while thumping {room_id}"
				);
				continue;
			}

			self.services
				.event_handler
				.handle_incoming_pdu(remote_server, room_id, &event_id, value, false)
				.await?;
		}

		Ok(())
	}
}
