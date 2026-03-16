#![type_length_limit = "3072"]

extern crate conduwuit_core as conduwuit;
extern crate rust_rocksdb as rocksdb;

conduwuit::mod_ctor! {}
conduwuit::mod_dtor! {}

#[cfg(test)]
mod benches;
mod cork;
mod de;
mod deserialized;
mod engine;
mod handle;
pub mod keyval;
mod map;
pub mod maps;
mod pool;
mod ser;
mod stream;
#[cfg(test)]
mod tests;
pub mod transaction;
pub(crate) mod util;
mod watchers;

use std::{future::Future, ops::Index, sync::Arc};

use conduwuit::{Result, Server, err};

pub use self::{
	de::{Ignore, IgnoreAll},
	deserialized::Deserialized,
	handle::Handle,
	keyval::{KeyVal, Slice, serialize_key, serialize_val},
	map::{Get, Map, Qry, compact},
	ser::{Cbor, Interfix, Json, SEP, Separator, serialize, serialize_to, serialize_to_vec},
};
pub(crate) use self::{
	engine::{Engine, context::Context},
	util::or_else,
};
use crate::maps::{Maps, MapsKey, MapsVal};

pub struct Database {
	maps: Maps,
	pub db: Arc<Engine>,
	pub(crate) _ctx: Arc<Context>,
}

impl Database {
	/// Load an existing database or create a new one.
	pub async fn open(server: &Arc<Server>) -> Result<Arc<Self>> {
		let ctx = Context::new(server)?;
		let db = Engine::open(ctx.clone(), maps::MAPS).await?;
		Ok(Arc::new(Self {
			maps: maps::open(&db)?,
			db: db.clone(),
			_ctx: ctx,
		}))
	}

	#[inline]
	pub fn get(&self, name: &str) -> Result<&Arc<Map>> {
		self.maps
			.get(name)
			.ok_or_else(|| err!(Request(NotFound("column not found"))))
	}

	#[inline]
	pub fn iter(&self) -> impl Iterator<Item = (&MapsKey, &MapsVal)> + Send + '_ {
		self.maps.iter()
	}

	#[inline]
	pub fn keys(&self) -> impl Iterator<Item = &MapsKey> + Send + '_ { self.maps.keys() }

	/// Executes a block of database operations using a write batch.
	///
	/// All operations that go through [`transaction::TRANSACTION_BATCH`]
	/// (currently [`Map::insert`] and [`Map::remove`]) are buffered into a
	/// single [`WriteBatch`](rocksdb::WriteBatchWithTransaction). If the
	/// closure returns `Ok`, that batch is written atomically; if it returns
	/// an error the batch is dropped without being written (rollback).
	///
	/// Other write paths that bypass [`transaction::TRANSACTION_BATCH`]
	/// (such as direct `write_opt`/`put_cf_opt` calls or `insert_batch`)
	/// are **not** included in this batch and will commit immediately even
	/// when invoked inside the closure. Callers must ensure they only use
	/// transaction-aware APIs inside this method if they require atomicity.
	///
	/// **Note:** Nested transactions are not supported. Calling
	/// [`Database::transaction`] from within another `transaction` closure
	/// will cause a panic at runtime (via an internal assertion) instead of
	/// creating a new independent batch. Callers must avoid invoking this
	/// method reentrantly and should structure their code to use a single
	/// outer transaction when atomicity is required.
	pub async fn transaction<F, Fut, R>(&self, f: F) -> Result<R>
	where
		F: FnOnce() -> Fut,
		Fut: Future<Output = Result<R>>,
	{
		use std::sync::Mutex;

		assert!(
			transaction::TRANSACTION_BATCH.try_with(|_| ()).is_err(),
			"Nested Database::transaction() calls are not supported and break atomicity."
		);

		let batch = Arc::new(Mutex::new(transaction::TransactionContext::default()));

		let res = transaction::TRANSACTION_BATCH
			.scope(batch.clone(), async { f().await })
			.await?;

		let mut batch_guard = batch.lock().expect("Transaction batch mutex poisoned");
		let write_options = map::write_options_default(&self.db);
		self.db
			.db
			.write_opt(&batch_guard.batch, &write_options)
			.or_else(or_else)?;

		if !self.db.corked() {
			self.db.flush().expect("database flush error");
		}

		// Mark as committed immediately after a successful write and flush. If flush()
		// panics, we want to run on_rollback closures.
		batch_guard.committed = true;

		// Move the on-commit closures out of the mutex-protected struct, then drop
		// the guard before executing them to avoid holding the mutex during arbitrary
		// callback execution.
		let wake_closures = std::mem::take(&mut batch_guard.on_commit);
		drop(batch_guard);

		for wake_closure in wake_closures {
			// Ensure that a panic in one on-commit hook does not prevent subsequent
			// hooks from running, and does not unwind past this point after the
			// transaction has already been committed.
			if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				wake_closure();
			})) {
				let msg = e
					.downcast_ref::<&'static str>()
					.copied()
					.or_else(|| e.downcast_ref::<String>().map(|s| s.as_str()))
					.unwrap_or("Box<dyn Any>");
				tracing::error!("on_commit hook panicked: {}", msg);
			}
		}

		Ok(res)
	}

	/// Adds a closure to be executed after the current transaction successfully
	/// commits. Returns true if the closure was successfully added to a
	/// transaction, false if there is no active transaction.
	pub fn push_on_commit<F>(&self, f: F) -> bool
	where
		F: FnOnce() + Send + 'static,
	{
		transaction::push_on_commit(f)
	}
}

impl Index<&str> for Database {
	type Output = Arc<Map>;

	fn index(&self, name: &str) -> &Self::Output {
		self.maps
			.get(name)
			.expect("column in database does not exist")
	}
}

#[cfg(test)]
mod transaction_tests {
	use super::*;

	#[tokio::test]
	async fn test_transaction_batch_rejects_nested_scope() {
		// Mock config and database initialization.
		// Testing this directly is tricky because Database::load requires
		// setting up proper args, config, and RocksDB directories but we
		// can also just simulate the try_with behaviour manually to verify the
		// transaction scope behaviour.

		let batch = Arc::new(std::sync::Mutex::new(transaction::TransactionContext::default()));

		// Here we simulate being inside an existing transaction batch:
		transaction::TRANSACTION_BATCH
			.scope(batch, async {
				// Attempting to open another batch from inside the scope must fail.
				assert!(
					transaction::TRANSACTION_BATCH.try_with(|_| ()).is_err(),
					"TRANSACTION_BATCH should reject nested scopes to preserve atomicity."
				);
			})
			.await;
	}

	#[tokio::test]
	async fn test_push_on_commit_queues_closures() {
		let batch = Arc::new(std::sync::Mutex::new(transaction::TransactionContext::default()));

		// Simulate being inside a transaction
		transaction::TRANSACTION_BATCH
			.scope(batch.clone(), async {
				let success = transaction::push_on_commit(|| {});

				assert!(success, "push_on_commit inside transaction should succeed");

				let guard = batch.lock().unwrap();
				assert_eq!(
					guard.on_commit.len(),
					1,
					"Closure should be queued in the transaction batch"
				);
			})
			.await;
	}

	#[tokio::test]
	async fn test_push_on_rollback_queues_closures() {
		let batch = Arc::new(std::sync::Mutex::new(transaction::TransactionContext::default()));

		// Simulate being inside a transaction
		transaction::TRANSACTION_BATCH
			.scope(batch.clone(), async {
				let success = transaction::push_on_rollback(|| {});

				assert!(success, "push_on_rollback inside transaction should succeed");

				let guard = batch.lock().unwrap();
				assert_eq!(
					guard.on_rollback.len(),
					1,
					"Rollback closure should be queued in the transaction batch"
				);
			})
			.await;
	}

	#[tokio::test]
	async fn test_on_rollback_executes_on_drop() {
		let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let ran_clone = ran.clone();

		{
			let batch =
				Arc::new(std::sync::Mutex::new(transaction::TransactionContext::default()));
			transaction::TRANSACTION_BATCH
				.scope(batch.clone(), async move {
					transaction::push_on_rollback(move || {
						ran_clone.store(true, std::sync::atomic::Ordering::SeqCst);
					});
				})
				.await;
			// batch goes out of scope here and drops TransactionContext
		}

		assert!(
			ran.load(std::sync::atomic::Ordering::SeqCst),
			"Rollback closure should run when TransactionContext is dropped without being \
			 committed"
		);
	}

	#[tokio::test]
	async fn test_on_rollback_does_not_run_if_committed() {
		let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let ran_clone = ran.clone();

		{
			let batch =
				Arc::new(std::sync::Mutex::new(transaction::TransactionContext::default()));
			transaction::TRANSACTION_BATCH
				.scope(batch.clone(), async move {
					transaction::push_on_rollback(move || {
						ran_clone.store(true, std::sync::atomic::Ordering::SeqCst);
					});

					let mut guard = batch.lock().unwrap();
					guard.committed = true;
				})
				.await;
		}

		assert!(
			!ran.load(std::sync::atomic::Ordering::SeqCst),
			"Rollback closure should NOT run when TransactionContext is dropped after being \
			 committed"
		);
	}

	#[test]
	fn test_push_on_commit_outside_transaction() {
		// Outside of any transaction::TRANSACTION_BATCH scope
		let success = transaction::push_on_commit(|| {});

		assert!(!success, "push_on_commit outside transaction should return false");
	}

	#[test]
	fn test_push_on_rollback_outside_transaction() {
		// Outside of any transaction::TRANSACTION_BATCH scope
		let success = transaction::push_on_rollback(|| {});

		assert!(!success, "push_on_rollback outside transaction should return false");
	}
}
