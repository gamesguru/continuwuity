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

/// `pdus`/`pdus_rev` in `service::rooms::timeline::data` are EXCLUSIVE of
/// their boundary and rely on this exact operation (`saturating_inc`) at
/// their call sites to become inclusive when needed (e.g.
/// `/members?at=...`). If this arithmetic ever drifts, that boundary
/// handling silently breaks — see the `TestSearch`/`/members?at=` regression
/// this test was added to guard against.
#[test]
fn saturating_inc_forward() {
	use ruma::api::Direction;

	// Normal count
	let count = Count::Normal(10);
	let next = count.saturating_inc(Direction::Forward);
	assert_eq!(next, Count::Normal(11));

	// Backfilled stays Backfilled going forward even once non-negative —
	// saturating_add does not renormalize the variant, only from_signed does.
	let count = Count::Backfilled(-1);
	let next = count.saturating_inc(Direction::Forward);
	assert_eq!(next, Count::Backfilled(0));

	let count = Count::Backfilled(0);
	let next = count.saturating_inc(Direction::Forward);
	assert_eq!(next, Count::Backfilled(1));

	// NOTE (pre-existing latent bug, not introduced by this change):
	// Count::max() is Normal(i64::MAX as u64), but saturating_add operates on
	// the raw u64 (ceiling u64::MAX), not i64::MAX. Incrementing past it
	// produces Normal(i64::MAX as u64 + 1) = Normal(2^63), whose
	// into_signed() cast (`i as i64`) wraps to i64::MIN — so under `Ord`
	// this "incremented max" sorts as the SMALLEST possible Count, not the
	// largest. Documenting the actual (broken) behavior here so it isn't
	// silently relied upon; a real fix belongs in Count::saturating_add,
	// not here.
	let count = Count::max();
	let next = count.saturating_inc(Direction::Forward);
	assert_eq!(next, Count::Normal(i64::MAX as u64 + 1));
	assert!(
		next < Count::max(),
		"known bug: incrementing past Count::max() wraps to the smallest Ord value"
	);
}

/// Documents the exact compensation callers must apply to get inclusive
/// semantics out of the exclusive `pdus`/`pdus_rev` primitives: bump the
/// boundary one step in the direction of travel before passing it in.
/// `members.rs`'s `/members?at=` handler depends on this identity to
/// include the requested point-in-time event as its first `pdus_rev`
/// result instead of the event immediately before it.
#[test]
fn saturating_inc_compensates_exclusive_boundary_for_inclusive_callers() {
	use ruma::api::Direction;

	// pdus_rev(until) excludes `until`; a caller wanting `at` included as the
	// first (most recent) result must request pdus_rev(at + 1) so that
	// "everything strictly before at+1" == "everything up to and including at".
	let at = Count::Normal(42);
	let bumped_for_pdus_rev = at.saturating_inc(Direction::Forward);
	assert_eq!(bumped_for_pdus_rev, Count::Normal(43));

	// pdus(from) excludes `from`; a caller wanting `from` included as the
	// first (earliest) result must request pdus(from - 1).
	let from = Count::Normal(42);
	let bumped_for_pdus = from.saturating_inc(Direction::Backward);
	assert_eq!(bumped_for_pdus, Count::Normal(41));
}

#[test]
fn raw_id_normal_shorteventid_matches_bytes() {
	use super::{Id, RawId};

	let id = Id {
		shortroomid: 42,
		shorteventid: Count::Normal(12345),
	};
	let raw: RawId = id.into();

	// shorteventid() returns the offset-binary-encoded count bytes
	// (sign bit flipped for correct unsigned lexicographic sorting)
	let expected = Count::offset_binary_encoding(12345_i64.to_be_bytes());
	assert_eq!(raw.shorteventid(), expected);

	// as_ref()[8..] is the same 8 encoded bytes in the uniform 16-byte layout
	assert_eq!(&raw.as_ref()[8..], &expected);
}

#[test]
fn raw_id_backfilled_shorteventid_returns_count() {
	use super::{Id, RawId};

	let id = Id {
		shortroomid: 42,
		shorteventid: Count::Backfilled(-99),
	};
	let raw: RawId = id.into();

	// Uniform 16-byte layout: [room(8) | offset_binary_encoded_count(8)]
	assert_eq!(raw.as_ref().len(), 16);

	// shorteventid() returns the offset-binary-encoded count bytes
	let expected = Count::offset_binary_encoding((-99_i64).to_be_bytes());
	assert_eq!(raw.shorteventid(), expected);

	// as_ref()[8..] is exactly 8 bytes — the encoded count
	assert_eq!(raw.as_ref()[8..].len(), 8);
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
