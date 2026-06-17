use std::collections::BTreeMap;

use axum::extract::State;
use axum_client_ip::ClientIp;
use conduwuit::{Err, PduCount, Result};
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::{read_marker::set_read_marker, receipt::create_receipt},
	events::{
		RoomAccountDataEventType,
		receipt::{ReceiptThread, ReceiptType},
	},
};

use crate::Ruma;

/// # `POST /_matrix/client/r0/rooms/{roomId}/read_markers`
///
/// Sets different types of read markers.
///
/// - Updates fully-read account data event to `fully_read`
/// - If `read_receipt` is set: Update private marker and public read receipt
///   EDU
pub(crate) async fn set_read_marker_route(
	State(services): State<crate::State>,
	body: Ruma<set_read_marker::v3::Request>,
) -> Result<set_read_marker::v3::Response> {
	let sender_user = body.sender_user();

	if let Some(event) = &body.fully_read {
		let fully_read_event = ruma::events::fully_read::FullyReadEvent {
			content: ruma::events::fully_read::FullyReadEventContent { event_id: event.clone() },
		};

		services
			.account_data
			.update(
				Some(&body.room_id),
				sender_user,
				RoomAccountDataEventType::FullyRead,
				&serde_json::to_value(fully_read_event)?,
			)
			.await?;
	}

	// ping presence
	if services.config.allow_local_presence {
		services
			.presence
			.ping_presence(sender_user, &ruma::presence::PresenceState::Online)
			.await?;
	}

	if let Some(event) = &body.read_receipt {
		if services.config.allow_local_read_receipts
			&& !services.users.is_suspended(sender_user).await?
		{
			// Spec: server SHOULD NOT allow read receipts to move backwards
			let new_count = services
				.rooms
				.timeline
				.get_pdu_count(event)
				.await
				.map_err(|_| conduwuit::err!(Request(NotFound("Event not found."))))?;

			let mut ignore_receipt = false;
			if let PduCount::Normal(new_count) = new_count {
				if let Some(old_event_id) = services
					.rooms
					.read_receipt
					.readreceipt_get(&body.room_id, sender_user, Some(&ReceiptThread::Unthreaded))
					.await
				{
					if let Ok(PduCount::Normal(old_count)) =
						services.rooms.timeline.get_pdu_count(&old_event_id).await
					{
						if new_count <= old_count {
							conduwuit::info!(
								target: "read_receipt_debug",
								"Ignoring read receipt for {} from {} because it moves backwards from {} to {}",
								&body.room_id, sender_user, old_count, new_count
							);
							ignore_receipt = true;
						}
					}
				}
			} else {
				conduwuit::info!(
					target: "read_receipt_debug",
					"Event {} not found in timeline, ignoring read receipt", event
				);
			}

			if !ignore_receipt {
				let receipt_content = BTreeMap::from_iter([(
					event.to_owned(),
					BTreeMap::from_iter([(
						ReceiptType::Read,
						BTreeMap::from_iter([(
							sender_user.to_owned(),
							ruma::events::receipt::Receipt {
								ts: Some(MilliSecondsSinceUnixEpoch::now()),
								thread: ReceiptThread::Unthreaded,
							},
						)]),
					)]),
				)]);

				services
					.rooms
					.read_receipt
					.readreceipt_update(
						sender_user,
						&body.room_id,
						&ruma::events::receipt::ReceiptEvent {
							content: ruma::events::receipt::ReceiptEventContent(receipt_content),
							room_id: body.room_id.clone(),
						},
					)
					.await;

				services
					.rooms
					.user
					.reset_notification_counts(sender_user, &body.room_id);

				conduwuit::info!(
					target: "read_receipt_debug",
					"Accepted read receipt for {} from {}", event, sender_user
				);
			}
		}
	}

	if let Some(event) = &body.private_read_receipt {
		let count = services
			.rooms
			.timeline
			.get_pdu_count(event)
			.await
			.map_err(|_| conduwuit::err!(Request(NotFound("Event not found."))))?;

		let PduCount::Normal(new_count) = count else {
			return Err!(Request(InvalidParam(
				"Event is a backfilled PDU and cannot be marked as read."
			)));
		};
		// Don't allow private receipt to move backwards
		let old_count = services
			.rooms
			.read_receipt
			.private_read_get_count(&body.room_id, sender_user)
			.await
			.unwrap_or(0);

		if new_count > old_count {
			let receipt_content = BTreeMap::from_iter([(
				event.to_owned(),
				BTreeMap::from_iter([(
					ReceiptType::ReadPrivate,
					BTreeMap::from_iter([(
						sender_user.to_owned(),
						ruma::events::receipt::Receipt {
							ts: Some(MilliSecondsSinceUnixEpoch::now()),
							thread: ReceiptThread::Unthreaded,
						},
					)]),
				)]),
			)]);

			let receipt_event = ruma::events::receipt::ReceiptEvent {
				content: ruma::events::receipt::ReceiptEventContent(receipt_content),
				room_id: body.room_id.clone(),
			};

			services.rooms.read_receipt.private_read_set(
				&body.room_id,
				sender_user,
				new_count,
				&receipt_event,
			)?;

			services
				.rooms
				.user
				.reset_notification_counts(sender_user, &body.room_id);

			conduwuit::debug!("Accepted private read receipt for {} from {}", event, sender_user);
		} else {
			conduwuit::info!(
				target: "read_receipt_debug",
				"Ignoring private read receipt for {} from {} because it moves backwards \
				 from {} to {}",
				&body.room_id,
				sender_user,
				old_count,
				new_count
			);
		}
	}

	Ok(set_read_marker::v3::Response {})
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/receipt/{receiptType}/{eventId}`
///
/// Sets private read marker and public read receipt EDU.
pub(crate) async fn create_receipt_route(
	State(services): State<crate::State>,
	ClientIp(client_ip): ClientIp,
	body: Ruma<create_receipt::v3::Request>,
) -> Result<create_receipt::v3::Response> {
	let sender_user = body.sender_user();
	services
		.users
		.update_device_last_seen(sender_user, body.sender_device.as_deref(), client_ip)
		.await;

	// ping presence
	if services.config.allow_local_presence {
		services
			.presence
			.ping_presence(sender_user, &ruma::presence::PresenceState::Online)
			.await?;
	}

	match body.receipt_type {
		| create_receipt::v3::ReceiptType::FullyRead => {
			let fully_read_event = ruma::events::fully_read::FullyReadEvent {
				content: ruma::events::fully_read::FullyReadEventContent {
					event_id: body.event_id.clone(),
				},
			};
			services
				.account_data
				.update(
					Some(&body.room_id),
					sender_user,
					RoomAccountDataEventType::FullyRead,
					&serde_json::to_value(fully_read_event)?,
				)
				.await?;
		},
		| create_receipt::v3::ReceiptType::Read => {
			// Spec: server SHOULD NOT allow read receipts to move backwards
			let new_count = services
				.rooms
				.timeline
				.get_pdu_count(&body.event_id)
				.await
				.map_err(|_| conduwuit::err!(Request(NotFound("Event not found."))))?;

			let mut ignore_receipt = false;
			if let PduCount::Normal(new_count) = new_count {
				if let Some(old_event_id) = services
					.rooms
					.read_receipt
					.readreceipt_get(&body.room_id, sender_user, Some(&body.body.thread))
					.await
				{
					if let Ok(PduCount::Normal(old_count)) =
						services.rooms.timeline.get_pdu_count(&old_event_id).await
					{
						if new_count <= old_count {
							conduwuit::info!(
								target: "read_receipt_debug",
								"Ignoring read receipt for {} from {} because it moves \
								 backwards from {} to {}",
								&body.room_id,
								sender_user,
								old_count,
								new_count
							);
							ignore_receipt = true;
						}
					}
				}
			} else {
				conduwuit::debug!(
					"Event {} not found in timeline, ignoring read receipt",
					&body.event_id
				);
			}

			if !ignore_receipt {
				let receipt_content = BTreeMap::from_iter([(
					body.event_id.clone(),
					BTreeMap::from_iter([(
						ReceiptType::Read,
						BTreeMap::from_iter([(
							sender_user.to_owned(),
							ruma::events::receipt::Receipt {
								ts: Some(MilliSecondsSinceUnixEpoch::now()),
								thread: body.body.thread.clone(),
							},
						)]),
					)]),
				)]);

				services
					.rooms
					.read_receipt
					.readreceipt_update(
						sender_user,
						&body.room_id,
						&ruma::events::receipt::ReceiptEvent {
							content: ruma::events::receipt::ReceiptEventContent(receipt_content),
							room_id: body.room_id.clone(),
						},
					)
					.await;

				services
					.rooms
					.user
					.reset_notification_counts(sender_user, &body.room_id);

				conduwuit::debug!(
					"Accepted read receipt for {} from {}",
					&body.event_id,
					sender_user
				);
			}
		},
		| create_receipt::v3::ReceiptType::ReadPrivate => {
			let count = services
				.rooms
				.timeline
				.get_pdu_count(&body.event_id)
				.await
				.map_err(|_| conduwuit::err!(Request(NotFound("Event not found."))))?;

			let PduCount::Normal(new_count) = count else {
				return Err!(Request(InvalidParam(
					"Event is a backfilled PDU and cannot be marked as read."
				)));
			};
			// Don't allow private receipt to move backwards
			let old_count = services
				.rooms
				.read_receipt
				.private_read_get_count(&body.room_id, sender_user)
				.await
				.unwrap_or(0);

			if new_count > old_count {
				let receipt_content = BTreeMap::from_iter([(
					body.event_id.clone(),
					BTreeMap::from_iter([(
						ReceiptType::ReadPrivate,
						BTreeMap::from_iter([(
							sender_user.to_owned(),
							ruma::events::receipt::Receipt {
								ts: Some(MilliSecondsSinceUnixEpoch::now()),
								thread: body.body.thread.clone(),
							},
						)]),
					)]),
				)]);

				let receipt_event = ruma::events::receipt::ReceiptEvent {
					content: ruma::events::receipt::ReceiptEventContent(receipt_content),
					room_id: body.room_id.clone(),
				};

				services.rooms.read_receipt.private_read_set(
					&body.room_id,
					sender_user,
					new_count,
					&receipt_event,
				)?;

				services
					.rooms
					.user
					.reset_notification_counts(sender_user, &body.room_id);

				conduwuit::debug!(
					"Accepted private read receipt for {} from {}",
					&body.event_id,
					sender_user
				);
			} else {
				conduwuit::info!(
					target: "read_receipt_debug",
					"Ignoring private read receipt for {} from {} because it moves \
					 backwards from {} to {}",
					&body.room_id,
					sender_user,
					old_count,
					new_count
				);
			}
		},
		| _ => {
			return Err!(Request(InvalidParam(warn!(
				"Received unknown read receipt type: {}",
				&body.receipt_type
			))));
		},
	}

	Ok(create_receipt::v3::Response {})
}
