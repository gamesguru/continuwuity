use std::sync::Arc;

use rocksdb::WriteBatchWithTransaction;
use tokio::sync::Mutex;

tokio::task_local! {
	pub static TRANSACTION_BATCH: Arc<Mutex<WriteBatchWithTransaction<false>>>;
}
