mod acl_check;
pub(crate) mod extremities;
mod fetch_and_handle_outliers;
mod fetch_prev;
mod fetch_state;
mod handle_incoming_pdu;
mod handle_outlier_pdu;
mod handle_prev_pdu;
pub(crate) mod healer;
mod parse_incoming_pdu;
mod policy_server;
mod pre_fetch_state_res_deps;
mod resolve_state;
mod state_at_incoming;
pub mod upgrade_outlier_pdu;

use std::{
	collections::{BTreeMap, HashMap},
	fmt::Write,
	sync::{
		Arc,
		atomic::{AtomicU32, Ordering},
	},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use conduwuit::{
	Err, Event, PduEvent, Result, RoomVersion, Server, SyncRwLock,
	utils::{MutexMap, stream::ReadyExt},
};
use futures::StreamExt;
use ruma::{
	CanonicalJsonValue, OwnedEventId, OwnedRoomId, OwnedServerName, RoomId, RoomVersionId,
	events::room::create::RoomCreateEventContent,
};

use crate::{Dep, globals, rooms, sending, server_keys};

pub struct Service {
	pub mutex_federation: RoomMutexMap,
	pub federation_handletime: SyncRwLock<HandleTimeMap>,
	pub bad_room_ratelimiter: SyncRwLock<HashMap<OwnedRoomId, (u32, Instant)>>,
	pub peer_scorer: dashmap::DashMap<OwnedServerName, PeerStats>,
	pub dag_healer: tokio::sync::mpsc::UnboundedSender<HealRequest>,
	dag_healer_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<HealRequest>>>,
	pub timeline_worker_tx:
		dashmap::DashMap<OwnedRoomId, tokio::sync::mpsc::UnboundedSender<PduUpgradeRequest>>,
	services: Services,
}

/// A PDU that was stored as an outlier because its auth events were missing.
/// Carried by `HealRequest::MissingState` so the healer can retry it after
/// fetching the remote state via /state_ids.
#[derive(Debug)]
pub struct WaitingPdu {
	pub event_id: OwnedEventId,
	pub value: BTreeMap<String, CanonicalJsonValue>,
	pub origin: OwnedServerName,
}

#[derive(Debug)]
pub struct PduUpgradeRequest {
	pub incoming_pdu: PduEvent,
	pub val: BTreeMap<String, CanonicalJsonValue>,
	pub create_event: PduEvent,
	pub origin: OwnedServerName,
	pub room_id: OwnedRoomId,
	pub response_tx: tokio::sync::oneshot::Sender<conduwuit::Result<()>>,
}

#[derive(Debug)]
pub enum HealRequest {
	MissingEvents {
		room_id: OwnedRoomId,
		missing_events: Vec<OwnedEventId>,
	},
	MissingState {
		room_id: OwnedRoomId,
		/// The event to call /state_ids at (usually the prev_event of
		/// waiting_pdu).
		event_id: OwnedEventId,
		origin: OwnedServerName,
		/// If set, after fetching state the healer will also fetch `event_id`
		/// directly and then retry handle_incoming_pdu for this PDU.
		waiting_pdu: Option<Box<WaitingPdu>>,
	},
	UpdateTimeline(Box<PduUpgradeRequest>),
}

#[derive(Default, Debug)]
pub struct PeerStats {
	pub successes: AtomicU32,
	pub errors: AtomicU32,
	pub latency_ms: AtomicU32,
}

struct Services {
	globals: Dep<globals::Service>,
	sending: Dep<sending::Service>,
	auth_chain: Dep<rooms::auth_chain::Service>,
	metadata: Dep<rooms::metadata::Service>,
	outlier: Dep<rooms::outlier::Service>,
	pdu_metadata: Dep<rooms::pdu_metadata::Service>,
	server_keys: Dep<server_keys::Service>,
	short: Dep<rooms::short::Service>,
	state: Dep<rooms::state::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	state_compressor: Dep<rooms::state_compressor::Service>,
	timeline: Dep<rooms::timeline::Service>,
	server: Arc<Server>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
type HandleTimeMap = HashMap<OwnedRoomId, (OwnedEventId, Instant)>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

		let service = Arc::new(Self {
			mutex_federation: RoomMutexMap::new(),
			federation_handletime: HandleTimeMap::new().into(),
			bad_room_ratelimiter: HashMap::new().into(),
			peer_scorer: dashmap::DashMap::new(),
			dag_healer: sender,
			dag_healer_rx: std::sync::Mutex::new(Some(receiver)),
			timeline_worker_tx: dashmap::DashMap::new(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				sending: args.depend::<sending::Service>("sending"),
				auth_chain: args.depend::<rooms::auth_chain::Service>("rooms::auth_chain"),
				metadata: args.depend::<rooms::metadata::Service>("rooms::metadata"),
				outlier: args.depend::<rooms::outlier::Service>("rooms::outlier"),
				server_keys: args.depend::<server_keys::Service>("server_keys"),
				pdu_metadata: args.depend::<rooms::pdu_metadata::Service>("rooms::pdu_metadata"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
				server: args.server.clone(),
			},
		});

		Ok(service)
	}

	async fn worker(self: Arc<Self>) -> Result<()> {
		let receiver = self.dag_healer_rx.lock().unwrap().take();
		if let Some(receiver) = receiver {
			healer::healer_worker(receiver, self.clone()).await;
		}
		Ok(())
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_federation = self.mutex_federation.len();
		writeln!(out, "federation_mutex: {mutex_federation}")?;

		let federation_handletime = self.federation_handletime.read().len();
		writeln!(out, "federation_handletime: {federation_handletime}")?;

		let peer_scorer = self.peer_scorer.len();
		writeln!(out, "peer_scorer: {peer_scorer}")?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	async fn event_fetch(
		&self,
		room_id: Option<&RoomId>,
		event_id: OwnedEventId,
	) -> Option<PduEvent> {
		if let Ok(pdu) = self
			.services
			.timeline
			.get_pdu_in_room(room_id, &event_id)
			.await
		{
			Some(pdu)
		} else {
			self.services.outlier.get_pdu_outlier(&event_id).await.ok()
		}
	}

	/// Build a prioritized list of federation servers for fetching events:
	///  1. origin (the server that sent the transaction)
	///  2. trusted/notary servers (from config)
	///  3. room member servers (capped by room_server_cap)
	pub async fn build_federation_server_list(
		&self,
		room_id: &RoomId,
		origin: &ruma::ServerName,
		room_server_cap: usize,
	) -> Vec<OwnedServerName> {
		let mut servers: Vec<OwnedServerName> = vec![origin.to_owned()];
		for s in &self.services.server.config.trusted_servers {
			if !self.services.globals.server_is_ours(s) && !servers.contains(s) {
				servers.push(s.clone());
			}
		}
		let mut room_servers: Vec<OwnedServerName> = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|s| {
				!self.services.globals.server_is_ours(s) && !servers.iter().any(|x| x == s)
			})
			.map(ToOwned::to_owned)
			.collect()
			.await;

		// Sort by peer score: lower is better (latency + errors*1000 - successes*100)
		// i64 allows healthy servers to score negative, ranking above unseen peers
		room_servers.sort_unstable_by_key(|s| {
			if let Some(stats) = self.peer_scorer.get(s) {
				let success = i64::from(stats.successes.load(Ordering::Relaxed));
				let error = i64::from(stats.errors.load(Ordering::Relaxed));
				let latency = i64::from(stats.latency_ms.load(Ordering::Relaxed));
				latency
					.saturating_add(error.saturating_mul(1000))
					.saturating_sub(success.saturating_mul(100))
			} else {
				// Default penalization for unknown servers (assume 5s latency)
				5000_i64
			}
		});

		room_servers.truncate(room_server_cap);
		servers.extend(room_servers);
		servers
	}

	pub fn update_peer_stats(&self, server: &ruma::ServerName, success: bool, latency: Duration) {
		let latency_ms = u32::try_from(latency.as_millis()).unwrap_or(u32::MAX);
		let stats = self.peer_scorer.entry(server.to_owned()).or_default();
		if success {
			stats.successes.fetch_add(1, Ordering::Relaxed);
			let old = stats.latency_ms.load(Ordering::Relaxed);
			let new_latency = if old == 0 {
				// First measurement — seed directly rather than blending with zero
				latency_ms
			} else {
				old.saturating_mul(7)
					.saturating_add(latency_ms.saturating_mul(3))
					/ 10
			};
			stats.latency_ms.store(new_latency, Ordering::Relaxed);
		} else {
			stats.errors.fetch_add(1, Ordering::Relaxed);
		}
	}
}

fn check_room_id<Pdu: Event>(room_id: &RoomId, pdu: &Pdu) -> Result {
	// room_id_or_hash() returns None only for v12 create events where room_id
	// is derived from the event_id. All other events must have room_id.
	// If room_id is missing on a non-create event, the stored JSON is corrupt
	// but we still proceed rather than blocking the entire auth chain.
	if let Some(pdu_room_id) = pdu.room_id_or_hash() {
		if *pdu_room_id != *room_id {
			return Err!(Request(InvalidParam(error!(
				pdu_event_id = %pdu.event_id(),
				pdu_room_id = %pdu_room_id,
				pdu_sender = %pdu.sender(),
				pdu_event_type = %pdu.event_type(),
				expected_room_id = %room_id,
				"PDU room_id mismatch: event belongs to a different room than expected",
			))));
		}
	}

	Ok(())
}

fn get_room_version_id<Pdu: Event>(create_event: &Pdu) -> Result<RoomVersionId> {
	let content: RoomCreateEventContent = create_event.get_content()?;
	let room_version = content.room_version;

	Ok(room_version)
}

#[inline]
fn to_room_version(room_version_id: &RoomVersionId) -> RoomVersion {
	RoomVersion::new(room_version_id).expect("room version is supported")
}
