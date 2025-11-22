use axum::async_trait;
use oxide_auth::{
	endpoint::{OAuthError, Scope, Scopes, Template, WebRequest},
	frontends::simple::extensions::AddonList,
	primitives::{
		prelude::{AuthMap, RandomGenerator},
		registrar::RegisteredUrl,
	},
};
use oxide_auth_async::{
	endpoint::{Endpoint, Extension, OwnerSolicitor},
	primitives::{Authorizer, Issuer, Registrar},
};
use url::Url;

mod device_store;
pub use device_store::DeviceStore;
mod registrar;
pub use registrar::{OidcClient, OidcRegistrar};
mod solicitor;
pub use solicitor::AsyncSolicitor;
mod issuer;
pub use issuer::{OidcDevice, OidcIssuer};

use crate::{OidcError, OidcRequest, OidcResponse};

pub const SCOPE_PREFIX_DEVICE: &str = "urn:matrix:org.matrix.msc2967.client:device:";
pub const SCOPE_PREFIX_API: &str = "urn:matrix:org.matrix.msc2967.client:api:";

pub struct OidcEndpoint<R, I>
where
	R: Registrar,
	I: Issuer,
{
	pub authorizer: AuthMap<RandomGenerator>,
	pub registrar: R,
	pub issuer: I,
	pub solicitor: AsyncSolicitor,
	pub extension: AddonList,
	pub scopes: Vec<Scope>,
}

impl<R, I> OidcEndpoint<R, I>
where
	R: Registrar + Sync + Send,
	I: Issuer + Sync + Send,
{
	pub fn from_primitives(registrar: R, issuer: I, solicitor: AsyncSolicitor) -> Self {
		let authorizer = AuthMap::new(RandomGenerator::new(16));
		let extension = AddonList::new();
		let scopes = Vec::new();

		OidcEndpoint {
			authorizer,
			registrar,
			issuer,
			solicitor,
			extension,
			scopes,
		}
	}
}

#[async_trait]
impl<R, I> Endpoint<OidcRequest> for &mut OidcEndpoint<R, I>
where
	R: Registrar + Sync + Send,
	I: Issuer + Sync + Send,
{
	type Error = OidcError;

	fn registrar(&self) -> Option<&(dyn Registrar + Sync)> { Some(&self.registrar) }

	fn authorizer_mut(&mut self) -> Option<&mut (dyn Authorizer + Send)> {
		Some(&mut self.authorizer)
	}

	fn issuer_mut(&mut self) -> Option<&mut (dyn Issuer + Send)> { Some(&mut self.issuer) }

	fn owner_solicitor(&mut self) -> Option<&mut (dyn OwnerSolicitor<OidcRequest> + Send)> {
		Some(&mut self.solicitor)
	}

	fn scopes(&mut self) -> Option<&mut dyn Scopes<OidcRequest>> { Some(&mut self.scopes) }

	fn response(
		&mut self,
		_request: &mut OidcRequest,
		_kind: Template<'_>,
	) -> Result<<OidcRequest as WebRequest>::Response, Self::Error> {
		// TODO check.
		Ok(OidcResponse::default())
	}

	fn error(&mut self, err: OAuthError) -> Self::Error {
		match err {
			| OAuthError::DenySilently => OidcError::Authorization,
			| OAuthError::BadRequest => OidcError::Encoding,
			| OAuthError::PrimitiveError => OidcError::InternalError(None),
		}
	}

	fn web_error(&mut self, err: OidcError) -> Self::Error { err }

	fn extension(&mut self) -> Option<&mut (dyn Extension + Send)> { None }
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
