use axum::extract::State;
use conduwuit::{Err, Result};
use ruma::{UInt, api::client::space::get_hierarchy, assign};
use service::rooms::summary::Accessibility;

use crate::Ruma;

const MAX_MAX_DEPTH: u32 = 10;

/// # `GET /_matrix/client/v1/rooms/{room_id}/hierarchy`
///
/// Paginates over the space tree in a depth-first manner to locate child rooms
/// of a given space.
pub(crate) async fn get_hierarchy_route(
	State(services): State<crate::State>,
	body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
	let max_depth = body
		.max_depth
		.map(|max_depth| max_depth.min(UInt::from(MAX_MAX_DEPTH)));

	let hierarchy = services
		.rooms
		.summary
		.get_room_hierarchy_for_user(
			body.sender_user(),
			body.room_id.clone(),
			max_depth,
			body.suggested_only,
		)
		.await?;

	match hierarchy {
		| Accessibility::Accessible(rooms) => {
			let limit = body
				.limit
				.map_or(100, u64::from)
				.min(1000)
				.try_into()
				.unwrap_or(usize::MAX);

			let from = body
				.from
				.as_ref()
				.and_then(|s| s.parse::<usize>().ok())
				.unwrap_or(0);

			let next_batch = if from.saturating_add(limit) < rooms.len() {
				Some(from.saturating_add(limit).to_string())
			} else {
				None
			};

			let rooms = rooms.into_iter().skip(from).take(limit).collect();

			Ok(assign!(get_hierarchy::v1::Response::new(), {
				rooms,
				next_batch,
			}))
		},
		| Accessibility::Inaccessible => {
			Err!(Request(Forbidden("You may not preview this room."), FORBIDDEN))
		},
		| Accessibility::NotFound => {
			Err!(Request(Forbidden("This room does not exist."), FORBIDDEN))
		},
	}
}
