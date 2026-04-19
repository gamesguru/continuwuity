use conduwuit::implement;
use futures::StreamExt;
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName,
	events::{
		StateEventType, TimelineEventType,
		room::history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
	},
};

/// Whether a server is allowed to see an event through federation, based on
/// the room's history_visibility at that event's state.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn server_can_see_event(
	&self,
	origin: OwnedServerName,
	room_id: OwnedRoomId,
	event_id: OwnedEventId,
) -> bool {
	if event_id.server_name() == Some(&origin) {
		return true;
	}

	if let Ok(pdu) = self.services.timeline.get_pdu(&event_id).await {
		if pdu.sender.server_name() == origin
			|| pdu.origin.as_deref() == Some(&origin)
			|| pdu.kind == TimelineEventType::RoomCreate
		{
			return true;
		}
	}

	let Ok(shortstatehash) = self.pdu_shortstatehash(&event_id).await else {
		// Fallback: If we don't know the state at the event, use current room
		// visibility
		if self.is_world_readable(&room_id).await {
			return true;
		}

		let server_is_participant = self
			.services
			.state_cache
			.server_is_participant(&origin, &room_id)
			.await;

		if server_is_participant {
			if let Ok(shortstatehash) =
				self.services.state.get_room_shortstatehash(&room_id).await
			{
				let history_visibility = self
					.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
					.await
					.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
						c.history_visibility
					});

				if history_visibility == HistoryVisibility::Shared {
					return true;
				}
			}
		}

		return server_is_participant;
	};

	let history_visibility = self
		.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	match history_visibility {
		| HistoryVisibility::WorldReadable => true,
		| HistoryVisibility::Shared => {
			// Spec: all servers whose users have joined the room can see all history
			self.services
				.state_cache
				.server_is_participant(&origin, &room_id)
				.await
		},
		| HistoryVisibility::Invited => {
			// Allow if any member on requesting server was AT LEAST invited at that state
			let members: Vec<ruma::OwnedUserId> = self
				.services
				.state_cache
				.room_useroncejoined(&room_id)
				.chain(self.services.state_cache.room_members_invited(&room_id))
				.map(ToOwned::to_owned)
				.collect()
				.await;

			for member in members {
				if member.server_name() == origin
					&& self.user_was_invited(shortstatehash, &member).await
				{
					return true;
				}
			}

			false
		},
		| HistoryVisibility::Joined => {
			// Allow if any member on requesting server was joined at that state
			let members: Vec<ruma::OwnedUserId> = self
				.services
				.state_cache
				.room_useroncejoined(&room_id)
				.map(ToOwned::to_owned)
				.collect()
				.await;

			for member in members {
				if member.server_name() == origin
					&& self.user_was_joined(shortstatehash, &member).await
				{
					return true;
				}
			}

			false
		},
		| _ => false,
	}
}
