use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use conduwuit_core::{
	debug, err, trace,
	utils::{millis_since_unix_epoch, result::Result},
};
use conduwuit_database::{Deserialized, Json, Map};
use oxide_auth::{
	endpoint::Scope,
	primitives::{
		grant::{Extensions, Grant},
		issuer::{IssuedToken, RefreshedToken, TokenType},
		registrar::RegisteredUrl,
	},
};
use oxide_auth_async::primitives::Issuer;
use ruma::{DeviceId, OwnedDeviceId, OwnedServerName, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};

use super::{DeviceStore, SCOPE_PREFIX_DEVICE, normalize_redirect, registrar::OidcClient};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcDevice {
	pub client_id: String,
	pub scope: Scope,
	pub redirect_uri: RegisteredUrl,
	pub until: u64,
}

/// Bearer tokens are also random generated but 256-bit tokens, since they
/// live longer.
///
/// We could also use a `TokenSigner::ephemeral` here to create signed
/// tokens which can be read and parsed by anyone, but not maliciously
/// created. However, they can not be revoked and thus don't offer even
/// longer lived refresh tokens.
pub struct OidcIssuer<T>
where
	T: DeviceStore,
{
	pub server_name: OwnedServerName,
	pub token_ttl: i64,
	pub refresh_ttl: i64,
	/// Maps [String] refresh tokens to (([OwnedUserId], [OwnedDeviceId]),
	/// `expires_at`) where `expires_at` is an [i64] timestamp.
	refreshtoken_userdeviceidexpiresat: Arc<Map>,
	/// Maps client id [String]s to [OidcClient]s.
	clientid_oidcclient: Arc<Map>,
	/// Maps (&[UserId], &[DeviceId]) to their [OidcDevice].
	userdeviceid_oidcdevice: Arc<Map>,
	devices: T,
}

impl<T> OidcIssuer<T>
where
	T: DeviceStore,
{
	pub fn new(
		server_name: OwnedServerName,
		token_ttl: i64,
		refresh_ttl: i64,
		refreshtoken_userdeviceidexpiresat: Arc<Map>,
		clientid_oidcclient: Arc<Map>,
		userdeviceid_oidcdevice: Arc<Map>,
		devices: T,
	) -> Self {
		OidcIssuer {
			server_name,
			token_ttl,
			refresh_ttl,
			refreshtoken_userdeviceidexpiresat,
			clientid_oidcclient,
			userdeviceid_oidcdevice,
			devices,
		}
	}

	pub async fn get_client(&self, client_id: &str) -> Result<Option<OidcClient>> {
		if let Err(_) = self.clientid_oidcclient.exists(client_id).await {
			return Ok(None);
		}
		let client = self
			.clientid_oidcclient
			.get(client_id)
			.await?
			.deserialized()?;

		Ok(Some(client))
	}

	pub async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<OidcDevice> {
		// TODO return Result<Option<OidcDevice>>.
		self.userdeviceid_oidcdevice
			.qry(&(user_id, device_id))
			.await?
			.deserialized()
	}

	pub async fn revoke_device(&mut self, token: &str) -> Result<()> {
		let key = self.devices.find(token).await?;
		self.userdeviceid_oidcdevice.del(&key);

		Ok(())
	}
}

#[async_trait]
impl<T> Issuer for OidcIssuer<T>
where
	T: DeviceStore,
{
	async fn issue(&mut self, grant: Grant) -> Result<IssuedToken, ()> {
		debug!("issuing token for {grant:?}");
		let user_id = UserId::parse_with_server_name(&grant.owner_id, &self.server_name)
			.expect("valid owner id in grant");
		trace!("deserialising device id");
		let device_id = deviceid_from_scope(grant.scope.clone())
			.expect("valid device id from the grant's scope");
		let client = self
			.get_client(&grant.client_id)
			.await
			.map_err(|_| ())?
			.ok_or(())?;
		let access_token = self.devices.generate_token().await;
		let refresh_token = self.devices.generate_token().await;
		let until = Utc::now() + Duration::milliseconds(self.token_ttl);
		let refresh_until = Utc::now() + Duration::milliseconds(self.refresh_ttl);

		trace!("checking if the device is registered");
		// Register the device if not registered in the owner devices list.
		if !self.devices.exists(&user_id, &device_id).await {
			let oidc_device = OidcDevice {
				client_id: grant.client_id,
				scope: grant.scope,
				redirect_uri: normalize_redirect(grant.redirect_uri),
				// TODO deal with the possible overflow.
				until: until.timestamp_millis() as u64,
			};
			trace!("saving device metadata");
			self.devices
				.create(&user_id, &device_id, &access_token, client.name, None)
				.await
				.map_err(|_| ())?;
			trace!("saving device OIDC details");
			let key = (&user_id, &device_id);
			self.userdeviceid_oidcdevice.put(key, Json(oidc_device));
		}

		// Store the device's login token.
		trace!("saving device's access token");
		self.devices
			.set_token(&user_id, &device_id, &access_token)
			.await
			.map_err(|_| ())?;

		// Store the device's refresh token with the device id.
		trace!("saving device's refresh token");
		let value = ((user_id, device_id), refresh_until.timestamp_millis());
		self.refreshtoken_userdeviceidexpiresat
			.put(refresh_token.clone(), value);

		trace!("returning token");
		Ok(IssuedToken {
			token: access_token,
			refresh: Some(refresh_token),
			until,
			token_type: TokenType::Bearer,
		})
	}

	async fn refresh(&mut self, refresh: &str, grant: Grant) -> Result<RefreshedToken, ()> {
		let expected_device_id = deviceid_from_scope(grant.scope.clone()).map_err(|_| ())?;
		trace!("getting refresh token's expiration date");
		let ((user_id, device_id), expires_at): ((OwnedUserId, OwnedDeviceId), i64) = self
			.refreshtoken_userdeviceidexpiresat
			.get(refresh)
			.await
			.map_err(|_| ())?
			.deserialized()
			.map_err(|_| ())?;

		// Check the device id.
		if device_id != expected_device_id {
			debug!("the device ID doesn't match the one recorded");
			return Err(());
		}

		// Check the refresh token's expiration date.
		if (expires_at as u64) < millis_since_unix_epoch() {
			trace!(?user_id, ?device_id, ?refresh, "refresh token is expired, removing device");
			self.refreshtoken_userdeviceidexpiresat.remove(&refresh);
			self.devices.remove(&user_id, &device_id).await;
			return Err(());
		}

		let until = Utc::now() + Duration::milliseconds(self.token_ttl);

		// Replace the old token with a new one.
		let new_access = self.devices.generate_token().await;
		let new_refresh = self.devices.generate_token().await;
		self.devices
			.set_token(&user_id, &device_id, &new_access)
			.await
			.map_err(|_| ())?;

		// Store the device's refresh token with the device id.
		// TODO remove old refresh tokens.
		let refresh_until = Utc::now() + Duration::milliseconds(self.refresh_ttl);
		let value = ((user_id, device_id), refresh_until.timestamp_millis());
		self.refreshtoken_userdeviceidexpiresat
			.put(new_refresh.clone(), value);

		Ok(RefreshedToken {
			token: new_access,
			refresh: Some(new_refresh),
			until,
			token_type: TokenType::Bearer,
		})
	}

	async fn recover_token<'a>(&'a mut self, token: &str) -> Result<Option<Grant>, ()> {
		let (user_id, device_id) = self.devices.find(token).await.map_err(|_| ())?;
		let device = self
			.get_device(&user_id, &device_id)
			.await
			.map_err(|_| ())?;

		// Check that the device is not expired.
		if device.until < millis_since_unix_epoch() {
			trace!(?user_id, ?device_id, ?token, "removing expired device");
			self.devices.remove(&user_id, &device_id).await;
			return Err(());
		}

		// TODO the cast as i64 could overflow, deal with it.
		let until =
			DateTime::from_timestamp_millis(device.until as i64).expect("some valid timestamp");
		let grant = Grant {
			owner_id: user_id.to_string(),
			client_id: device.client_id,
			scope: device.scope,
			redirect_uri: device.redirect_uri.to_url(),
			until,
			// TODO understand what extensions are.
			extensions: Extensions::new(),
		};

		Ok(Some(grant))
	}

	async fn recover_refresh<'a>(&'a mut self, refresh: &str) -> Result<Option<Grant>, ()> {
		// First check that the token exists.
		let ((user_id, device_id), expires_at): ((OwnedUserId, OwnedDeviceId), i64) = self
			.refreshtoken_userdeviceidexpiresat
			.get(refresh)
			.await
			.map_err(|_| ())?
			.deserialized()
			.map_err(|_| ())?;
		let device = self
			.get_device(&user_id, &device_id)
			.await
			.map_err(|_| ())?;

		// Then check that it's not expired.
		if (expires_at as u64) < millis_since_unix_epoch() {
			trace!(?device, ?refresh, "removing expired device refresh token");
			self.devices.remove(&user_id, &device_id).await;
			return Err(());
		}

		// TODO the cast as i64 could overflow, deal with it.
		let until =
			DateTime::from_timestamp_millis(device.until as i64).expect("some valid timestamp");
		let grant = Grant {
			owner_id: user_id.to_string(),
			client_id: device.client_id,
			scope: device.scope,
			redirect_uri: device.redirect_uri.to_url(),
			until,
			extensions: Extensions::new(),
		};

		Ok(Some(grant.into()))
	}
}

fn deviceid_from_scope(scope: Scope) -> Result<OwnedDeviceId> {
	let Some(device_id) = scope.iter().find(|s| s.starts_with(SCOPE_PREFIX_DEVICE)) else {
		tracing::warn!("device_id not found in scope {scope:?}");
		return Err(err!(Request(InvalidParam("something went wrong with the scope"))));
	};
	let device_id = device_id.replace(SCOPE_PREFIX_DEVICE, "");

	OwnedDeviceId::try_from(device_id.clone())
		.map_err(|err| err!(Request(InvalidParam("invalid device_id {device_id:?}: {err}"))))
}
