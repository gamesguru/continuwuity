use conduwuit::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn clear_ratelimiter(&self) -> Result {
	self.bail_restricted()?;
	self.services.globals.bad_event_ratelimiter.write().clear();
	self.write_str("Cleared the global bad_event ratelimiter cache.")
		.await
}
