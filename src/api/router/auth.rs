use std::any::{Any, TypeId};

use conduwuit::{Err, Result, err};
use ruma::{
	OwnedDeviceId, OwnedServerName, OwnedUserId, UserId,
	api::{
		IncomingRequest,
		auth_scheme::{
			AccessToken, AccessTokenOptional, AppserviceToken, AppserviceTokenOptional,
			AuthScheme, NoAccessToken, NoAuthentication,
		},
		federation::authentication::ServerSignatures,
	},
};
use service::Services;

use crate::{router::args::AuthQueryParams, service::appservice::RegistrationInfo};

#[derive(Default)]
pub(super) struct Auth {
	pub(super) origin: Option<OwnedServerName>,
	pub(super) sender_user: Option<OwnedUserId>,
	pub(super) sender_device: Option<OwnedDeviceId>,
	pub(super) appservice_info: Option<RegistrationInfo>,
}

pub(super) trait CheckAuth: AuthScheme {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send;

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		services: &Services,
		output: Self::Output,
		request: &hyper::Request<B>,
		query: AuthQueryParams,
		route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send;
}

impl CheckAuth for ServerSignatures {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let route = TypeId::of::<R>();

			let output = Self::extract_authentication(incoming_request).map_err(|err| {
				err!(Request(Unauthorized(warn!("Failed to extract signatures: {:?}", err))))
			})?;

			Self::verify(services, output, incoming_request, query, route).await
		}
	}

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		services: &Services,
		output: Self::Output,
		request: &hyper::Request<B>,
		_query: AuthQueryParams,
		_route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			use service::server_keys::{PubKeyMap, PubKeys};

			let destination = services.globals.server_name();
			if output
				.destination
				.as_ref()
				.is_some_and(|supplied_destination| supplied_destination != destination)
			{
				return Err!(Request(Unauthorized("Destination mismatch.")));
			}

			let key = services
				.server_keys
				.get_verify_key(&output.origin, &output.key)
				.await
				.map_err(|e| {
					err!(Request(Unauthorized(warn!("Failed to fetch signing keys: {e}"))))
				})?;

			let keys: PubKeys = [(output.key.to_string(), key.key)].into();
			let keys: PubKeyMap = [(output.origin.as_str().into(), keys)].into();

			match output.verify_request(request, destination, &keys) {
				| Ok(()) => Ok(Auth {
					origin: Some(output.origin.clone()),
					..Default::default()
				}),
				| Err(err) =>
					Err!(Request(Unauthorized(warn!("Failed to verify X-Matrix header: {err}")))),
			}
		}
	}
}

impl CheckAuth for AccessToken {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let route = TypeId::of::<R>();

			let output = Self::extract_authentication(incoming_request).map_err(|err| {
				let err_str = format!("{err:?}");
				if err_str.contains("NoToken") || err_str.contains("Missing") {
					err!(Request(MissingToken(
						"No access token found, but this endpoint requires one."
					)))
				} else {
					err!(Request(Unauthorized(warn!(
						"Failed to extract authorization: {:?}",
						err
					))))
				}
			})?;

			Self::verify(services, output, incoming_request, query, route).await
		}
	}

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		services: &Services,
		output: Self::Output,
		_request: &hyper::Request<B>,
		query: AuthQueryParams,
		route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			// Check for user tokens first
			if let Ok((sender_user, sender_device)) =
				services.users.find_from_token(&output).await
			{
				// Locked users can only use /logout and /logout/all
				if services
					.users
					.is_locked(&sender_user)
					.await
					.is_ok_and(std::convert::identity)
				{
					if !(route == TypeId::of::<ruma::api::client::session::logout::v3::Request>()
						|| route
							== TypeId::of::<ruma::api::client::session::logout_all::v3::Request>(
							)) {
						return Err!(Request(Unauthorized("Your account is locked.")));
					}
				}

				return Ok(Auth {
					sender_user: Some(sender_user),
					sender_device: Some(sender_device),
					..Default::default()
				});
			}

			// Fall back to appservice tokens
			if let Ok(appservice_info) = services.appservice.find_from_token(&output).await {
				let Ok(sender_user) = query.user_id.clone().map_or_else(
					|| {
						UserId::parse_with_server_name(
							appservice_info.registration.sender_localpart.as_str(),
							services.globals.server_name(),
						)
					},
					UserId::parse,
				) else {
					return Err!(Request(InvalidUsername("Username is invalid.")));
				};

				if !appservice_info.is_user_match(&sender_user) {
					return Err!(Request(Exclusive("User is not in namespace.")));
				}

				// MSC3202/MSC4190: Handle device_id masquerading for appservices.
				let sender_device =
					if let Some(device_id) = query.device_id.as_deref().map(Into::into) {
						if services
							.users
							.get_device_metadata(&sender_user, device_id)
							.await
							.is_err()
						{
							return Err!(Request(Forbidden(
								"Device does not exist for user or appservice cannot masquerade \
								 as this device."
							)));
						}

						Some(device_id.to_owned())
					} else {
						None
					};

				return Ok(Auth {
					sender_user: Some(sender_user),
					sender_device,
					appservice_info: Some(appservice_info),
					..Default::default()
				});
			}

			Err!(Request(UnknownToken("Invalid access token.")))
		}
	}
}

impl CheckAuth for AccessTokenOptional {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let route = TypeId::of::<R>();

			let output = Self::extract_authentication(incoming_request).map_err(|err| {
				err!(Request(Unauthorized(warn!(
					"Failed to extract optional authorization: {:?}",
					err
				))))
			})?;

			<Self as CheckAuth>::verify(services, output, incoming_request, query, route).await
		}
	}

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		services: &Services,
		output: Self::Output,
		request: &hyper::Request<B>,
		query: AuthQueryParams,
		route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			match output {
				| Some(token) =>
					<AccessToken as CheckAuth>::verify(services, token, request, query, route)
						.await,
				| None => Ok(Auth::default()),
			}
		}
	}
}

impl CheckAuth for AppserviceToken {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let route = TypeId::of::<R>();

			let output = Self::extract_authentication(incoming_request).map_err(|err| {
				err!(Request(Unauthorized(warn!(
					"Failed to extract appservice authorization: {:?}",
					err
				))))
			})?;

			Self::verify(services, output, incoming_request, query, route).await
		}
	}

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		services: &Services,
		output: Self::Output,
		_request: &hyper::Request<B>,
		query: AuthQueryParams,
		_route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let Ok(appservice_info) = services.appservice.find_from_token(&output).await else {
				return Err!(Request(UnknownToken("Invalid appservice token.")));
			};

			let Ok(sender_user) = query.user_id.clone().map_or_else(
				|| {
					UserId::parse_with_server_name(
						appservice_info.registration.sender_localpart.as_str(),
						services.globals.server_name(),
					)
				},
				UserId::parse,
			) else {
				return Err!(Request(InvalidUsername("Username is invalid.")));
			};

			if !appservice_info.is_user_match(&sender_user) {
				return Err!(Request(Exclusive("User is not in namespace.")));
			}

			// MSC3202/MSC4190: Handle device_id masquerading for appservices.
			let sender_device =
				if let Some(device_id) = query.device_id.as_deref().map(Into::into) {
					if services
						.users
						.get_device_metadata(&sender_user, device_id)
						.await
						.is_err()
					{
						return Err!(Request(Forbidden(
							"Device does not exist for user or appservice cannot masquerade as \
							 this device."
						)));
					}

					Some(device_id.to_owned())
				} else {
					None
				};

			Ok(Auth {
				sender_user: Some(sender_user),
				sender_device,
				appservice_info: Some(appservice_info),
				..Default::default()
			})
		}
	}
}

impl CheckAuth for AppserviceTokenOptional {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let route = TypeId::of::<R>();

			let output = Self::extract_authentication(incoming_request).map_err(|err| {
				err!(Request(Unauthorized(warn!(
					"Failed to extract optional appservice authorization: {:?}",
					err
				))))
			})?;

			<Self as CheckAuth>::verify(services, output, incoming_request, query, route).await
		}
	}

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		services: &Services,
		output: Self::Output,
		request: &hyper::Request<B>,
		query: AuthQueryParams,
		route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			match output {
				| Some(token) =>
					<AppserviceToken as CheckAuth>::verify(services, token, request, query, route)
						.await,
				| None => Ok(Auth::default()),
			}
		}
	}
}

impl CheckAuth for NoAuthentication {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let route = TypeId::of::<R>();
			Self::verify(services, (), incoming_request, query, route).await
		}
	}

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		_services: &Services,
		_output: Self::Output,
		_request: &hyper::Request<B>,
		_query: AuthQueryParams,
		_route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move { Ok(Auth::default()) }
	}
}

impl CheckAuth for NoAccessToken {
	#[allow(clippy::manual_async_fn)]
	fn authenticate<R: IncomingRequest + Any, B: AsRef<[u8]> + Sync>(
		services: &Services,
		incoming_request: &hyper::Request<B>,
		query: AuthQueryParams,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move {
			let route = TypeId::of::<R>();

			// We handle these the same as AccessTokenOptional
			let token =
				AccessTokenOptional::extract_authentication(incoming_request).map_err(|err| {
					err!(Request(Unauthorized(warn!(
						"Failed to extract authorization for NoAccessToken: {:?}",
						err
					))))
				})?;

			<AccessTokenOptional as CheckAuth>::verify(
				services,
				token,
				incoming_request,
				query,
				route,
			)
			.await
		}
	}

	#[allow(clippy::manual_async_fn)]
	fn verify<B: AsRef<[u8]> + Sync>(
		_services: &Services,
		_output: Self::Output,
		_request: &hyper::Request<B>,
		_query: AuthQueryParams,
		_route: TypeId,
	) -> impl Future<Output = Result<Auth>> + Send {
		async move { panic!("NoAccessToken::verify should not be called, use authenticate instead") }
	}
}
