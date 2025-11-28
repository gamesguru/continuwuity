use axum::async_trait;
use oxide_auth::{
	endpoint::{OAuthError, Scope, Scopes, Template, WebRequest},
	frontends::simple::extensions::AddonList,
	primitives::prelude::{AuthMap, RandomGenerator},
};
use oxide_auth_async::{
	endpoint::{Endpoint, Extension, OwnerSolicitor},
	primitives::{Authorizer, Issuer, Registrar},
};

use crate::{OidcError, OidcRequest, OidcResponse, endpoint::AsyncSolicitor};

/// The oxide-auth-async OIDC flows endpoint, defined over any registrar and
/// issuer types.
///
/// Will be used to create :
///   - authentication flows with
///     [oxide_auth_async::endpoint::authorization::AuthorizationFlow]
///   - access token flows with
///     [oxide_auth_async::endpoint::access_token::AccessTokenFlow]
///   - refresh token flows with
///     [oxide_auth_async::endpoint::refresh::RefreshFlow]
///
/// See [conduwuit_api::client::oidc] functions for usage examples.
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
