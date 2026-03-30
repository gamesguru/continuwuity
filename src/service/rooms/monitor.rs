use std::{sync::Arc, time::Duration};

use conduwuit::{Result, debug, info, warn};
use futures::StreamExt;
use ruma::api::federation::event::get_room_state;

use crate::Services;

pub struct Service {
	services: Services,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: Services {
				server: args.server.clone(),
				globals: args.depend::<crate::globals::Service>("globals"),
				rooms: args.depend::<crate::rooms::Service>("rooms"),
				sending: args.depend::<crate::sending::Service>("sending"),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

struct Services {
	server: Arc<conduwuit::Server>,
	globals: crate::Dep<crate::globals::Service>,
	rooms: crate::Dep<crate::rooms::Service>,
	sending: crate::Dep<crate::sending::Service>,
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
			let rooms = self.services.rooms.metadata.iter_ids();
			let mut room_stream = rooms.boxed();

			while let Some(room_id) = room_stream.next().await {
				if let Err(e) = self.check_room(room_id).await {
					debug!("Auto-Thumper: Error checking room {room_id}: {e}");
				}
				tokio::task::yield_now().await;
			}
		}
	}

	async fn check_room(&self, room_id: &ruma::RoomId) -> Result<()> {
		let latest_pdu = self
			.services
			.rooms
			.timeline
			.latest_pdu_in_room(room_id)
			.await?;
		let now = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.expect("time went backwards")
			.as_millis() as u64;

		let stale_threshold = 4 * 3600 * 1000; // 4 hours
		if now.saturating_sub(latest_pdu.origin_server_ts().into()) < stale_threshold {
			return Ok(());
		}

		// Check if room has remote members
		let mut remote_servers = self.services.rooms.state_cache.room_servers(room_id);
		let Some(remote_server) = remote_servers
			.next()
			.await
			.filter(|&s| s != self.services.globals.server_name())
		else {
			return Ok(()); // Local-only or no other servers
		};

		warn!(
			target: "backfill",
			"Auto-Thumper: Room {room_id} is stagnant (latest PDU was {ts}ms ago). Thumping via {remote_server}...",
			ts = now.saturating_sub(latest_pdu.origin_server_ts().into())
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
			let (event_id, value) = self
				.services
				.rooms
				.event_handler
				.parse_incoming_pdu(&pdu)
				.await?;
			self.services
				.rooms
				.event_handler
				.handle_incoming_pdu(remote_server, room_id, &event_id, value, false)
				.await?;
		}

		Ok(())
	}
}
