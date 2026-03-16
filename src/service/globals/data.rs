use std::{collections::BTreeSet, sync::Arc};

use conduwuit::{Result, SyncRwLock, utils};
use database::{Database, Deserialized, Map};

pub struct Data {
	global: Arc<Map>,
	counter: SyncRwLock<u64>,
	in_flight_txn_counts: Arc<SyncRwLock<BTreeSet<u64>>>,
	pub(super) db: Arc<Database>,
}

const COUNTER: &[u8] = b"c";

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			global: db["global"].clone(),
			counter: SyncRwLock::new(Self::stored_count(&db["global"]).unwrap_or_default()),
			in_flight_txn_counts: Arc::new(SyncRwLock::new(BTreeSet::new())),
			db: args.db.clone(),
		}
	}

	pub fn next_count(&self) -> Result<u64> {
		let _cork = self.db.cork();
		let mut lock = self.counter.write();
		let counter: &mut u64 = &mut lock;
		if self.in_flight_txn_counts.read().is_empty() {
			// Although, this may be more risky than devs-only worth seeing
			debug_assert!(
				*counter == Self::stored_count(&self.global).unwrap_or_default(),
				"counter mismatch"
			);
		}

		*counter = counter.checked_add(1).unwrap_or(*counter);
		let count = *counter;

		// Track this count as in-flight FIRST
		self.in_flight_txn_counts.write().insert(count);

		// THEN expose it to the database/global state
		self.global.insert(COUNTER, count.to_be_bytes());

		let in_flight = self.in_flight_txn_counts.clone();
		let in_flight_rollback = self.in_flight_txn_counts.clone();

		// Open the txn
		let in_txn = self.db.push_on_commit(move || {
			in_flight.write().remove(&count);
		});

		if in_txn {
			let global_rollback = self.global.clone();
			// Register fallback/rollback hook so token doesn't get stuck
			self.db.push_on_rollback(move || {
				in_flight_rollback.write().remove(&count);
				// Expose rollback count to the DB directly (outside txn batch),
				// so it is permanently skipped on restart, and clients will
				// see it as a reused event.
				global_rollback.insert(COUNTER, count.to_be_bytes());
			});
		} else {
			// If NOT in a txn, the write was synchronous
			self.in_flight_txn_counts.write().remove(&count);
		}

		Ok(count)
	}

	#[inline]
	pub fn current_count(&self) -> u64 {
		let lock = self.counter.read();
		let counter: &u64 = &lock;
		debug_assert!(
			*counter == Self::stored_count(&self.global).unwrap_or_default(),
			"counter mismatch"
		);

		*counter
	}

	/// Returns a lower-bound sequence number that is safe to expose to readers.
	///
	/// If there are any counts currently held by uncommitted transactions, this
	/// returns one less than the smallest such count; otherwise it returns the
	/// current global count. Read/write isolation is provided against the
	/// global sequence counter, preventing `/sync` from advancing past events
	/// assigned a sequence number but not yet committed to the DB.
	pub fn current_count_in_flight(&self) -> u64 {
		let current = *self.counter.read();

		let lock = self.in_flight_txn_counts.read();
		if let Some(first_in_flight) = lock.first() {
			// If there are transactions in flight, clients should not sync past the
			// lowest sequence number currently held by an uncommitted transaction.
			// NOTE: Hopefully safe to return one less than earliest in-flight seq num.
			return first_in_flight.saturating_sub(1);
		}

		current
	}

	fn stored_count(global: &Arc<Map>) -> Result<u64> {
		global
			.get_blocking(COUNTER)
			.as_deref()
			.map_or(Ok(0_u64), utils::u64_from_bytes)
	}

	pub async fn database_version(&self) -> u64 {
		self.global
			.get(b"version")
			.await
			.deserialized()
			.unwrap_or(0)
	}

	#[inline]
	pub fn bump_database_version(&self, new_version: u64) {
		self.global.raw_put(b"version", new_version);
	}
}
