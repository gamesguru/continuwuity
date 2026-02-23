use std::{convert::AsRef, fmt::Debug, io::Write};

use conduwuit::{arrayvec::ArrayVec, implement};
use serde::Serialize;

use crate::{keyval::KeyBuf, ser, util::or_else};

#[implement(super::Map)]
#[inline]
pub fn del<K>(&self, key: K)
where
	K: Serialize + Debug,
{
	let mut buf = KeyBuf::new();
	self.bdel(key, &mut buf);
}

#[implement(super::Map)]
#[inline]
pub fn adel<const MAX: usize, K>(&self, key: K)
where
	K: Serialize + Debug,
{
	let mut buf = ArrayVec::<u8, MAX>::new();
	self.bdel(key, &mut buf);
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, buf), level = "trace")]
pub fn bdel<K, B>(&self, key: K, buf: &mut B)
where
	K: Serialize + Debug,
	B: Write + AsRef<[u8]>,
{
	let key = ser::serialize(buf, key).expect("failed to serialize deletion key");
	self.remove(key);
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, key), fields(%self), level = "trace")]
pub fn remove<K>(&self, key: &K)
where
	K: AsRef<[u8]> + ?Sized + Debug,
{
	let write_options = &self.write_options;

	let appended_to_txn = crate::transaction::TRANSACTION_BATCH
		.try_with(|batch| {
			// blocking_lock is structurally safe here since TRANSACTION_BATCH
			// is task_local! and only accessed consecutively within the same
			// async task. Falling back with try_lock would silently break atomicity.
			let mut batch_guard = batch.lock().expect("Transaction batch mutex poisoned");
			let (batch, _closures) = &mut *batch_guard;
			batch.delete_cf(&self.cf(), key.as_ref());
		})
		.is_ok();

	if !appended_to_txn {
		self.db
			.db
			.delete_cf_opt(&self.cf(), key, write_options)
			.or_else(or_else)
			.expect("database remove error");

		if !self.db.corked() {
			self.db.flush().expect("database flush error");
		}
	}
}
