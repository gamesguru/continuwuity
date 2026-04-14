mod data;

use std::{collections::HashMap, fmt::Write, sync::Arc, time::Instant};

use async_trait::async_trait;
use conduwuit::{Result, Server, SyncRwLock, error, utils::bytes::pretty};
use data::Data;
use regex::RegexSet;
use ruma::{OwnedEventId, OwnedRoomAliasId, OwnedServerName, OwnedUserId, ServerName, UserId};

use crate::service;

pub struct Service {
	pub db: Data,
	server: Arc<Server>,

	pub bad_event_ratelimiter: Arc<SyncRwLock<HashMap<OwnedEventId, RateLimitState>>>,
	pub server_user: OwnedUserId,
	pub admin_alias: OwnedRoomAliasId,
	pub turn_secret: String,
}

type RateLimitState = (Instant, u32); // Time if last failed try, number of failed tries

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let db = Data::new(&args);
		let config = &args.server.config;

		let turn_secret = config.turn_secret_file.as_ref().map_or_else(
			|| config.turn_secret.clone(),
			|path| match std::fs::read_to_string(path) {
				| Ok(secret) => secret.trim().to_owned(),
				| Err(e) => {
					error!("Failed to read the TURN secret file: {e}");

					config.turn_secret.clone()
				},
			},
		);

		Ok(Arc::new(Self {
			db,
			server: args.server.clone(),
			bad_event_ratelimiter: Arc::new(SyncRwLock::new(HashMap::new())),
			admin_alias: OwnedRoomAliasId::try_from(format!("#admins:{}", &args.server.name))
				.expect("#admins:server_name is valid alias name"),
			server_user: UserId::parse_with_server_name(
				String::from("conduit"),
				&args.server.name,
			)
			.expect("@conduit:server_name is valid"),
			turn_secret,
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let (ber_count, ber_bytes) = self.bad_event_ratelimiter.read().iter().fold(
			(0_usize, 0_usize),
			|(mut count, mut bytes), (event_id, _)| {
				bytes = bytes.saturating_add(event_id.capacity());
				bytes = bytes.saturating_add(size_of::<RateLimitState>());
				count = count.saturating_add(1);
				(count, bytes)
			},
		);

		writeln!(out, "bad_event_ratelimiter: {ber_count} ({})", pretty(ber_bytes))?;

		Ok(())
	}

	async fn clear_cache(&self) { self.bad_event_ratelimiter.write().clear(); }

	async fn worker(self: Arc<Self>) -> Result<()> {
		let mut interval = tokio::time::interval(std::time::Duration::from_secs(60)); // 1 min

		let mut last_http_success = 0;
		let mut last_http_fail = 0;
		let mut last_http_time = 0;

		let mut last_dns_success = 0;
		let mut last_dns_fail = 0;
		let mut last_dns_time = 0;

		let mut last_transactions = 0;

		let mut shutdown = self.server.signal.subscribe();

		loop {
			tokio::select! {
				_ = interval.tick() => {},
				_ = shutdown.recv() => {},
			}
			if !self.server.running() {
				break;
			}

			let http_success = self
				.server
				.metrics
				.requests_success
				.load(std::sync::atomic::Ordering::Relaxed);
			let http_fail = self
				.server
				.metrics
				.requests_fail
				.load(std::sync::atomic::Ordering::Relaxed);
			let http_time = self
				.server
				.metrics
				.requests_time
				.load(std::sync::atomic::Ordering::Relaxed);

			let dns_success = self
				.server
				.metrics
				.dns_requests_success
				.load(std::sync::atomic::Ordering::Relaxed);
			let dns_fail = self
				.server
				.metrics
				.dns_requests_fail
				.load(std::sync::atomic::Ordering::Relaxed);
			let dns_time = self
				.server
				.metrics
				.dns_requests_time
				.load(std::sync::atomic::Ordering::Relaxed);

			let d_http_success = http_success.saturating_sub(last_http_success);
			let d_http_fail = http_fail.saturating_sub(last_http_fail);
			let d_http_time_us = http_time.saturating_sub(last_http_time);
			let d_http_total = d_http_success.saturating_add(d_http_fail);

			self.server
				.metrics
				.requests_rate_1m
				.store(d_http_total, std::sync::atomic::Ordering::Relaxed);

			let (http_avg_latency_ms, http_fail_rate) = {
				#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
				if d_http_total > 0 {
					(
						(d_http_time_us as f64 / d_http_total as f64) / 1000.0,
						(d_http_fail as f64 / d_http_total as f64) * 100.0,
					)
				} else {
					(0.0, 0.0)
				}
			};

			let d_dns_success = dns_success.saturating_sub(last_dns_success);
			let d_dns_fail = dns_fail.saturating_sub(last_dns_fail);
			let d_dns_time_us = dns_time.saturating_sub(last_dns_time);
			let d_dns_total = d_dns_success.saturating_add(d_dns_fail);

			self.server
				.metrics
				.dns_rate_1m
				.store(d_dns_total, std::sync::atomic::Ordering::Relaxed);

			let (dns_avg_latency_ms, dns_fail_rate) = {
				#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
				if d_dns_total > 0 {
					(
						(d_dns_time_us as f64 / d_dns_total as f64) / 1000.0,
						(d_dns_fail as f64 / d_dns_total as f64) * 100.0,
					)
				} else {
					(0.0, 0.0)
				}
			};

			let transactions = self
				.server
				.metrics
				.transactions_processed
				.load(std::sync::atomic::Ordering::Relaxed);
			let d_transactions = transactions.saturating_sub(last_transactions);
			self.server
				.metrics
				.transactions_rate_1m
				.store(d_transactions, std::sync::atomic::Ordering::Relaxed);

			let presence = self
				.server
				.metrics
				.presence_pending_updates
				.load(std::sync::atomic::Ordering::Relaxed);
			let backfill = self
				.server
				.metrics
				.federation_active_rooms
				.load(std::sync::atomic::Ordering::Relaxed);
			let sending = self
				.server
				.metrics
				.sending_queue_total
				.load(std::sync::atomic::Ordering::Relaxed);

			conduwuit::info!(
				target: "stats",
				"Network stats (Last 1m) - HTTP Router: {} reqs ({:.2}% fail, {:.2}ms avg \
				 latency) | DNS Resolver: {} reqs ({:.2}% fail, {:.2}ms avg latency) | Fed \
				 Txns: {} | Background: {} pres, {} bfill, {} send",
				d_http_total,
				http_fail_rate,
				http_avg_latency_ms,
				d_dns_total,
				dns_fail_rate,
				dns_avg_latency_ms,
				d_transactions,
				presence,
				backfill,
				sending
			);

			last_http_success = http_success;
			last_http_fail = http_fail;
			last_http_time = http_time;

			last_dns_success = dns_success;
			last_dns_fail = dns_fail;
			last_dns_time = dns_time;

			last_transactions = transactions;
		}

		Ok(())
	}

	fn name(&self) -> &str { service::make_name(std::module_path!()) }
}

impl Service {
	#[inline]
	pub fn next_count(&self) -> Result<u64> { self.db.next_count() }

	pub fn next_count_batch(&self, diff: u64) -> Result<u64> { self.db.next_count_batch(diff) }

	#[inline]
	pub fn current_count(&self) -> Result<u64> { Ok(self.db.current_count()) }

	#[inline]
	pub fn server_name(&self) -> &ServerName { self.server.name.as_ref() }

	pub fn allow_public_room_directory_over_federation(&self) -> bool {
		self.server
			.config
			.allow_public_room_directory_over_federation
	}

	pub fn allow_device_name_federation(&self) -> bool {
		self.server.config.allow_device_name_federation
	}

	pub fn allow_room_creation(&self) -> bool { self.server.config.allow_room_creation }

	pub fn new_user_displayname_suffix(&self) -> &String {
		&self.server.config.new_user_displayname_suffix
	}

	pub fn allow_announcements_check(&self) -> bool {
		self.server.config.allow_announcements_check
	}

	pub fn trusted_servers(&self) -> &[OwnedServerName] { &self.server.config.trusted_servers }

	pub fn turn_password(&self) -> &String { &self.server.config.turn_password }

	pub fn turn_ttl(&self) -> u64 { self.server.config.turn_ttl }

	pub fn turn_uris(&self) -> &[String] { &self.server.config.turn_uris }

	pub fn turn_username(&self) -> &String { &self.server.config.turn_username }

	pub fn notification_push_path(&self) -> &String { &self.server.config.notification_push_path }

	pub fn url_preview_domain_contains_allowlist(&self) -> &Vec<String> {
		&self.server.config.url_preview_domain_contains_allowlist
	}

	pub fn url_preview_domain_explicit_allowlist(&self) -> &Vec<String> {
		&self.server.config.url_preview_domain_explicit_allowlist
	}

	pub fn url_preview_domain_explicit_denylist(&self) -> &Vec<String> {
		&self.server.config.url_preview_domain_explicit_denylist
	}

	pub fn url_preview_url_contains_allowlist(&self) -> &Vec<String> {
		&self.server.config.url_preview_url_contains_allowlist
	}

	pub fn url_preview_max_spider_size(&self) -> usize {
		self.server.config.url_preview_max_spider_size
	}

	pub fn url_preview_check_root_domain(&self) -> bool {
		self.server.config.url_preview_check_root_domain
	}

	pub fn url_preview_allow_audio_video(&self) -> bool {
		self.server.config.url_preview_allow_audio_video
	}

	pub fn forbidden_alias_names(&self) -> &RegexSet { &self.server.config.forbidden_alias_names }

	pub fn forbidden_usernames(&self) -> &RegexSet { &self.server.config.forbidden_usernames }

	/// checks if `user_id` is local to us via server_name comparison
	#[inline]
	pub fn user_is_local(&self, user_id: &UserId) -> bool {
		self.server_is_ours(user_id.server_name())
	}

	#[inline]
	pub fn server_is_ours(&self, server_name: &ServerName) -> bool {
		server_name == self.server_name() || server_name.host() == self.server_name().host()
	}
}
