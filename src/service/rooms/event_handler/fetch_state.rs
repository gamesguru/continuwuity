use std::collections::{HashMap, hash_map};

use conduwuit::{Err, Event, Result, debug, debug_warn, err, implement, warn};
use futures::FutureExt;
use ruma::{
	EventId, OwnedEventId, RoomId, ServerName, api::federation::event::get_room_state_ids,
	events::StateEventType,
};

use crate::rooms::short::ShortStateKey;

/// Call /state_ids to find out what the state at this pdu is. We trust the
/// server's response to some extend (sic), but we still do a lot of checks
/// on the events
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(%origin),
)]
pub(super) async fn fetch_state<Pdu>(
	&self,
	origin: &ServerName,
	create_event: &Pdu,
	room_id: &RoomId,
	event_id: &EventId,
) -> Result<Option<HashMap<u64, OwnedEventId>>>
where
	Pdu: Event + Send + Sync,
{
	// Build the full fallback server list: origin → trusted → room members.
	// This mirrors fetch_and_handle_outliers so that when the origin is
	// unreachable (connection error, timeout, 404), we still get state from
	// another server that has the room.
	let servers = self
		.build_federation_server_list(
			room_id,
			origin,
			self.services.server.config.federation_fallback_room_servers,
		)
		.await;

	let mut last_err: conduwuit::Error =
		conduwuit::err!(Request(NotFound("No server could provide /state_ids")));
	let res = 'found: {
		for server in &servers {
			match self
				.services
				.sending
				.send_federation_request(server, get_room_state_ids::v1::Request {
					room_id: room_id.to_owned(),
					event_id: event_id.to_owned(),
				})
				.await
			{
				| Ok(res) => {
					if server != origin {
						debug!(%server, "fetch_state: used fallback server for /state_ids");
					}
					break 'found res;
				},
				| Err(e) => {
					debug_warn!(%server, "fetch_state /state_ids failed: {e}");
					last_err = e;
				},
			}
		}
		warn!(
			n_servers = servers.len(),
			"fetch_state: all servers failed /state_ids for {event_id}"
		);
		return Err(last_err);
	};

	debug!("Fetching state events");
	let state_ids = res.pdu_ids.iter().map(AsRef::as_ref);
	let state_vec = self
		.fetch_and_handle_outliers(origin, state_ids, create_event, room_id)
		.boxed()
		.await;

	let mut state: HashMap<ShortStateKey, OwnedEventId> = HashMap::with_capacity(state_vec.len());
	for (pdu, _) in state_vec {
		let state_key = pdu
			.state_key()
			.ok_or_else(|| err!(Database("Found non-state pdu in state events.")))?;

		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&pdu.kind().to_string().into(), state_key)
			.await;

		match state.entry(shortstatekey) {
			| hash_map::Entry::Vacant(v) => {
				v.insert(pdu.event_id().to_owned());
			},
			| hash_map::Entry::Occupied(_) => {
				return Err!(Database(
					"State event's type and state_key combination exists multiple times: {}, {}",
					pdu.kind(),
					state_key
				));
			},
		}
	}

	// The original create event must still be in the state
	let create_shortstatekey = self
		.services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await?;

	if state.get(&create_shortstatekey).map(AsRef::as_ref) != Some(create_event.event_id()) {
		return Err!(Database("Incoming event refers to wrong create event."));
	}

	Ok(Some(state))
}
