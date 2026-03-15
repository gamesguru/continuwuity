use std::sync::{Arc, atomic::Ordering};

use crate::{Database, Engine};

pub struct Cork {
	db: Arc<Engine>,
	flush: bool,
	sync: bool,
}

impl Database {
	#[inline]
	#[must_use]
	pub fn cork(&self) -> Cork { Cork::new(&self.db, false, false) }

	#[inline]
	#[must_use]
	pub fn cork_and_flush(&self) -> Cork { Cork::new(&self.db, true, false) }

	#[inline]
	#[must_use]
	pub fn cork_and_sync(&self) -> Cork { Cork::new(&self.db, true, true) }
}

impl Cork {
	#[inline]
	pub(super) fn new(db: &Arc<Engine>, flush: bool, sync: bool) -> Self {
		db.cork();
		Self { db: db.clone(), flush, sync }
	}
}

impl Drop for Cork {
	fn drop(&mut self) {
		if self.flush {
			self.db.pending_flush.store(true, Ordering::Relaxed);
		}
		if self.sync {
			self.db.pending_sync.store(true, Ordering::Relaxed);
		}

		if self.db.uncork() {
			let sync = self.db.pending_sync.swap(false, Ordering::Acquire);
			let flush = self.db.pending_flush.swap(false, Ordering::Acquire);

			if sync {
				self.db.sync().ok();
			} else if flush {
				self.db.flush().ok();
			}
		}
	}
}
