use conduwuit::implement;
use ruma::{
	EventId, RoomId, ServerName,
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
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
) -> bool {
	if event_id.server_name() == Some(origin) {
		return true;
	}

	if let Ok(pdu) = self.services.timeline.get_pdu(event_id).await {
		if pdu.sender.server_name() == origin
			|| pdu.origin.as_deref() == Some(origin)
			|| pdu.kind == TimelineEventType::RoomCreate
		{
			return true;
		}
	}

	let Ok(shortstatehash) = self.pdu_shortstatehash(event_id).await else {
		return self
			.services
			.state_cache
			.server_is_participant(origin, room_id)
			.await;
	};

	let history_visibility = self
		.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	match history_visibility {
		| HistoryVisibility::Invited => {
			// Allow if any member on requesting server was AT LEAST invited, else deny
			self.services
				.state_cache
				.server_is_participant(origin, room_id)
				.await
		},
		| HistoryVisibility::Joined => {
			// Allow if any member on requesting server was joined, else deny
			self.services
				.state_cache
				.server_in_room(origin, room_id)
				.await
		},
		| _ => true,
	}
}
