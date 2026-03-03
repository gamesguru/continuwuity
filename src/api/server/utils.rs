use conduwuit::{Err, Result, implement, is_false};
use conduwuit_service::Services;
use futures::{FutureExt, future::OptionFuture, join};
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
		.map(|result| result.is_ok());

	let world_readable = self
		.services
		.rooms
		.state_accessor
		.is_world_readable(self.room_id);

	let server_in_room = self
		.services
		.rooms
		.state_cache
		.server_in_room(self.origin, self.room_id);

	let server_can_see: OptionFuture<_> = self
		.event_id
		.map(|event_id| {
			self.services.rooms.state_accessor.server_can_see_event(
				self.origin,
				self.room_id,
				event_id,
			)
		})
		.into();

	let (world_readable, server_in_room, server_can_see, acl_check) =
		join!(world_readable, server_in_room, server_can_see, acl_check);

	if !acl_check {
		return Err!(Request(Forbidden(warn!(
			%self.origin,
			%self.room_id,
			"Server access denied by ACL."
		))));
	}

	if !world_readable && !server_in_room {
		return Err!(Request(Forbidden(warn!(
			%self.origin,
			%self.room_id,
			"Server is not in room and room is not world-readable."
		))));
	}

	if server_can_see.is_some_and(is_false!()) {
		let event_id = self.event_id.expect("server_can_see implies event_id");
		let event_type = self
			.services
			.rooms
			.timeline
			.get_pdu(event_id)
			.await
			.ok()
			.map(|pdu| pdu.kind.to_string());

		return Err!(Request(Forbidden(info!(
			%self.origin,
			%self.room_id,
			%event_id,
			?event_type,
			"Server is not allowed to see event."
		))));
	}

	Ok(())
}
