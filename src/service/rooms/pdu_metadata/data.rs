use std::{mem::size_of, sync::Arc};

use conduwuit::{
	arrayvec::ArrayVec,
	matrix::{Event, PduCount},
	utils::{
		ReadyExt,
		stream::{TryIgnore, WidebandExt},
		u64_from_u8,
	},
};
use database::Map;
use futures::{Stream, StreamExt};
use ruma::{EventId, RoomId, UserId, api::Direction};

use crate::{
	Dep,
	rooms::{
		self,
		short::{ShortEventId, ShortRoomId},
		timeline::{PduId, PdusIterItem, RawPduId},
	},
};

pub(super) struct Data {
	tofrom_relation: Arc<Map>,
	referencedevents: Arc<Map>,
	eventid_metadata: Arc<Map>,
	services: Services,
}

struct Services {
	timeline: Dep<rooms::timeline::Service>,
}

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			tofrom_relation: db["tofrom_relation"].clone(),
			referencedevents: db["referencedevents"].clone(),
			eventid_metadata: db["eventid_metadata"].clone(),
			services: Services {
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
			},
		}
	}

	pub(super) fn add_relation(&self, from: u64, to: u64) {
		const BUFSIZE: usize = size_of::<u64>() * 2;

		let key: &[u64] = &[to, from];
		self.tofrom_relation.aput_raw::<BUFSIZE, _, _>(key, []);
	}

	pub(super) fn get_relations<'a>(
		&'a self,
		user_id: &'a UserId,
		shortroomid: ShortRoomId,
		target: ShortEventId,
		from: PduCount,
		dir: Direction,
	) -> impl Stream<Item = PdusIterItem> + Send + 'a {
		// Query from exact position then filter excludes it (saturating_inc could skip
		// events at min/max boundaries).
		//
		// Relations currently only index normal timeline counts. `PduCount::min()`
		// is a backfilled sentinel whose unsigned encoding lands far beyond any
		// normal count, which would make forward pagination from the beginning
		// return an empty stream.
		let from_unsigned = match (dir, from) {
			| (Direction::Forward, PduCount::Backfilled(_)) => 0,
			| _ => from.into_unsigned(),
		};
		let mut current = ArrayVec::<u8, 16>::new();
		current.extend(target.to_be_bytes());
		current.extend(from_unsigned.to_be_bytes());
		let current = current.as_slice();
		match dir {
			| Direction::Forward => self.tofrom_relation.raw_keys_from(current).boxed(),
			| Direction::Backward => self.tofrom_relation.rev_raw_keys_from(current).boxed(),
		}
		.ignore_err()
		.ready_take_while(move |key| key.starts_with(&target.to_be_bytes()))
		.map(|to_from| u64_from_u8(&to_from[8..16]))
		.map(PduCount::from_unsigned)
		.ready_filter(move |count| {
			if from == PduCount::min() || from == PduCount::max() {
				true
			} else {
				let count_unsigned = count.into_unsigned();
				match dir {
					| Direction::Forward => count_unsigned > from_unsigned,
					| Direction::Backward => count_unsigned < from_unsigned,
				}
			}
		})
		.wide_filter_map(move |shorteventid| async move {
			let pdu_id: RawPduId = PduId { shortroomid, shorteventid }.into();

			let mut pdu = self.services.timeline.get_pdu_from_id(&pdu_id).await.ok()?;

			pdu.as_mut_pdu().set_unsigned(Some(user_id));

			Some((shorteventid, pdu))
		})
	}

	#[inline]
	pub(super) fn mark_as_referenced<'a, I>(&self, room_id: &RoomId, event_ids: I)
	where
		I: Iterator<Item = &'a EventId>,
	{
		for prev in event_ids {
			let key = (room_id, prev);
			self.referencedevents.put_raw(key, []);
		}
	}

	pub(super) async fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> bool {
		let key = (room_id, event_id);
		self.referencedevents.qry(&key).await.is_ok()
	}

	pub(super) fn mark_event_soft_failed(&self, event_id: &EventId, reason: &str) {
		let mut meta = if let Ok(metadata_bytes) = self.eventid_metadata.get_blocking(event_id) {
			rooms::timeline::EventMetadata::from_bincode(&metadata_bytes).unwrap_or_default()
		} else {
			// New metadata: events reaching this path without existing metadata
			// are always outliers (not yet in the timeline).
			rooms::timeline::EventMetadata { is_outlier: true, ..Default::default() }
		};

		if !meta.soft_failed || meta.soft_fail_reason.is_empty() {
			meta.soft_failed = true;
			reason.clone_into(&mut meta.soft_fail_reason);
			if let Ok(new_bytes) = bincode::serialize(&meta) {
				self.eventid_metadata.insert(event_id, new_bytes);
			}
		}
	}

	pub(super) async fn is_event_soft_failed(&self, event_id: &EventId) -> bool {
		if let Ok(metadata_bytes) = self.eventid_metadata.get(event_id).await {
			if let Ok(meta) = rooms::timeline::EventMetadata::from_bincode(&metadata_bytes) {
				return meta.soft_failed;
			}
		}
		false
	}

	pub(super) async fn get_soft_fail_reason(&self, event_id: &EventId) -> Option<String> {
		let metadata_bytes = self.eventid_metadata.get(event_id).await.ok()?;
		let meta = rooms::timeline::EventMetadata::from_bincode(&metadata_bytes).ok()?;
		if meta.soft_failed && !meta.soft_fail_reason.is_empty() {
			Some(meta.soft_fail_reason)
		} else {
			None
		}
	}

	pub(super) fn unmark_event_soft_failed(&self, event_id: &EventId) {
		if let Ok(metadata_bytes) = self.eventid_metadata.get_blocking(event_id) {
			if let Ok(mut meta) = rooms::timeline::EventMetadata::from_bincode(&metadata_bytes) {
				if meta.soft_failed {
					meta.soft_failed = false;
					if let Ok(new_bytes) = bincode::serialize(&meta) {
						self.eventid_metadata.insert(event_id, new_bytes);
					}
				}
			}
		}
	}

	pub(super) fn mark_event_rejected(&self, event_id: &EventId, reason: &str) {
		let mut meta = if let Ok(metadata_bytes) = self.eventid_metadata.get_blocking(event_id) {
			rooms::timeline::EventMetadata::from_bincode(&metadata_bytes).unwrap_or_default()
		} else {
			// New metadata: events reaching this path without existing metadata
			// are always outliers (not yet in the timeline).
			rooms::timeline::EventMetadata { is_outlier: true, ..Default::default() }
		};

		if !meta.rejected || meta.rejection_reason.is_empty() {
			meta.rejected = true;
			reason.clone_into(&mut meta.rejection_reason);
			if let Ok(new_bytes) = bincode::serialize(&meta) {
				self.eventid_metadata.insert(event_id, new_bytes);
			}
		}
	}

	pub(super) async fn get_rejection_reason(&self, event_id: &EventId) -> Option<String> {
		let metadata_bytes = self.eventid_metadata.get(event_id).await.ok()?;
		let meta = rooms::timeline::EventMetadata::from_bincode(&metadata_bytes).ok()?;
		if meta.rejected && !meta.rejection_reason.is_empty() {
			Some(meta.rejection_reason)
		} else {
			None
		}
	}

	pub(super) async fn is_event_rejected(&self, event_id: &EventId) -> bool {
		if let Ok(metadata_bytes) = self.eventid_metadata.get(event_id).await {
			if let Ok(meta) = rooms::timeline::EventMetadata::from_bincode(&metadata_bytes) {
				return meta.rejected;
			}
		}
		false
	}

	pub(super) fn unmark_event_rejected(&self, event_id: &EventId) {
		if let Ok(metadata_bytes) = self.eventid_metadata.get_blocking(event_id) {
			if let Ok(mut meta) = rooms::timeline::EventMetadata::from_bincode(&metadata_bytes) {
				if meta.rejected {
					meta.rejected = false;
					meta.rejection_reason.clear();
					if let Ok(new_bytes) = bincode::serialize(&meta) {
						self.eventid_metadata.insert(event_id, new_bytes);
					}
				}
			}
		}
	}

	/// Removes any soft-fail or rejection markers applied to the target PDU
	pub(super) fn clear_pdu_markers(&self, event_id: &EventId) {
		self.unmark_event_rejected(event_id);
		self.unmark_event_soft_failed(event_id);
	}
}
