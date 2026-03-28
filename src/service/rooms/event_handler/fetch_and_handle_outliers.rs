use std::{
	collections::{BTreeMap, HashSet, hash_map},
	time::Instant,
};

use conduwuit::{
	Event, PduEvent, debug, implement, info, matrix::event::gen_event_id_canonical_json, trace,
	utils::continue_exponential_backoff_secs, warn,
};
use futures::{
	FutureExt,
	stream::{FuturesUnordered, StreamExt},
};
use ruma::{
	CanonicalJsonValue, EventId, OwnedEventId, RoomId, ServerName,
	api::federation::event::get_event,
};

use super::get_room_version_id;

#[implement(super::Service)]
pub(super) async fn fetch_and_handle_outliers<'a, Pdu, Events>(
	&self,
	origin: &'a ServerName,
	events: Events,
	create_event: &'a Pdu,
	room_id: &'a RoomId,
) -> Vec<(PduEvent, Option<BTreeMap<String, CanonicalJsonValue>>)>
where
	Pdu: Event + Send + Sync,
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let back_off = |id| match self
		.services
		.globals
		.bad_event_ratelimiter
		.write()
		.entry(id)
	{
		| hash_map::Entry::Vacant(e) => {
			e.insert((Instant::now(), 1));
		},
		| hash_map::Entry::Occupied(mut e) => {
			*e.get_mut() = (Instant::now(), e.get().1.saturating_add(1));
		},
	};

	let mut events_with_auth_events = Vec::with_capacity(events.clone().count());
	trace!("Fetching {} outlier pdus", events.clone().count());

	for id in events {
		if self.services.pdu_metadata.is_event_soft_failed(id).await {
			info!(target: "auth_chain", "Skipping known soft-failed outlier: {id}");
			continue;
		}

		if let Ok(local_pdu) = self.services.timeline.get_pdu(id).await {
			trace!("Found {id} in main timeline or outlier tree");
			events_with_auth_events.push((id.to_owned(), Some(local_pdu), vec![]));
			continue;
		}

		let mut events_in_reverse_order = Vec::new();
		let mut events_all = HashSet::with_capacity(32);
		let mut active_fetches = FuturesUnordered::new();

		let limit = self.services.server.config.max_fetch_prev_events;
		if let Some((time, tries)) = self.services.globals.bad_event_ratelimiter.read().get(id) {
			const MIN_DURATION: u64 = 60 * 2;
			const MAX_DURATION: u64 = 60 * 60 * 8;
			// These logs can be disabled with CONDUWUIT_LOG="auth_chain=off,backfill=off"
			if continue_exponential_backoff_secs(
				MIN_DURATION,
				MAX_DURATION,
				time.elapsed(),
				*tries,
			) {
				info!(
					target: "auth_chain",
					tried = ?*tries,
					elapsed = ?time.elapsed(),
					"Backing off from {id} (ratelimited)"
				);
			} else {
				active_fetches.push(
					async move {
						let id = id.to_owned();
						let res = self
							.services
							.sending
							.send_federation_request(origin, get_event::v1::Request {
								event_id: id.clone(),
								include_unredacted_content: None,
							})
							.await;
						(id, res)
					}
					.boxed(),
				);
				events_all.insert(id.to_owned());
			}
		} else {
			active_fetches.push(
				async move {
					let id = id.to_owned();
					let res = self
						.services
						.sending
						.send_federation_request(origin, get_event::v1::Request {
							event_id: id.clone(),
							include_unredacted_content: None,
						})
						.await;
					(id, res)
				}
				.boxed(),
			);
			events_all.insert(id.to_owned());
		}

		while let Some((next_id, fetch_res)) = active_fetches.next().await {
			if events_all.len() >= limit.into() {
				info!(target: "auth_chain", "Max auth event limit reached! Limit: {limit}");
				break;
			}

			match fetch_res {
				| Ok(res) => {
					debug!("Got {next_id} over federation from {origin}");
					let Ok(room_version_id) = get_room_version_id(create_event) else {
						back_off(next_id);
						continue;
					};

					let Ok((calculated_event_id, value)) =
						gen_event_id_canonical_json(&res.pdu, &room_version_id)
					else {
						back_off(next_id);
						continue;
					};

					if calculated_event_id != next_id {
						warn!(
							"Server didn't return event id we requested: requested: {next_id}, \
							 we got {calculated_event_id}. Event: {:?}",
							&res.pdu
						);
					}

					if let Some(auth_events) = value
						.get("auth_events")
						.and_then(CanonicalJsonValue::as_array)
					{
						for auth_event in auth_events {
							if let Ok(auth_event) =
								serde_json::from_value::<OwnedEventId>(auth_event.clone().into())
							{
								if !events_all.contains(&auth_event)
									&& !self.services.timeline.pdu_exists(&auth_event).await
								{
									if self
										.services
										.pdu_metadata
										.is_event_soft_failed(&auth_event)
										.await
									{
										info!(target: "auth_chain", "Skipping known soft-failed auth event: {auth_event}");
										continue;
									}

									let ratelimited = if let Some((time, tries)) = self
										.services
										.globals
										.bad_event_ratelimiter
										.read()
										.get(&*auth_event)
									{
										const MIN_DURATION: u64 = 60 * 2;
										const MAX_DURATION: u64 = 60 * 60 * 8;
										continue_exponential_backoff_secs(
											MIN_DURATION,
											MAX_DURATION,
											time.elapsed(),
											*tries,
										)
									} else {
										false
									};

									if ratelimited {
										info!(target: "auth_chain", "Backing off from {auth_event} (auth event ratelimited)");
										continue;
									}

									if events_all.len() >= limit.into() {
										info!(target: "auth_chain", "Max auth event limit reached! Limit: {limit}");
										break;
									}

									trace!(
										"Found auth event id {auth_event} for event {next_id}"
									);
									let auth_event_clone = auth_event.clone();
									active_fetches.push(
										async move {
											let res = self
												.services
												.sending
												.send_federation_request(
													origin,
													get_event::v1::Request {
														event_id: auth_event_clone.clone(),
														include_unredacted_content: None,
													},
												)
												.await;
											(auth_event_clone, res)
										}
										.boxed(),
									);
									events_all.insert(auth_event);
								}
							}
						}
					} else {
						warn!("Auth event list invalid");
					}

					events_in_reverse_order.push((next_id, value));
				},
				| Err(e) => {
					warn!("Failed to fetch auth event {next_id} from {origin}: {e}");
					back_off(next_id);
				},
			}
		}

		events_with_auth_events.push((id.to_owned(), None, events_in_reverse_order));
	}

	let mut pdus = Vec::with_capacity(events_with_auth_events.len());
	for (id, local_pdu, events_in_reverse_order) in events_with_auth_events {
		if let Some(local_pdu) = local_pdu {
			trace!("Found {id} in main timeline or outlier tree");
			pdus.push((local_pdu.clone(), None));
		}

		for (next_id, value) in events_in_reverse_order.into_iter().rev() {
			if let Some((time, tries)) = self
				.services
				.globals
				.bad_event_ratelimiter
				.read()
				.get(&*next_id)
			{
				const MIN_DURATION: u64 = 5 * 60;
				const MAX_DURATION: u64 = 60 * 60 * 24;
				if continue_exponential_backoff_secs(
					MIN_DURATION,
					MAX_DURATION,
					time.elapsed(),
					*tries,
				) {
					debug!("Backing off from {next_id}");
					continue;
				}
			}

			trace!("Handling outlier {next_id}");
			match Box::pin(self.handle_outlier_pdu(
				origin,
				create_event,
				&next_id,
				room_id,
				value.clone(),
				true,
			))
			.await
			{
				| Ok((pdu, json)) =>
					if next_id == *id {
						trace!("Handled outlier {next_id} (original request)");
						pdus.push((pdu, Some(json)));
					},
				| Err(e) => {
					warn!("Authentication of event {next_id} failed: {e:?}");
					back_off(next_id);
				},
			}
		}
	}
	trace!("Fetched and handled {} outlier pdus", pdus.len());
	pdus
}
