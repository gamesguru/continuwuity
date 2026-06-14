use arrayvec::ArrayVec;

use super::{Count, Id, ShortEventId, ShortId, ShortRoomId};

// TODO: RawId has two byte layouts — Normal is 16 bytes [room(8) | count(8)],
// Backfilled is 24 bytes [room(8) | 0x00_tag(8) | count(8)]. NEVER use
// as_ref()[8..] to extract the count; it yields zeros for Backfilled. Always
// use shortroomid() and shorteventid() which handle both variants correctly.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RawId {
	Normal(RawIdNormal),
	Backfilled(RawIdBackfilled),
}

type RawIdNormal = [u8; RawId::NORMAL_LEN];
type RawIdBackfilled = [u8; RawId::BACKFILLED_LEN];

const INT_LEN: usize = size_of::<ShortId>();

impl RawId {
	const BACKFILLED_LEN: usize = size_of::<ShortRoomId>() + INT_LEN + size_of::<ShortEventId>();
	const MAX_LEN: usize = Self::BACKFILLED_LEN;
	const NORMAL_LEN: usize = size_of::<ShortRoomId>() + size_of::<ShortEventId>();

	#[inline]
	#[must_use]
	pub fn pdu_count(&self) -> Count {
		let id: Id = (*self).into();
		id.shorteventid
	}

	#[inline]
	#[must_use]
	pub fn shortroomid(self) -> [u8; INT_LEN] {
		match self {
			| Self::Normal(raw) => raw[0..INT_LEN]
				.try_into()
				.expect("normal raw shortroomid array from slice"),
			| Self::Backfilled(raw) => raw[0..INT_LEN]
				.try_into()
				.expect("backfilled raw shortroomid array from slice"),
		}
	}

	#[inline]
	#[must_use]
	pub fn shorteventid(self) -> [u8; INT_LEN] {
		match self {
			| Self::Normal(raw) => raw[INT_LEN..INT_LEN * 2]
				.try_into()
				.expect("normal raw shorteventid array from slice"),
			| Self::Backfilled(raw) => raw[INT_LEN * 2..INT_LEN * 3]
				.try_into()
				.expect("backfilled raw shorteventid array from slice"),
		}
	}

	/// Returns a canonical 16-byte key [shortroomid(8) | shorteventid(8)]
	/// that is safe for both Normal and Backfilled variants. Use this instead
	/// of as_ref() slicing when you need a uniform pdu_id representation.
	#[inline]
	#[must_use]
	pub fn to_short_key(self) -> [u8; 16] {
		let mut key = [0_u8; 16];
		key[..8].copy_from_slice(&self.shortroomid());
		key[8..].copy_from_slice(&self.shorteventid());
		key
	}

	#[deprecated = "use shortroomid(), shorteventid(), or to_short_key() -- as_bytes() returns \
	                different lengths for Normal (16) vs Backfilled (24) and raw slicing will \
	                silently yield wrong bytes for Backfilled variants"]
	#[allow(clippy::must_use_candidate)]
	#[inline]
	pub fn as_bytes(&self) -> &[u8] {
		match self {
			| Self::Normal(raw) => raw,
			| Self::Backfilled(raw) => raw,
		}
	}
}

impl AsRef<[u8]> for RawId {
	#[inline]
	fn as_ref(&self) -> &[u8] {
		match self {
			| Self::Normal(raw) => raw,
			| Self::Backfilled(raw) => raw,
		}
	}
}

impl From<&[u8]> for RawId {
	#[inline]
	fn from(id: &[u8]) -> Self {
		match id.len() {
			| Self::NORMAL_LEN => Self::Normal(
				id[0..Self::NORMAL_LEN]
					.try_into()
					.expect("normal RawId from [u8]"),
			),
			| Self::BACKFILLED_LEN => Self::Backfilled(
				id[0..Self::BACKFILLED_LEN]
					.try_into()
					.expect("backfilled RawId from [u8]"),
			),
			| _ => unimplemented!("unrecognized RawId length"),
		}
	}
}

impl From<Id> for RawId {
	#[inline]
	fn from(id: Id) -> Self {
		const MAX_LEN: usize = RawId::MAX_LEN;
		type RawVec = ArrayVec<u8, MAX_LEN>;

		let mut vec = RawVec::new();
		vec.extend(id.shortroomid.to_be_bytes());
		id.shorteventid.debug_assert_valid();
		match id.shorteventid {
			| Count::Normal(shorteventid) => {
				vec.extend(shorteventid.to_be_bytes());
				Self::Normal(vec.as_ref().try_into().expect("RawVec into RawId::Normal"))
			},
			| Count::Backfilled(shorteventid) => {
				// Zero-tag ensures backfilled keys sort before all Normal keys
				// in RocksDB byte ordering. This makes the raw byte layout 24
				// bytes instead of 16 — as_ref()[8..] will NOT give the count.
				vec.extend(0_u64.to_be_bytes());
				vec.extend(shorteventid.to_be_bytes());
				Self::Backfilled(
					vec.as_ref()
						.try_into()
						.expect("RawVec into RawId::Backfilled"),
				)
			},
		}
	}
}
