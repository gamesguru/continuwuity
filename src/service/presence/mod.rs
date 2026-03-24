mod data;
mod presence;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use conduwuit::{
	Error, Result, Server, checked, debug, debug_info, debug_warn, error, info, result::LogErr,
	warn,
};
use futures::{Stream, TryFutureExt};
use loole::{Receiver, Sender};
use ruma::{OwnedUserId, UInt, UserId, events::presence::PresenceEvent, presence::PresenceState};

use self::{data::Data, presence::Presence};
use crate::{Dep, globals, users};

pub struct Service {
	timer_channel: (Sender<TimerType>, Receiver<TimerType>),
	timeout_remote_users: bool,
	idle_timeout: u64,
	offline_timeout: u64,
	db: Data,
	services: Services,
}

struct Services {
	server: Arc<Server>,
	globals: Dep<globals::Service>,
	users: Dep<users::Service>,
}

type TimerType = (OwnedUserId, Option<Duration>);

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let idle_timeout_s = config.presence_idle_timeout_s;
		let offline_timeout_s = config.presence_offline_timeout_s;
		Ok(Arc::new(Self {
			timer_channel: loole::unbounded(),
			timeout_remote_users: config.presence_timeout_remote_users,
			idle_timeout: checked!(idle_timeout_s * 1_000)?,
			offline_timeout: checked!(offline_timeout_s * 1_000)?,
			db: Data::new(&args),
			services: Services {
				server: args.server.clone(),
				globals: args.depend::<globals::Service>("globals"),
				users: args.depend::<users::Service>("users"),
			},
		}))
	}

	async fn worker(self: Arc<Self>) -> Result<()> {
		let receiver = self.timer_channel.1.clone();

		// Resetting dormant online/away statuses to offline on startup
		let startup_task = if self.services.server.config.allow_local_presence {
			let self_ = Arc::clone(&self);
			Some(self.services.server.runtime().spawn(async move {
				self_.unset_all_presence().await;
				_ = self_
					.ping_presence(&self_.services.globals.server_user, &PresenceState::Online)
					.await;
			}))
		} else {
			None
		};

		let mut presence_timers =
			std::collections::HashMap::<OwnedUserId, tokio::task::JoinHandle<()>>::new();
		let mut events_received: u64 = 0;
		let mut next_tally = tokio::time::Instant::now()
			.checked_add(Duration::from_secs(300))
			.unwrap_or_else(tokio::time::Instant::now);

		while !receiver.is_closed() {
			let event = receiver.recv_async().await;
			match event {
				| Err(_) => break,
				| Ok((user_id, Some(timeout))) => {
					events_received = events_received.saturating_add(1);
					let self_clone = Arc::clone(&self);
					let user_id_clone = user_id.clone();

					let new_task = self.services.server.runtime().spawn(async move {
						tokio::time::sleep(timeout).await;
						self_clone
							.process_presence_timer(&user_id_clone)
							.await
							.log_err()
							.ok();
					});

					if let Some(old_task) = presence_timers.insert(user_id, new_task) {
						old_task.abort();
					}
				},
				| Ok((user_id, None)) =>
					if let Some(task) = presence_timers.remove(&user_id) {
						task.abort();
					},
			}

			// Periodic tally
			if tokio::time::Instant::now() >= next_tally {
				warn!(
					"presence stats: {} active timers, {} received",
					presence_timers.len(),
					events_received
				);
				events_received = 0;
				next_tally = tokio::time::Instant::now()
					.checked_add(Duration::from_secs(300))
					.unwrap_or_else(tokio::time::Instant::now);
			}
		}

		for (_, handle) in presence_timers {
			handle.abort();
		}

		if let Some(task) = startup_task {
			_ = task.await;
		}

		Ok(())
	}

	fn interrupt(&self) {
		let (timer_sender, _) = &self.timer_channel;
		if !timer_sender.is_closed() {
			timer_sender.close();
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Returns the latest presence event for the given user.
	#[inline]
	pub async fn get_presence(&self, user_id: &UserId) -> Result<PresenceEvent> {
		self.db
			.get_presence(user_id)
			.map_ok(|(_, presence)| presence)
			.await
	}

	/// Pings the presence of the given user in the given room, setting the
	/// specified state.
	pub async fn ping_presence(&self, user_id: &UserId, new_state: &PresenceState) -> Result<()> {
		const REFRESH_TIMEOUT: u64 = 60 * 1000;

		// I am working on an unrelated PR. But just seeing this is making me cringe.
		// TODO: Do not read or set presence from the DB if it is unchanged
		// (i.e., cached). Probably 90% or more of status updates are redundant and
		// can be processed much more efficiently in RAM than by making multiple
		// unconditional, barbaric DB calls!
		let last_presence = self.db.get_presence(user_id).await;
		let state_changed = match last_presence {
			| Err(_) => true,
			| Ok((_, ref presence)) => presence.content.presence != *new_state,
		};

		let last_last_active_ago = match last_presence {
			| Err(_) => 0_u64,
			| Ok((_, ref presence)) =>
				presence.content.last_active_ago.unwrap_or_default().into(),
		};

		if !state_changed && last_last_active_ago < REFRESH_TIMEOUT {
			return Ok(());
		}

		let status_msg = match last_presence {
			| Ok((_, ref presence)) => presence.content.status_msg.clone(),
			| Err(_) => Some(String::new()),
		};

		let last_active_ago = UInt::new(0);
		let currently_active = *new_state == PresenceState::Online;
		// TODO: As above, this makes a dumb, unconditional DB call. Please implement
		// a basic LRU.
		self.set_presence(user_id, new_state, Some(currently_active), last_active_ago, status_msg)
			.await
	}

	/// Adds a presence event which will be saved until a new event replaces it.
	pub async fn set_presence(
		&self,
		user_id: &UserId,
		state: &PresenceState,
		currently_active: Option<bool>,
		last_active_ago: Option<UInt>,
		status_msg: Option<String>,
	) -> Result<()> {
		let presence_state = match state.as_str() {
			| "" => &PresenceState::Offline, // default an empty string to 'offline'
			| &_ => state,
		};

		self.db
			.set_presence(user_id, presence_state, currently_active, last_active_ago, status_msg)
			.await?;

		if (self.timeout_remote_users || self.services.globals.user_is_local(user_id))
			&& user_id != self.services.globals.server_user
		{
			let timeout = match presence_state {
				| PresenceState::Online => self.services.server.config.presence_idle_timeout_s,
				| _ => self.services.server.config.presence_offline_timeout_s,
			};

			self.timer_channel
				.0
				.send((user_id.to_owned(), Some(Duration::from_secs(timeout))))
				.map_err(|e| {
					error!("Failed to add presence timer: {}", e);
					Error::bad_database("Failed to add presence timer")
				})?;
		}

		Ok(())
	}

	/// Removes the presence record for the given user from the database.
	///
	/// TODO: Why is this not used?
	#[allow(dead_code)]
	pub async fn remove_presence(&self, user_id: &UserId) {
		self.db.remove_presence(user_id).await;
	}

	// Unset online/unavailable presence to offline on startup
	pub async fn unset_all_presence(&self) {
		use futures::{StreamExt, stream::FuturesUnordered};

		debug_info!("Resetting presence for active users...");
		let mut reset = 0_usize;
		let mut jobs = FuturesUnordered::new();

		let mut presence_stream = Box::pin(self.db.presence_since(0));
		while let Some((user_id, _count, bytes)) = presence_stream.next().await {
			if !self.services.server.running() {
				info!("Shutdown requested during presence reset.");
				break;
			}

			if !self.services.globals.user_is_local(user_id)
				|| user_id == self.services.globals.server_user
			{
				continue;
			}

			let Ok(presence) = Presence::from_json_bytes(bytes) else {
				continue;
			};

			if !matches!(
				presence.state,
				PresenceState::Unavailable | PresenceState::Online | PresenceState::Busy
			) {
				continue;
			}

			let user_id = user_id.to_owned();
			let now = conduwuit::utils::millis_since_unix_epoch();
			let last_active_ago =
				Some(UInt::new_saturating(now.saturating_sub(presence.last_active_ts)));

			reset = reset.saturating_add(1);

			let status_msg = presence.status_msg.clone();

			jobs.push(async move {
				if let Err(e) = self
					.db
					.set_presence_silent(
						&user_id,
						&PresenceState::Offline,
						Some(false),
						last_active_ago,
						status_msg,
					)
					.await
				{
					debug_warn!(
						?user_id,
						"Failed to reset presence for {user_id} to offline: {e}"
					);
				}
			});

			if jobs.len() >= 100 {
				while jobs.next().await == Some(()) {
					if jobs.len() < 50 {
						break;
					}
				}
			}
		}

		while jobs.next().await == Some(()) {}

		warn!("Presence reset complete: {reset} users reset to offline.");
	}

	/// Returns the most recent presence updates that happened after the event
	/// with id `since`.
	pub fn presence_since(
		&self,
		since: u64,
	) -> impl Stream<Item = (&UserId, u64, &[u8])> + Send + '_ {
		self.db.presence_since(since)
	}

	#[inline]
	pub async fn from_json_bytes_to_event(
		&self,
		bytes: &[u8],
		user_id: &UserId,
	) -> Result<PresenceEvent> {
		let presence = Presence::from_json_bytes(bytes)?;
		let event = presence
			.to_presence_event(user_id, &self.services.users)
			.await;

		Ok(event)
	}

	async fn process_presence_timer(&self, user_id: &OwnedUserId) -> Result<()> {
		let mut presence_state = PresenceState::Offline;
		let mut last_active_ago = None;
		let mut status_msg = None;

		let presence_event = self.get_presence(user_id).await;

		if let Ok(presence_event) = presence_event {
			presence_state = presence_event.content.presence;
			last_active_ago = presence_event.content.last_active_ago;
			status_msg = presence_event.content.status_msg;
		}

		let new_state = match (&presence_state, last_active_ago.map(u64::from)) {
			| (PresenceState::Online, Some(ago)) if ago >= self.idle_timeout =>
				Some(PresenceState::Unavailable),
			| (PresenceState::Unavailable, Some(ago)) if ago >= self.offline_timeout =>
				Some(PresenceState::Offline),
			| _ => None,
		};

		debug!(
			"Processed presence timer for user '{user_id}': Old state = {presence_state}, New \
			 state = {new_state:?}"
		);

		if let Some(new_state) = new_state {
			self.set_presence(user_id, &new_state, Some(false), last_active_ago, status_msg)
				.await?;
		}

		Ok(())
	}
}
