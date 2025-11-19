//! OIDC service.
//!
//! Provides the registrar, authorizer and issuer needed by [api::client::oidc].
//! The whole OIDC OAuth2 flow is taken care of by [oxide-auth].
//!
//! Summing up relations between OIDC (oxide-auth) ids and Matrix (ruma) ids:
//!
//! [ruma::DeviceId] is oidc_device_id
//! [ruma::UserId] is oidc_owner_id
//!
//! [oxide-auth]: https://docs.rs/oxide-auth

use std::sync::Arc;

use async_trait::async_trait;
use conduwuit::Result;
use futures::lock::Mutex;
use ruma::OwnedServerName;

use crate::users;

mod endpoint;
pub use endpoint::{OxideEndpoint, OxideIssuer, OxideRegistrar, normalize_redirect};

pub const SCOPE_PREFIX_DEVICE: &str = "urn:matrix:org.matrix.msc2967.client:device:";
pub const SCOPE_PREFIX_API: &str = "urn:matrix:org.matrix.msc2967.client:api:";

pub struct Service {
	pub endpoint: Arc<Mutex<OxideEndpoint>>,
	pub server_name: OwnedServerName,
	pub login_token_ttl: i64,
	pub refresh_token_ttl: i64,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let server_name = args.server.config.server_name.clone();
		let login_token_ttl = args.server.config.login_token_ttl as i64;
		let refresh_token_ttl = 7_200_000;
		let issuer = OxideIssuer::new(
			server_name.clone(),
			login_token_ttl,
			refresh_token_ttl,
			args.db["refreshtoken_userdeviceidexpiresat"].clone(),
			args.db["clientid_oidcclient"].clone(),
			args.db["userdeviceid_oidcdevice"].clone(),
			args.depend::<users::Service>("users"),
		);
		let registrar = OxideRegistrar::new(args.db["clientid_oidcclient"].clone());
		let endpoint = Arc::new(Mutex::new(OxideEndpoint::from_primitives(registrar, issuer)));
		Ok(Arc::new(Self {
			endpoint,
			server_name,
			login_token_ttl,
			refresh_token_ttl,
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
