use conduwuit::{Err, Result};
use futures::StreamExt;
use ruma::{OwnedRoomId, OwnedUserId};

use crate::{PAGE_SIZE, admin_command, get_room_info};

#[allow(clippy::fn_params_excessive_bools)]
#[admin_command]
pub(super) async fn list_rooms(
	&self,
	page: Option<usize>,
	exclude_disabled: bool,
	exclude_banned: bool,
	include_empty: bool,
	no_details: bool,
) -> Result {
	// TODO: i know there's a way to do this with clap, but i can't seem to find it
	let page = page.unwrap_or(1);
	let mut rooms = self
		.services
		.rooms
		.metadata
		.iter_ids()
		.filter_map(|room_id| async move {
			(!exclude_disabled || !self.services.rooms.metadata.is_disabled(room_id).await)
				.then_some(room_id)
		})
		.filter_map(|room_id| async move {
			(!exclude_banned || !self.services.rooms.metadata.is_banned(room_id).await)
				.then_some(room_id)
		})
		.then(|room_id| get_room_info(self.services, room_id))
		.then(|(room_id, total_members, name)| async move {
			let local_members: Vec<_> = self
				.services
				.rooms
				.state_cache
				.active_local_users_in_room(&room_id)
				.collect()
				.await;
			let local_members = local_members.len();
			(room_id, total_members, local_members, name)
		})
		.filter_map(|(room_id, total_members, local_members, name)| async move {
			(include_empty || local_members > 0).then_some((room_id, total_members, name))
		})
		.collect::<Vec<_>>()
		.await;

	let total_rooms = rooms.len();
	rooms.sort_by_key(|r| r.1);
	rooms.reverse();

	let rooms = rooms
		.into_iter()
		.skip(page.saturating_sub(1).saturating_mul(PAGE_SIZE))
		.take(PAGE_SIZE)
		.collect::<Vec<_>>();

	if rooms.is_empty() {
		return Err!("No more rooms.");
	}

	let body = rooms
		.iter()
		.map(|(id, members, name)| {
			if no_details {
				format!("{id}")
			} else {
				format!("{id}\tMembers: {members}\tName: {name}")
			}
		})
		.collect::<Vec<_>>()
		.join("\n");

	self.write_str(&format!("Rooms (Total: {total_rooms}, Page {page}):\n```\n{body}\n```"))
		.await
}

#[admin_command]
pub(super) async fn exists(&self, room_id: OwnedRoomId) -> Result {
	let result = self.services.rooms.metadata.exists(&room_id).await;

	self.write_str(&format!("{result}")).await
}

#[admin_command]
pub(super) async fn bump(&self, room_id: OwnedRoomId) -> Result {
	self.bail_restricted()?;

	if !self
		.services
		.rooms
		.state_cache
		.server_in_room(&self.services.server.name, &room_id)
		.await
	{
		return Err!("We are not participating in the room / we don't know about the room ID.");
	}

	let state_lock = self.services.rooms.state.mutex.lock(&room_id).await;

	let pdu_builder = conduwuit::matrix::pdu::PduBuilder {
		event_type: "org.matrix.dummy_event".into(),
		content: serde_json::value::to_raw_value(&serde_json::json!({})).expect("valid json"),
		..Default::default()
	};

	// Use an active local member as sender — server_user has no membership
	// in rooms it didn't create, which causes M_FORBIDDEN on auth checks.
	let sender: OwnedUserId = self
		.services
		.rooms
		.state_cache
		.active_local_users_in_room(&room_id)
		.boxed()
		.next()
		.await
		.map(ToOwned::to_owned)
		.ok_or_else(|| conduwuit::err!("No local users in room {room_id} - cannot bump"))?;

	let event_id = self
		.services
		.rooms
		.timeline
		.build_and_append_pdu(pdu_builder, &sender, Some(&room_id), &state_lock)
		.await
		.map_err(|e| {
			conduwuit::err!(Database("Failed appending dummy event into room timeline: {e}"))
		})?;

	self.write_str(&format!("Successfully bumped room {room_id} with event {event_id}"))
		.await
}
