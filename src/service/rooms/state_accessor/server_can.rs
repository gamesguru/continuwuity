use conduwuit::implement;
use futures::{StreamExt, future::ready};
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
			.server_in_room(origin, room_id)
			.await;
	};

	let history_visibility = self
		.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	match history_visibility {
		| HistoryVisibility::WorldReadable => true,
		| HistoryVisibility::Shared | HistoryVisibility::Invited => {
			// Allow if any member on requesting server is AT LEAST invited, else deny
			let members = self
				.services
				.state_cache
				.room_members(room_id)
				.chain(self.services.state_cache.room_members_invited(room_id))
				.map(ToOwned::to_owned)
				.filter(|member| ready(member.server_name() == origin))
				.collect::<Vec<_>>()
				.await;

			for member in members {
				if self.user_was_invited(shortstatehash, &member).await {
					return true;
				}
			}

			false
		},
		| HistoryVisibility::Joined => {
			// Allow if any member on requesting server is joined, else deny
			let members = self
				.services
				.state_cache
				.room_members(room_id)
				.map(ToOwned::to_owned)
				.filter(|member| ready(member.server_name() == origin))
				.collect::<Vec<_>>()
				.await;

			for member in members {
				if self.user_was_joined(shortstatehash, &member).await {
					return true;
				}
			}

			false
		},
		| _ => true,
	}
}
