use std::sync::{Arc, Mutex};

use rocksdb::WriteBatchWithTransaction;

pub struct TransactionContext {
	pub batch: WriteBatchWithTransaction<false>,
	pub on_commit: Vec<Box<dyn FnOnce() + Send>>,
	pub on_rollback: Vec<Box<dyn FnOnce() + Send>>,
	pub committed: bool,
}

impl Default for TransactionContext {
	fn default() -> Self {
		Self {
			batch: Default::default(),
			on_commit: Vec::new(),
			on_rollback: Vec::new(),
			committed: false,
		}
	}
}

impl Drop for TransactionContext {
	fn drop(&mut self) {
		if !self.committed {
			let rollback_closures = std::mem::take(&mut self.on_rollback);
			for closure in rollback_closures {
				if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(closure)) {
					let msg = e
						.downcast_ref::<&'static str>()
						.copied()
						.or_else(|| e.downcast_ref::<String>().map(|s| s.as_str()))
						.unwrap_or("Box<dyn Any>");
					tracing::error!("on_rollback hook panicked: {}", msg);
				}
			}
		}
	}
}

tokio::task_local! {
	pub(crate) static TRANSACTION_BATCH: Arc<Mutex<TransactionContext>>;
}

/// Adds a closure to execute after current transaction commits.
/// Returns true if closure added to a txn, false if no txn active.
pub fn push_on_commit<F>(f: F) -> bool
where
	F: FnOnce() + Send + 'static,
{
	TRANSACTION_BATCH
		.try_with(|txn| {
			let mut txn_guard = txn.lock().expect("Transaction batch mutex poisoned");
			txn_guard.on_commit.push(Box::new(f));
		})
		.is_ok()
}

/// Adds a closure to execute if the current transaction fails (rolls back) or
/// panics. Returns true if closure added to a txn, false if no txn active.
pub fn push_on_rollback<F>(f: F) -> bool
where
	F: FnOnce() + Send + 'static,
{
	TRANSACTION_BATCH
		.try_with(|txn| {
			let mut txn_guard = txn.lock().expect("Transaction batch mutex poisoned");
			txn_guard.on_rollback.push(Box::new(f));
		})
		.is_ok()
}
