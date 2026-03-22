use std::fmt::Write;

use conduwuit::{Err, Result};
use futures::StreamExt;
use ruma::{OwnedRoomId, OwnedServerName, OwnedUserId, api::client::discovery::discover_support};

use crate::{admin_command, get_room_info};

#[admin_command]
pub(super) async fn disable_room(&self, room_id: OwnedRoomId) -> Result {
	self.bail_restricted()?;
	self.services.rooms.metadata.disable_room(&room_id, true);
	self.write_str("Room disabled.").await
}

#[admin_command]
pub(super) async fn enable_room(&self, room_id: OwnedRoomId) -> Result {
	self.bail_restricted()?;
	self.services.rooms.metadata.disable_room(&room_id, false);
	self.write_str("Room enabled.").await
}

#[admin_command]
pub(super) async fn incoming_federation(&self) -> Result {
	let msg = {
		let map = self
			.services
			.rooms
			.event_handler
			.federation_handletime
			.read();

		let mut msg = format!(
			"Handling {} incoming PDUs across {} active transactions:\n",
			map.len(),
			self.services.transactions.txn_active_handle_count()
		);
		for (r, (e, i)) in map.iter() {
			let elapsed = i.elapsed();
			writeln!(msg, "{} {}: {}m{}s", r, e, elapsed.as_secs() / 60, elapsed.as_secs() % 60)?;
		}
		msg
	};

	self.write_str(&msg).await
}

#[admin_command]
pub(super) async fn fetch_support_well_known(&self, server_name: OwnedServerName) -> Result {
	let request = discover_support::Request {};
	let response = self
		.services
		.federation
		.execute_synapse(&server_name, request)
		.await?;
	// simple unwrap since this info got extracted from json so unless theres a bug
	// in the extractor this will always succeed
	let contacts = serde_json::to_string_pretty(&response.contacts).unwrap();
	let support_page = serde_json::to_string_pretty(&response.support_page).unwrap();
	self.write_str(&format!(
		"Got response:\n\n```\nContacts: {contacts}\nSupport Page: {support_page}\n```"
	))
	.await
}

#[admin_command]
pub(super) async fn remote_user_in_rooms(&self, user_id: OwnedUserId) -> Result {
	if user_id.server_name() == self.services.server.name {
		return Err!(
			"User belongs to our server, please use `list-joined-rooms` user admin command \
			 instead.",
		);
	}

	if !self.services.users.exists(&user_id).await {
		return Err!("Remote user does not exist in our database.",);
	}

	let mut rooms: Vec<(OwnedRoomId, u64, String)> = self
		.services
		.rooms
		.state_cache
		.rooms_joined(&user_id)
		.then(|room_id| get_room_info(self.services, room_id))
		.collect()
		.await;

	if rooms.is_empty() {
		return Err!("User is not in any rooms.");
	}

	rooms.sort_by_key(|r| r.1);
	rooms.reverse();

	let num = rooms.len();
	let body = rooms
		.iter()
		.map(|(id, members, name)| format!("{id} | Members: {members} | Name: {name}"))
		.collect::<Vec<_>>()
		.join("\n");

	self.write_str(&format!("Rooms {user_id} shares with us ({num}):\n```\n{body}\n```",))
		.await
}
