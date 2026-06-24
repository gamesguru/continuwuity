use super::Count;

#[test]
fn backfilled_parse() {
	let count: Count = "-987654".parse().expect("parse() failed");
	let backfilled = matches!(count, Count::Backfilled(_));

	assert!(backfilled, "not backfilled variant");
}

#[test]
fn saturating_inc_backward() {
	use ruma::api::Direction;

	// Normal count
	let count = Count::Normal(10);
	let next = count.saturating_inc(Direction::Backward);
	assert_eq!(next, Count::Normal(9));

	// Transition to backfilled
	let count = Count::Normal(1);
	let next = count.saturating_inc(Direction::Backward);
	assert_eq!(next, Count::Normal(0));

	let count = Count::Normal(0);
	let next = count.saturating_inc(Direction::Backward);
	assert_eq!(next, Count::Backfilled(-1));

	// Minimum
	let count = Count::min();
	let next = count.saturating_inc(Direction::Backward);
	assert_eq!(next, Count::min());
}

#[test]
fn raw_id_normal_shorteventid_matches_bytes() {
	use super::{Id, RawId};

	let id = Id {
		shortroomid: 42,
		shorteventid: Count::Normal(12345),
	};
	let raw: RawId = id.into();

	// shorteventid() must return the count bytes
	assert_eq!(raw.shorteventid(), 12345_u64.to_be_bytes());

	// For Normal, as_ref()[8..] happens to be the same 8 bytes
	assert_eq!(&raw.as_ref()[8..], &12345_u64.to_be_bytes());
}

#[test]
fn raw_id_backfilled_shorteventid_returns_count() {
	use super::{Id, RawId};

	let id = Id {
		shortroomid: 42,
		shorteventid: Count::Backfilled(-99),
	};
	let raw: RawId = id.into();

	// Backfilled raw is 24 bytes: [room(8) | 0x00(8) | i64(8)]
	assert_eq!(raw.as_ref().len(), 24);

	// shorteventid() correctly returns the i64 count bytes
	assert_eq!(raw.shorteventid(), (-99_i64).to_be_bytes());

	// REGRESSION: as_ref()[8..16] gives ZEROS, not the count
	assert_eq!(
		&raw.as_ref()[8..16],
		&0_u64.to_be_bytes(),
		"Backfilled raw bytes 8..16 must be zero-tag, NOT the count"
	);

	// REGRESSION: as_ref()[8..] is 16 bytes, not 8 — copy_from_slice
	// into an 8-byte target would only take the zero bytes
	assert_eq!(
		raw.as_ref()[8..].len(),
		16,
		"as_ref()[8..] on Backfilled must be 16 bytes, exposing the bug"
	);
}

#[test]
fn raw_id_roundtrip_backfilled() {
	use super::{Id, RawId};

	let original = Id {
		shortroomid: 0xDEAD_BEEF,
		shorteventid: Count::Backfilled(-42),
	};
	let raw: RawId = original.into();
	let recovered: Id = raw.into();

	assert_eq!(recovered.shortroomid, original.shortroomid);
	assert_eq!(recovered.shorteventid, original.shorteventid);
}
