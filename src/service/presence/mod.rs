mod data;
mod presence;

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use conduwuit::{
	Error, Result, Server, checked, debug, debug_warn, error, info, result::LogErr, trace, utils,
};
use database::Database;
use futures::{Stream, StreamExt, TryFutureExt, stream::FuturesUnordered};
use loole::{Receiver, Sender};
use ruma::{OwnedUserId, UInt, UserId, events::presence::PresenceEvent, presence::PresenceState};
use tokio::time::{Instant, sleep};

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
	db: Arc<Database>,
	globals: Dep<globals::Service>,
	users: Dep<users::Service>,
}

type TimerType = (OwnedUserId, Duration);

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
				db: args.db.clone(),
				globals: args.depend::<globals::Service>("globals"),
				users: args.depend::<users::Service>("users"),
			},
		}))
	}

	async fn clear_cache(&self) { self.db.clear_cache(); }

	async fn worker(self: Arc<Self>) -> Result<()> {
		let receiver = self.timer_channel.1.clone();

		// Timers scheduled to auto-demote idle users (online -> unavailable -> offline)
		let mut presence_timers = FuturesUnordered::new();
		let mut scheduled_at: HashMap<OwnedUserId, Instant> = HashMap::new();
		while !receiver.is_closed() {
			tokio::select! {
				Some((user_id, created_at)) = presence_timers.next() => {
					// Only process latest timer, avoid overhead of whole list (they may have many updates in the queue)
					if scheduled_at.get(&user_id) == Some(&created_at) {
						scheduled_at.remove(&user_id);
						self.process_presence_timer(&user_id).await.log_err().ok();
					}
				},
				event = receiver.recv_async() => match event {
					Err(_) => break,
					Ok((user_id, timeout)) => {
						let now = Instant::now();
						scheduled_at.insert(user_id.clone(), now);
						debug!("Adding timer {}: {user_id} timeout:{timeout:?}", presence_timers.len());
						presence_timers.push(presence_timer(user_id, timeout, now));
					},
				},
			}
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

	/// Pings the presence of the given user, setting the specified state.
	///
	/// Fetches previous state, then passes it to avoid redundant DB read in
	/// `set_presence`
	pub async fn ping_presence(&self, user_id: &UserId, new_state: &PresenceState) -> Result<()> {
		const REFRESH_TIMEOUT: u64 = 60 * 1000;

		// Raw payload is smaller/cheaper in memory
		let last_presence = self.db.get_presence_raw(user_id).await;
		let state_changed = match last_presence {
			| Err(_) => true,
			| Ok((_, ref presence)) => presence.state != *new_state,
		};

		let now = utils::millis_since_unix_epoch();
		let last_last_active_ago = match last_presence {
			| Err(_) => 0_u64,
			| Ok((_, ref presence)) => now.saturating_sub(presence.last_active_ts),
		};

		if !state_changed && last_last_active_ago < REFRESH_TIMEOUT {
			return Ok(());
		}

		let status_msg = match last_presence {
			| Ok((_, ref presence)) => presence.status_msg.clone(),
			| Err(_) => None,
		};

		let last_active_ago = UInt::new(0);
		let currently_active = *new_state == PresenceState::Online;
		let _cork = self.services.db.cork();
		self.db
			.set_presence(
				user_id,
				new_state,
				Some(currently_active),
				last_active_ago,
				status_msg,
				last_presence.ok(),
			)
			.await?;

		self.schedule_timeout(user_id, new_state)?;

		Ok(())
	}

	/// Adds a presence event which will be saved until a new event replaces it.
	///
	/// External callers for APIs and federation. Previous state fetch if
	/// unknown.
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

		let _cork = self.services.db.cork();
		self.db
			.set_presence(
				user_id,
				presence_state,
				currently_active,
				last_active_ago,
				status_msg,
				None,
			)
			.await?;

		self.schedule_timeout(user_id, presence_state)?;

		Ok(())
	}

	/// Schedules a presence timeout timer for the given user if applicable.
	fn schedule_timeout(&self, user_id: &UserId, presence_state: &PresenceState) -> Result<()> {
		if (self.timeout_remote_users || self.services.globals.user_is_local(user_id))
			&& user_id != self.services.globals.server_user
		{
			let timeout = match presence_state {
				| PresenceState::Online => self.services.server.config.presence_idle_timeout_s,
				| _ => self.services.server.config.presence_offline_timeout_s,
			};

			self.timer_channel
				.0
				.send((user_id.to_owned(), Duration::from_secs(timeout)))
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
		info!("Resetting presence for local users...");
		let _cork = self.services.db.cork();
		let mut reset = 0_usize;

		for user_id in &self
			.services
			.users
			.list_local_users()
			.map(ToOwned::to_owned)
			.collect::<Vec<OwnedUserId>>()
			.await
		{
			let raw = self.db.get_presence_raw(user_id).await;

			let (count, presence) = match raw {
				| Ok((count, ref presence)) => (count, presence),
				| _ => continue,
			};

			if !matches!(
				presence.state,
				PresenceState::Unavailable | PresenceState::Online | PresenceState::Busy
			) {
				trace!(%user_id, ?presence, "Skipping user");
				continue;
			}

			let now = utils::millis_since_unix_epoch();
			let last_active_ago =
				UInt::new_saturating(now.saturating_sub(presence.last_active_ts));

			trace!(%user_id, ?presence, "Resetting presence to offline");

			if self
				.db
				.set_presence(
					user_id,
					&PresenceState::Offline,
					Some(false),
					Some(last_active_ago),
					presence.status_msg.clone(),
					Some((count, presence.clone())),
				)
				.await
				.inspect_err(|e| {
					debug_warn!(
						?presence,
						"{user_id} has invalid presence in database and failed to reset it to \
						 offline: {e}"
					);
				})
				.is_ok()
			{
				reset = reset.saturating_add(1);
			}
		}

		info!("Presence reset complete: {reset} users set to offline.");
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
		let mut previous = None;

		let raw = self.db.get_presence_raw(user_id).await;

		if let Ok((count, ref presence)) = raw {
			presence_state = presence.state.clone();
			let now = utils::millis_since_unix_epoch();
			last_active_ago =
				Some(UInt::new_saturating(now.saturating_sub(presence.last_active_ts)));
			status_msg = presence.status_msg.clone();
			previous = Some((count, presence.clone()));
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
			let _cork = self.services.db.cork();
			self.db
				.set_presence(
					user_id,
					&new_state,
					Some(false),
					last_active_ago,
					status_msg,
					previous,
				)
				.await?;

			self.schedule_timeout(user_id, &new_state)?;
		}

		Ok(())
	}
}

async fn presence_timer(
	user_id: OwnedUserId,
	timeout: Duration,
	created_at: Instant,
) -> (OwnedUserId, Instant) {
	// wait
	sleep(timeout).await;

	// return user and timer creation. calling loop will discard oldest
	(user_id, created_at)
}
