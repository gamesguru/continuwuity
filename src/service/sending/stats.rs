use std::sync::atomic::{AtomicU64, Ordering};

use conduwuit::info;

/// Lightweight atomic counters for federation activity.
/// Logged periodically and reset after each report.
#[derive(Default)]
pub struct FederationStats {
	pub outgoing_txns: AtomicU64,
	pub outgoing_pdus: AtomicU64,
	pub outgoing_edus: AtomicU64,
	pub outgoing_presence: AtomicU64,
	pub outgoing_errors: AtomicU64,
}

impl FederationStats {
	/// Log a summary and reset all counters. Returns true if any activity
	/// occurred.
	pub fn report_and_reset(&self) -> bool {
		let txns = self.outgoing_txns.swap(0, Ordering::Relaxed);
		let pdus = self.outgoing_pdus.swap(0, Ordering::Relaxed);
		let edus = self.outgoing_edus.swap(0, Ordering::Relaxed);
		let presence = self.outgoing_presence.swap(0, Ordering::Relaxed);
		let errors = self.outgoing_errors.swap(0, Ordering::Relaxed);

		if txns == 0 && pdus == 0 && edus == 0 {
			return false;
		}

		info!(
			"federation stats: {txns} txns ({pdus} PDUs, {edus} EDUs, {presence} presence), \
			 {errors} errors"
		);

		true
	}
}
