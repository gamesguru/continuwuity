use std::{borrow::Cow, sync::Arc};

use async_trait::async_trait;
use conduwuit::{Result, trace};
use database::{Deserialized, Json, Map};
use once_cell::sync::Lazy;
use oxide_auth::{
	endpoint::{PreGrant, Scope},
	primitives::{
		prelude::{Client, ClientUrl},
		registrar::{Argon2, BoundClient, EncodedClient, RegisteredClient, RegistrarError},
	},
};
use oxide_auth_async::primitives::Registrar;
use serde::{Deserialize, Serialize};

use super::normalize_redirect;

static PASSWORD_POLICY: Lazy<Argon2> = Lazy::new(Argon2::default);

/// A client app that connects to continuwuity via OIDC, as recorded in the
/// database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcClient {
	/// The name published by the app itself.
	pub name: Option<String>,
	/// The device's coordinates recorded by oxide-auth.
	pub client: EncodedClient,
}

pub struct OxideRegistrar {
	clientid_oidcclient: Arc<Map>,
}

impl OxideRegistrar {
	pub fn new(clientid_oidcclient: Arc<Map>) -> Self { OxideRegistrar { clientid_oidcclient } }

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

	/// Register an OIDC client in the database for future authentication flows.
	pub fn register_client(&self, display_name: Option<String>, client: &Client) {
		let client = client.clone().encode(&*PASSWORD_POLICY);
		let client_id = client.client_id.clone();
		let oidc_client = OidcClient { name: display_name, client };
		self.clientid_oidcclient.put(client_id, Json(oidc_client));
	}
}

/// Let this service act as an oxide-auth-async client app `Registrar`.
#[async_trait]
impl Registrar for OxideRegistrar {
	/// Determine the allowed redirect_uri for the client.
	async fn bound_redirect<'a>(
		&self,
		bound: ClientUrl<'a>,
	) -> Result<BoundClient<'a>, RegistrarError> {
		trace!(?bound, "registrar fetching client redirect");
		let Some(oidc_client) = self
			.get_client(bound.client_id.as_ref())
			.await
			.map_err(|_| RegistrarError::PrimitiveError)?
		else {
			return Err(RegistrarError::Unspecified);
		};
		trace!(?oidc_client, "registrar got oidc client");
		let client = oidc_client.client;

		// Perform exact matching as motivated in the rfc, but substitute
		// "127.0.0.1" and "[::1]" for "localhost" to let oxide-auth ignore
		// their port.
		let redirect_uri = bound.redirect_uri;
		let normalized_uri = redirect_uri.clone().map(|u| normalize_redirect(u.to_url()));
		let redirect_uri = match normalized_uri {
			| None => client.redirect_uri.clone(),
			| Some(url) => {
				let original = std::iter::once(&client.redirect_uri);
				let alternatives = client.additional_redirect_uris.iter();
				if original
					.chain(alternatives)
					.any(|registered| *registered == url)
				{
					// If normalized_uri is Some(url), so is redirect_uri, so unwrap().
					redirect_uri.unwrap().into_owned().into()
				} else {
					tracing::trace!(
						"the request's redirect url didn't match any registered. bound: {:?}, \
						 in client {:#?}",
						url,
						client
					);
					return Err(RegistrarError::Unspecified);
				}
			},
		};
		trace!(?redirect_uri, "registrar pushing redirect_uri");

		Ok(BoundClient {
			client_id: bound.client_id,
			redirect_uri: Cow::Owned(redirect_uri),
		})
	}

	/// Determine the allowed scope for the client.
	async fn negotiate<'a>(
		&self,
		bound: BoundClient<'a>,
		scope: Option<Scope>,
	) -> Result<PreGrant, RegistrarError> {
		trace!(?bound, ?scope, "registrar doing negociate");
		let Some(scope) = scope else {
			return Err(RegistrarError::Unspecified);
		};

		Ok(PreGrant {
			client_id: bound.client_id.into_owned(),
			redirect_uri: bound.redirect_uri.into_owned(),
			scope,
		})
	}

	/// Log the client in (not the user, not the device either). Currently
	/// limited to checking that the client is registered.
	async fn check(
		&self,
		client_id: &str,
		passphrase: Option<&[u8]>,
	) -> Result<(), RegistrarError> {
		trace!(?client_id, ?passphrase, "registrar doing client check");
		let Some(oidc_client) = self
			.get_client(client_id)
			.await
			.map_err(|_| RegistrarError::PrimitiveError)?
		else {
			return Err(RegistrarError::Unspecified);
		};

		trace!(?oidc_client, "registrar check passed");
		RegisteredClient::new(&oidc_client.client, &*PASSWORD_POLICY)
			.check_authentication(passphrase)
	}
}

#[async_trait]
impl Registrar for &OxideRegistrar {
	async fn bound_redirect<'a>(
		&self,
		bound: ClientUrl<'a>,
	) -> Result<BoundClient<'a>, RegistrarError> {
		self.bound_redirect(bound).await
	}

	async fn negotiate<'a>(
		&self,
		bound: BoundClient<'a>,
		scope: Option<Scope>,
	) -> Result<PreGrant, RegistrarError> {
		self.negotiate(bound, scope).await
	}

	async fn check(
		&self,
		client_id: &str,
		passphrase: Option<&[u8]>,
	) -> Result<(), RegistrarError> {
		self.check(client_id, passphrase).await
	}
}
