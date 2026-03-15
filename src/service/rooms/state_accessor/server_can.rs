use conduwuit::{debug_warn, implement, utils::stream::ReadyExt, Event};
use futures::StreamExt;
use ruma::{
	EventId, RoomId, ServerName,
	events::{
		StateEventType,
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
	let Ok(pdu) = self.services.timeline.get_pdu(event_id).await else {
		debug_warn!(
			"Unable to visibility check event {} in room {} for server {}: pdu not found",
			event_id,
			room_id,
			origin
		);
		return false;
	};

	if pdu.sender().server_name() == origin {
		return true;
	}

	if pdu.kind() == &ruma::events::TimelineEventType::RoomMember {
		if let Some(state_key) = pdu.state_key() {
			if let Ok(user_id) = <&ruma::UserId>::try_from(state_key) {
				if user_id.server_name() == origin {
					return true;
				}
			}
		}
	}

	let Ok(shortstatehash) = self.pdu_shortstatehash(event_id).await else {
		debug_warn!(
			"Unable to visibility check event {} in room {} for server {}: shortstatehash not \
			 found",
			event_id,
			room_id,
			origin
		);
		return false;
	};

	let history_visibility = self
		.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	let current_server_members = self
		.services
		.state_cache
		.room_members(room_id)
		.ready_filter(|member| member.server_name() == origin);

	match history_visibility {
		| HistoryVisibility::Invited => {
			// Allow if any member on requesting server was AT LEAST invited, else deny
			current_server_members
				.any(|member| self.user_was_invited(shortstatehash, member))
				.await
		},
		| HistoryVisibility::Joined => {
			// Allow if any member on requested server was joined, else deny
			current_server_members
				.any(|member| self.user_was_joined(shortstatehash, member))
				.await
		},
		| HistoryVisibility::WorldReadable | HistoryVisibility::Shared | _ => true,
	}
}
