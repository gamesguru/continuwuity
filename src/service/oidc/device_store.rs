use async_trait::async_trait;
use conduwuit::{Result, utils::ReadyExt};
use conduwuit_oidc::endpoint::DeviceStore;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};

use crate::{Dep, users};

/// Manage Matrix devices over Continuwuity's database through [users::Service].
pub struct DbDeviceStore(Dep<users::Service>);

impl DbDeviceStore {
	pub(crate) fn new(users: Dep<users::Service>) -> Self { Self(users) }
}

/// Let [crate::OidcIssuer] use [DbDeviceStore] as its [DeviceStore].
#[async_trait]
impl DeviceStore for DbDeviceStore {
	async fn exists(&self, user_id: &UserId, device_id: &DeviceId) -> bool {
		self.0
			.all_device_ids(user_id)
			.ready_any(|v| v == device_id)
			.await
	}

	async fn create(
		&mut self,
		user_id: &UserId,
		device_id: &DeviceId,
		access_token: &str,
		public_name: Option<String>,
		client_ip: Option<String>,
	) -> Result<()> {
		self.0
			.create_device(user_id, device_id, access_token, public_name, client_ip)
			.await
	}

	async fn remove(&mut self, user_id: &UserId, device_id: &DeviceId) {
		self.0.remove_device(user_id, device_id).await;
	}

	async fn generate_token(&self) -> String { self.0.generate_unique_token().await }

	async fn get_token(&self, user_id: &UserId, device_id: &DeviceId) -> Result<String> {
		self.0.get_token(user_id, device_id).await
	}

	async fn set_token(
		&mut self,
		user_id: &UserId,
		device_id: &DeviceId,
		token: &str,
	) -> Result<()> {
		self.0.set_token(user_id, device_id, token).await
	}

	async fn find(&self, token: &str) -> Result<(OwnedUserId, OwnedDeviceId)> {
		self.0.find_from_token(token).await
	}
}
