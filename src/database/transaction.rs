use std::sync::{Arc, Mutex};

use rocksdb::WriteBatchWithTransaction;

pub type TransactionContext = (WriteBatchWithTransaction<false>, Vec<Box<dyn FnOnce() + Send>>);

tokio::task_local! {
	pub(crate) static TRANSACTION_BATCH: Arc<Mutex<TransactionContext>>;
}
