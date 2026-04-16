use conduwuit::{Err, Result, implement};
use conduwuit_service::Services;
use ruma::{EventId, RoomId, ServerName};

pub(super) struct AccessCheck<'a> {
	pub(super) services: &'a Services,
	pub(super) origin: &'a ServerName,
	pub(super) room_id: &'a RoomId,
	pub(super) event_id: Option<&'a EventId>,
}

#[implement(AccessCheck, params = "<'_>")]
pub(super) async fn check(&self) -> Result {
	let acl_check = self
		.services
		.rooms
		.event_handler
		.acl_check(self.origin, self.room_id)
		.await;

	if acl_check.is_err() {
		return Err!(Request(Forbidden(warn!(
			%self.origin,
			%self.room_id,
			"Server access denied by ACL."
		))));
	}

	let world_readable = self
		.services
		.rooms
		.state_accessor
		.is_world_readable(self.room_id)
		.await;

	if world_readable {
		return Ok(());
	}

	let server_is_participant = self
		.services
		.rooms
		.state_cache
		.server_is_participant(self.origin, self.room_id)
		.await;

	if !server_is_participant {
		return Err!(Request(Forbidden(warn!(
			%self.origin,
			%self.room_id,
			"Server is not participating in room and room is not world-readable."
		))));
	}

	if let Some(event_id) = self.event_id {
		let can_see = self
			.services
			.rooms
			.state_accessor
			.server_can_see_event(self.origin, self.room_id, event_id)
			.await;

		if !can_see {
			return Err!(Request(Forbidden(warn!(
				%self.origin,
				%self.room_id,
				?self.event_id,
				"Server is not allowed to see event."
			))));
		}
	}

	Ok(())
}
