use async_trait::async_trait;
use conduwuit_core::Result;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};

#[async_trait]
pub trait DeviceStore: Send + Sync + 'static {
	async fn exists(&self, user_id: &UserId, device_id: &DeviceId) -> bool;
	async fn create(
		&mut self,
		user_id: &UserId,
		device_id: &DeviceId,
		access_token: &str,
		public_name: Option<String>,
		client_ip: Option<String>,
	) -> Result<()>;
	async fn remove(&mut self, user_id: &UserId, device_id: &DeviceId);
	async fn generate_token(&self) -> String;
	async fn get_token(&self, user_id: &UserId, device_id: &DeviceId) -> Result<String>;
	async fn set_token(
		&mut self,
		user_id: &UserId,
		device_id: &DeviceId,
		token: &str,
	) -> Result<()>;
	async fn find(&self, token: &str) -> Result<(OwnedUserId, OwnedDeviceId)>;
}
