use async_trait::async_trait;
use oxide_auth::{
	endpoint::Scope,
	frontends::simple::extensions::AddonList,
	primitives::{
		prelude::{AuthMap, RandomGenerator},
		registrar::RegisteredUrl,
	},
};
use oxide_auth_async::{
	code_grant::{
		access_token::Endpoint as TokenEndpoint,
		authorization::Endpoint as AuthorizationEndpoint,
		client_credentials::Endpoint as CredentialsEndpoint,
		refresh::Endpoint as RefreshEndpoint, resource::Endpoint as ResourceEndpoint,
	},
	endpoint::{AccessTokenExtension, AuthorizationExtension, ClientCredentialsExtension},
	primitives::{Authorizer, Issuer, Registrar},
};
use url::Url;

mod issuer;
pub use issuer::OxideIssuer;
mod registrar;
pub use registrar::OxideRegistrar;

pub struct OxideEndpoint {
	/// Authorization codes are 16 byte random keys to a memory hash map.
	///
	/// Will be reinitialised on continuwuity's restart.
	pub authorizer: AuthMap<RandomGenerator>,
	pub registrar: OxideRegistrar,
	pub issuer: OxideIssuer,
	pub extension: AddonList,
	pub scopes: Vec<Scope>,
}

impl OxideEndpoint {
	pub(super) fn from_primitives(registrar: OxideRegistrar, issuer: OxideIssuer) -> Self {
		let authorizer = AuthMap::new(RandomGenerator::new(16));
		let extension = AddonList::new();
		let scopes = Vec::new();

		OxideEndpoint {
			authorizer,
			registrar,
			issuer,
			extension,
			scopes,
		}
	}

	/*
	pub(super) fn new(
		users: Dep<users::Service>,
		server_name: OwnedServerName,
		token_ttl: i64,
		refresh_ttl: i64,
		userdeviceid_oidcdevice: Arc<Map>,
		refreshtoken_userdeviceidexpiresat: Arc<Map>,
		clientid_oidcclient: Arc<Map>,
	) -> Self {
		OxideEndpoint {
			authorizer: AuthMap::new(RandomGenerator::new(16)),
			registrar: OxideRegistrar::new(clientid_oidcclient),
			issuer: OxideIssuer::new(
				server_name,
				token_ttl,
				refresh_ttl,
				refreshtoken_userdeviceidexpiresat,
				userdeviceid_oidcdevice,
				users,
			),
			extension: AddonList::new(),
			scopes: Vec::new(),
		}
	}
	*/
}

#[async_trait]
impl TokenEndpoint for OxideEndpoint {
	fn registrar(&self) -> &(dyn Registrar + Sync) { &self.registrar }

	fn authorizer(&mut self) -> &mut (dyn Authorizer + Send) { &mut self.authorizer }

	fn issuer(&mut self) -> &mut (dyn Issuer + Send) { &mut self.issuer }

	fn extension(&mut self) -> &mut (dyn AccessTokenExtension + Send) { &mut self.extension }
}

#[async_trait]
impl AuthorizationEndpoint for OxideEndpoint {
	fn registrar(&self) -> &(dyn Registrar + Sync) { &self.registrar }

	fn authorizer(&mut self) -> &mut (dyn Authorizer + Send) { &mut self.authorizer }

	fn extension(&mut self) -> &mut (dyn AuthorizationExtension + Send) { &mut self.extension }
}

#[async_trait]
impl CredentialsEndpoint for OxideEndpoint {
	fn registrar(&self) -> &(dyn Registrar + Sync) { &self.registrar }

	fn authorizer(&mut self) -> &mut (dyn Authorizer + Send) { &mut self.authorizer }

	fn issuer(&mut self) -> &mut (dyn Issuer + Send) { &mut self.issuer }

	fn extension(&mut self) -> &mut (dyn ClientCredentialsExtension + Send) {
		&mut self.extension
	}
}

#[async_trait]
impl RefreshEndpoint for OxideEndpoint {
	fn registrar(&self) -> &(dyn Registrar + Sync) { &self.registrar }

	fn issuer(&mut self) -> &mut (dyn Issuer + Send) { &mut self.issuer }
}

#[async_trait]
impl ResourceEndpoint for OxideEndpoint {
	fn scopes(&mut self) -> &[Scope] { &mut self.scopes }

	fn issuer(&mut self) -> &mut (dyn Issuer + Send) { &mut self.issuer }
}

/// Substitute "127.0.0.1" and "[::1]" for "localhost" to let oxide-auth compare
/// them ignoring their port.
fn normalize_redirect_hostname(url: Url) -> Url {
	let mut new_url = url.clone();
	let new_host = url.host_str().map(|h| {
		h.replace("127.0.0.1", "localhost")
			.replace("[::1]", "localhost")
	});
	new_url
		.set_host(new_host.as_deref())
		.expect("replaceable redirect hostname");

	new_url
}

/// If `url` is a localhost (either 'localhost', '127.0.0.1' or '[::1]'), wrap
/// it in an `IgnorePortOnLocalhost`, so that oxide-auth ignores the port when
/// comparing it with the registered ones.
pub fn normalize_redirect(url: Url) -> RegisteredUrl {
	let new_url = normalize_redirect_hostname(url);

	match new_url.host_str() {
		| Some("localhost") => RegisteredUrl::IgnorePortOnLocalhost(new_url.into()),
		| _ => RegisteredUrl::Semantic(new_url),
	}
}
