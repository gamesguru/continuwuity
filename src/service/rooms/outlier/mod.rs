use std::sync::Arc;

use conduwuit::{Result, implement, matrix::PduEvent, utils::stream::TryIgnore};
use database::{Deserialized, Json, Map};
use futures::Stream;
use ruma::{CanonicalJsonObject, EventId, OwnedEventId};

pub struct Service {
	db: Data,
}

struct Data {
	eventid_outlierpdu: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_outlierpdu: args.db["eventid_outlierpdu"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

/// Returns the pdu from the outlier tree.
#[implement(Service)]
pub async fn get_pdu_outlier(&self, event_id: &EventId) -> Result<PduEvent> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

#[implement(Service)]
pub fn stream(&self) -> impl Stream<Item = (OwnedEventId, PduEvent)> + Send + '_ {
	self.db
		.eventid_outlierpdu
		.stream::<OwnedEventId, PduEvent>()
		.ignore_err()
}

/// Append the PDU as an outlier.
#[implement(Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) {
	self.db.eventid_outlierpdu.raw_put(event_id, Json(pdu));
}

/// Remove the PDU from the outlier tree.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn remove_outlier(&self, event_id: &EventId) { self.db.eventid_outlierpdu.remove(event_id); }
