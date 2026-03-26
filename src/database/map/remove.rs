use std::{convert::AsRef, fmt::Debug};

use conduwuit::implement;
use serde::Serialize;

use crate::{keyval::KeyBuf, ser, util::or_else};

#[implement(super::Map)]
#[inline]
pub fn del<K>(&self, key: K)
where
	K: Serialize + Debug,
{
	let mut buf = KeyBuf::new();
	let key = ser::serialize(&mut buf, key).expect("failed to serialize deletion key");
	self.remove_raw(key);
}

#[implement(super::Map)]
#[inline]
pub fn remove<K>(&self, key: &K)
where
	K: AsRef<[u8]> + ?Sized + Debug,
{
	self.remove_raw(key.as_ref());
}

#[implement(super::Map)]
#[inline]
pub fn remove_raw(&self, key: &[u8]) {
	let write_options = &self.write_options;
	let appended_to_txn = crate::transaction::TRANSACTION_BATCH
		.try_with(|batch| {
			// blocking_lock is structurally safe here since TRANSACTION_BATCH
			// is task_local! and only accessed consecutively within the same
			// async task. Falling back with try_lock would silently break atomicity.
			let mut batch_guard = batch.lock().expect("Transaction batch mutex poisoned");
			let (batch, _closures) = &mut *batch_guard;
			batch.delete_cf(&self.cf(), key);
		})
		.is_ok();

	if !appended_to_txn {
		self.db
			.db
			.delete_cf_opt(&self.cf(), key, write_options)
			.or_else(or_else)
			.expect("database remove error");

		// Honor corking semantics for remove operations: when not corked,
		// ensure the delete is flushed in the same way as inserts.
		if !self.db.corked() {
			self.db.flush().expect("database flush error after remove");
		}
	}

	self.watchers.wake(key);
}
