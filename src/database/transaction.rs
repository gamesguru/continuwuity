use std::sync::Arc;

use rocksdb::WriteBatchWithTransaction;
use tokio::sync::Mutex;

pub type TransactionContext = (WriteBatchWithTransaction<false>, Vec<Box<dyn FnOnce() + Send>>);

tokio::task_local! {
	pub static TRANSACTION_BATCH: Arc<Mutex<TransactionContext>>;
}
