use conduwuit::{
	Result,
	matrix::event::Event,
	utils::{ReadyExt, millis_since_unix_epoch},
	warn,
};
use futures::StreamExt;
use ruma::RoomId;

use super::Service;

impl Service {
	/// Periodic job to fix stuck federated rooms and force timeline advancement
	pub(super) async fn run_room_bumper(&self) -> Result<()> {
		let mut rooms_to_bump = Vec::new();

		let server_name = self.services.globals.server_name();
		self.services
			.state_cache
			.server_rooms(server_name)
			.ready_for_each(|room_id| rooms_to_bump.push(room_id.to_owned()))
			.await;

		for room_id in rooms_to_bump {
			// Pace database reads to avoid spinning CPU
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;

			if !self
				.services
				.state_cache
				.server_in_room(server_name, &room_id)
				.await
			{
				continue;
			}

			let local_users = self
				.services
				.state_cache
				.room_members(&room_id)
				.ready_filter(|user| self.services.globals.user_is_local(user))
				.count()
				.await;

			if local_users == 0 {
				continue; // Skip rooms with no local users
			}

			// Validate if the room is genuinely "stuck" or inactive for 24+ hours
			if let Ok(latest) = self.latest_pdu_in_room(&room_id).await {
				let ts = latest.origin_server_ts().get().into();
				let now = millis_since_unix_epoch();
				let age_ms = now.saturating_sub(ts);

				let days_old = age_ms / 1000 / 60 / 60 / 24;
				if days_old < 1 {
					continue; // Active recently, skip bump
				}
			} else {
				// No latest PDU could be determined? Skip it to be safe.
				continue;
			}

			if let Err(e) = self.bump_room(&room_id).await {
				warn!("Room bumper failed to append dummy event for room {room_id}: {e}");
			}
		}

		Ok(())
	}

	pub async fn bump_room(&self, room_id: &RoomId) -> Result<ruma::OwnedEventId> {
		let state_lock = self.services.state.mutex.lock(room_id).await;

		let pdu_builder = conduwuit::matrix::pdu::PduBuilder {
			event_type: "org.matrix.dummy_event".into(),
			content: serde_json::value::to_raw_value(&serde_json::json!({})).expect("valid json"),
			..Default::default()
		};

		let sender = self
			.services
			.state_cache
			.room_members(room_id)
			.ready_filter(|user| self.services.globals.user_is_local(user))
			.next()
			.await
			.ok_or_else(|| {
				conduwuit::err!(Request(Forbidden("No local users in room to send bump event")))
			})?;

		let event_id = self
			.build_and_append_pdu(pdu_builder, sender, Some(room_id), &state_lock)
			.await?;

		Ok(event_id)
	}
}
