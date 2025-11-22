//! Authorization service for OIDC-aware Matrix clients.
//!
//! Provides the registrar, authorizer and issuer needed by the
//! [conduwuit_api::client::oidc] endpoints. The whole OIDC OAuth2 flows are
//! taken care of by [oxide-auth-async] to provide connection tokens to user
//! [OidcDevice]s that have registered themselves as an [OidcClient].
//!
//! [oxide-auth-async]: https://docs.rs/oxide-auth-async

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use conduwuit::Result;
use conduwuit_oidc::endpoint::{AsyncSolicitor, OidcEndpoint, OidcIssuer, OidcRegistrar};
use futures::lock::Mutex;
use ruma::OwnedServerName;

use crate::{state::State, users};

mod device_store;
use device_store::DbDeviceStore;

pub struct Service {
	pub endpoint: Arc<Mutex<OidcEndpoint<OidcRegistrar, OidcIssuer<DbDeviceStore>>>>,
	pub server_name: OwnedServerName,
	pub login_token_ttl: i64,
	pub refresh_token_ttl: i64,
	pub(crate) cookie_signing_key: Key,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let server_name = args.server.config.server_name.clone();
		let login_token_ttl = args.server.config.login_token_ttl as i64;
		let refresh_token_ttl = 7_200_000;
		let cookie_signing_key = Key::generate();
		let device_store = DbDeviceStore::new(args.depend::<users::Service>("users"));
		let issuer = OidcIssuer::new(
			server_name.clone(),
			login_token_ttl,
			refresh_token_ttl,
			args.db["refreshtoken_userdeviceidexpiresat"].clone(),
			args.db["clientid_oidcclient"].clone(),
			args.db["userdeviceid_oidcdevice"].clone(),
			device_store,
		);
		let registrar = OidcRegistrar::new(args.db["clientid_oidcclient"].clone());
		let solicitor = AsyncSolicitor { hostname: server_name.to_string() };
		let endpoint =
			Arc::new(Mutex::new(OidcEndpoint::from_primitives(registrar, issuer, solicitor)));

		Ok(Arc::new(Self {
			endpoint,
			server_name,
			login_token_ttl,
			refresh_token_ttl,
			cookie_signing_key,
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Let [cookie::SignedJar]s access their signing key.
impl FromRef<State> for Key {
	fn from_ref(services: &State) -> Self { services.oidc.cookie_signing_key.clone() }
}
