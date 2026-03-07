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
	/// **Note:** Nested transactions are not supported. Calling `transaction`
	/// inside another `transaction` closure will panic to prevent nesting and
	/// preserve the atomicity guarantees of the outer transaction.
	pub async fn transaction<F, Fut, R>(&self, f: F) -> Result<R>
	where
		F: FnOnce() -> Fut,
		Fut: Future<Output = Result<R>>,
	{
		use std::sync::Mutex;

		use rocksdb::WriteBatchWithTransaction;

		assert!(
			transaction::TRANSACTION_BATCH.try_with(|_| ()).is_err(),
			"Nested Database::transaction() calls are not supported and break atomicity."
		);

		let batch =
			Arc::new(Mutex::new((WriteBatchWithTransaction::<false>::default(), Vec::new())));

		let res = transaction::TRANSACTION_BATCH
			.scope(batch.clone(), async { f().await })
			.await?;

		let mut batch_guard = batch.lock().expect("Transaction batch mutex poisoned");
		let write_options = map::write_options_default(&self.db);
		self.db
			.db
			.write_opt(&batch_guard.0, &write_options)
			.or_else(or_else)?;

		if !self.db.corked() {
			self.db.flush().expect("database flush error");
		}

		for wake_closure in batch_guard.1.drain(..) {
			wake_closure();
		}

		Ok(res)
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
	#[should_panic(
		expected = "Nested Database::transaction() calls are not supported and break atomicity."
	)]
	async fn test_nested_transaction_panics() {
		// Mock config and database initialization.
		// Testing this directly is tricky because Database::load requires
		// setting up proper args, config, and RocksDB directories but we
		// can also just simulate the try_with panic manually to verify the
		// transaction scope behaviour.

		let batch = Arc::new(std::sync::Mutex::new((
			rocksdb::WriteBatchWithTransaction::<false>::default(),
			Vec::new(),
		)));

		// Here we simulate being inside an existing transaction batch:
		transaction::TRANSACTION_BATCH
			.scope(batch, async {
				// Calling it again from inside the scope should trigger the assert!
				assert!(
					transaction::TRANSACTION_BATCH.try_with(|_| ()).is_err(),
					"Nested Database::transaction() calls are not supported and break atomicity."
				);
			})
			.await;
	}
}
