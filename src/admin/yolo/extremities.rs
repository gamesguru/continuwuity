use std::fmt::Write;

use conduwuit::{Result, matrix::Event};
use futures::StreamExt;
use ruma::{OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId};

use crate::admin_command;

#[admin_command]
pub(super) async fn clean_extremities(&self, room_id: OwnedRoomId) -> Result {
	let mutex = self.services.rooms.state.mutex.lock(&room_id).await;

	let state_hash = self
		.services
		.rooms
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	self.services
		.rooms
		.state
		.reset_extremities_to_state(&room_id, state_hash, &mutex)
		.await;

	self.write_str(&format!(
		"Pruned unused forward extremities and reset them to the current state for {room_id}.\n"
	))
	.await
}

#[admin_command]
pub(super) async fn view_extremities(
	&self,
	room: Option<OwnedRoomOrAliasId>,
	all: bool,
	verbose: bool,
) -> Result {
	if all || room.is_none() {
		let mut fractured = Vec::new();
		let rooms: Vec<_> = self
			.services
			.rooms
			.metadata
			.iter_ids()
			.map(ToOwned::to_owned)
			.collect()
			.await;

		for room_id in &rooms {
			let count = self
				.services
				.rooms
				.state
				.get_forward_extremities(room_id)
				.count()
				.await;
			if count > 1 {
				fractured.push((room_id.clone(), count));
			}
		}

		fractured.sort_by(|a, b| b.1.cmp(&a.1));

		if fractured.is_empty() {
			return self
				.write_str(&format!("All {} rooms have exactly 1 extremity. ✓", rooms.len()))
				.await;
		}

		let mut body = String::new();
		for (room_id, count) in &fractured {
			writeln!(body, "{room_id}\t{count} extremities")?;
			if verbose {
				let extremities: Vec<OwnedEventId> = self
					.services
					.rooms
					.state
					.get_forward_extremities(room_id)
					.map(ToOwned::to_owned)
					.collect()
					.await;
				for eid in &extremities {
					let detail = match self.services.rooms.timeline.get_pdu(eid).await {
						| Ok(pdu) => {
							let ts = pdu.origin_server_ts;
							let kind = pdu.kind.to_string();
							let sender = pdu.sender();
							format!("  {eid}  {kind}  {sender}  TS:{ts}")
						},
						| Err(_) => format!("  {eid}  (PDU not found in timeline)"),
					};
					writeln!(body, "{detail}")?;
				}
				writeln!(body)?;
			}
		}

		return self
			.write_str(&format!(
				"{} of {} rooms have multiple extremities:\n```\n{body}\n```",
				fractured.len(),
				rooms.len()
			))
			.await;
	}

	let room = room.expect("room required when not --all");
	let room_id = self.services.rooms.alias.resolve(&room).await?;
	let extremities: Vec<OwnedEventId> = self
		.services
		.rooms
		.state
		.get_forward_extremities(&room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let num = extremities.len();
	let mut body = String::new();
	for event_id in extremities {
		let pdu = self.services.rooms.timeline.get_pdu(&event_id).await;
		match pdu {
			| Ok(pdu) => {
				let ts = pdu.origin_server_ts;
				let sender = pdu.sender();
				writeln!(body, "{event_id}\tTS: {ts}\tSender: {sender}")?;
			},
			| Err(_) => {
				writeln!(body, "{event_id}\tERROR: PDU not found in timeline")?;
			},
		}
	}

	self.write_str(&format!("Room {room_id} has {num} extremities:\n```\n{body}\n```"))
		.await
}

#[admin_command]
pub(super) async fn recalculate_extremities(
	&self,
	room: OwnedRoomOrAliasId,
	tail: i64,
) -> Result {
	let room_id = self.services.rooms.alias.resolve(&room).await?;

	let actual_tail = if tail < 0 {
		usize::MAX
	} else {
		usize::try_from(tail).unwrap_or(usize::MAX)
	};

	let tail_str = if tail < 0 {
		"all".to_owned()
	} else {
		actual_tail.to_string()
	};

	self.write_str(&format!(
		"Recalculating forward extremities for room {room_id} using tail {tail_str}...\n"
	))
	.await?;

	let changed = self
		.services
		.rooms
		.timeline
		.recalculate_extremities(&room_id, actual_tail, true)
		.await?;

	if changed {
		self.write_str(
			"SUCCESS: DAG Extremities were silently broken and have now been recalculated and \
			 permanently healed!\n",
		)
		.await?;
	} else {
		self.write_str("DAG Extremities are already mathematically perfect. No changes made.\n")
			.await?;
	}

	Ok(())
}
