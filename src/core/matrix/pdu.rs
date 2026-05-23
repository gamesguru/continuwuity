mod builder;
mod count;
mod id;
mod raw_id;
mod redact;
#[cfg(test)]
mod tests;
mod unsigned;

use std::cmp::Ordering;

use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId,
	OwnedServerName, OwnedUserId, RoomId, UInt, UserId, events::TimelineEventType,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue as RawJsonValue;

pub use self::{
	Count as PduCount, Id as PduId, Pdu as PduEvent, RawId as RawPduId,
	builder::{Builder, Builder as PduBuilder},
	count::Count,
	id::{ShortId, *},
	raw_id::*,
};
use super::{Event, StateKey};
use crate::Result;

/// Persistent Data Unit (Event)
#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct Pdu {
	pub event_id: OwnedEventId,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub room_id: Option<OwnedRoomId>,

	pub sender: OwnedUserId,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub origin: Option<OwnedServerName>,

	pub origin_server_ts: UInt,

	#[serde(rename = "type")]
	pub kind: TimelineEventType,

	pub content: Box<RawJsonValue>,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub state_key: Option<StateKey>,

	pub prev_events: Vec<OwnedEventId>,

	pub depth: UInt,

	pub auth_events: Vec<OwnedEventId>,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub redacts: Option<OwnedEventId>,

	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub unsigned: Option<Box<RawJsonValue>>,

	pub hashes: EventHash,

	// BTreeMap<Box<ServerName>, BTreeMap<ServerSigningKeyId, String>>
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub signatures: Option<Box<RawJsonValue>>,

	/// Whether this event has been rejected (by auth check, soft-fail, or
	/// admin action). Populated at fetch time from pdu_metadata DB;
	/// not persisted in the event JSON itself.
	#[serde(skip)]
	pub rejected: bool,
}

/// Content hashes of a PDU.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventHash {
	/// The SHA-256 hash.
	pub sha256: String,
}

impl Pdu {
	pub fn from_id_val(
		event_id: &EventId,
		mut json: CanonicalJsonObject,
		room_id: Option<&RoomId>,
	) -> Result<Self> {
		json.insert(
			"event_id".into(),
			ruma::CanonicalJsonValue::String(event_id.as_str().to_owned()),
		);
		let mut pdu: Self = serde_json::from_value(serde_json::to_value(json)?)?;
		pdu.event_id = event_id.to_owned();

		if pdu.room_id.is_none() {
			if pdu.kind == TimelineEventType::RoomCreate {
				// V12+: room_id is omitted from the signed content. Derive it
				// deterministically from the event_id hash ($ -> !) to prevent
				// a malicious server from spoofing creator privileges.
				let constructed_hash = event_id.as_str().replacen('$', "!", 1);
				let constructed_room_id = RoomId::parse(&constructed_hash).map_err(|_| {
					crate::err!(Request(InvalidParam(
						"Invalid event_id for room hash derivation"
					)))
				})?;
				pdu.room_id = Some(constructed_room_id.into());
			} else if let Some(room_id) = room_id {
				pdu.room_id = Some(room_id.to_owned());
			} else {
				return Err(crate::err!(Request(InvalidParam("Event is missing room_id"))));
			}
		}

		// Validate the PDU belongs to the expected room if one is specified
		if let Some(expected_room) = room_id {
			if pdu.room_id_or_hash().as_deref() != Some(expected_room) {
				return Err(crate::err!(Request(InvalidParam(
					"PDU {event_id} does not belong to room {expected_room}"
				))));
			}
		}

		Ok(pdu)
	}
}

impl Event for Pdu {
	#[inline]
	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.auth_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn content(&self) -> &RawJsonValue { &self.content }

	#[inline]
	fn event_id(&self) -> &EventId { &self.event_id }

	#[inline]
	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
		MilliSecondsSinceUnixEpoch(self.origin_server_ts)
	}

	#[inline]
	fn depth(&self) -> ruma::UInt { self.depth }

	#[inline]
	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.prev_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn redacts(&self) -> Option<&EventId> { self.redacts.as_deref() }

	#[inline]
	fn room_id(&self) -> Option<&RoomId> { self.room_id.as_deref() }

	#[inline]
	fn room_id_or_hash(&self) -> Option<OwnedRoomId> {
		if let Some(room_id) = &self.room_id {
			return Some(room_id.clone());
		}
		if *self.event_type() == TimelineEventType::RoomCreate {
			let constructed_hash = self.event_id.as_str().replace('$', "!");
			return RoomId::parse(&constructed_hash).ok().map(ToOwned::to_owned);
		}
		None
	}

	#[inline]
	fn sender(&self) -> &UserId { &self.sender }

	#[inline]
	fn state_key(&self) -> Option<&str> { self.state_key.as_deref() }

	#[inline]
	fn kind(&self) -> &TimelineEventType { &self.kind }

	#[inline]
	fn unsigned(&self) -> Option<&RawJsonValue> { self.unsigned.as_deref() }

	#[inline]
	fn rejected(&self) -> bool { self.rejected }

	#[inline]
	fn as_mut_pdu(&mut self) -> &mut Pdu { self }

	#[inline]
	fn as_pdu(&self) -> &Pdu { self }

	#[inline]
	fn into_pdu(self) -> Pdu { self }

	#[inline]
	fn is_owned(&self) -> bool { true }
}

impl Event for &Pdu {
	#[inline]
	fn auth_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.auth_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn content(&self) -> &RawJsonValue { &self.content }

	#[inline]
	fn event_id(&self) -> &EventId { &self.event_id }

	#[inline]
	fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
		MilliSecondsSinceUnixEpoch(self.origin_server_ts)
	}

	#[inline]
	fn depth(&self) -> ruma::UInt { self.depth }

	#[inline]
	fn prev_events(&self) -> impl DoubleEndedIterator<Item = &EventId> + Clone + Send + '_ {
		self.prev_events.iter().map(AsRef::as_ref)
	}

	#[inline]
	fn redacts(&self) -> Option<&EventId> { self.redacts.as_deref() }

	#[inline]
	fn room_id(&self) -> Option<&RoomId> { self.room_id.as_ref().map(AsRef::as_ref) }

	#[inline]
	fn room_id_or_hash(&self) -> Option<OwnedRoomId> {
		if let Some(room_id) = &self.room_id {
			return Some(room_id.clone());
		}
		if *self.event_type() == TimelineEventType::RoomCreate {
			let constructed_hash = self.event_id.as_str().replace('$', "!");
			return RoomId::parse(&constructed_hash).ok().map(ToOwned::to_owned);
		}
		None
	}

	#[inline]
	fn sender(&self) -> &UserId { &self.sender }

	#[inline]
	fn state_key(&self) -> Option<&str> { self.state_key.as_deref() }

	#[inline]
	fn kind(&self) -> &TimelineEventType { &self.kind }

	#[inline]
	fn unsigned(&self) -> Option<&RawJsonValue> { self.unsigned.as_deref() }

	#[inline]
	fn rejected(&self) -> bool { self.rejected }

	#[inline]
	fn as_pdu(&self) -> &Pdu { self }

	#[inline]
	fn into_pdu(self) -> Pdu { self.clone() }

	#[inline]
	fn is_owned(&self) -> bool { false }
}

/// Prevent derived equality which wouldn't limit itself to event_id
impl Eq for Pdu {}

/// Equality determined by the Pdu's ID, not the memory representations.
impl PartialEq for Pdu {
	fn eq(&self, other: &Self) -> bool { self.event_id == other.event_id }
}

/// Ordering determined by the Pdu's ID, not the memory representations.
impl Ord for Pdu {
	fn cmp(&self, other: &Self) -> Ordering { self.event_id.cmp(&other.event_id) }
}

/// Ordering determined by the Pdu's ID, not the memory representations.
impl PartialOrd for Pdu {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
