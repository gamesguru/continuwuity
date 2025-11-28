use oxide_auth::primitives::registrar::RegisteredUrl;
use url::Url;

mod device_store;
mod endpoint;
mod issuer;
mod registrar;
mod solicitor;
pub use self::{
	device_store::DeviceStore,
	endpoint::OidcEndpoint,
	issuer::{OidcDevice, OidcIssuer},
	registrar::{OidcClient, OidcRegistrar},
	solicitor::AsyncSolicitor,
};

pub const SCOPE_PREFIX_DEVICE: &str = "urn:matrix:org.matrix.msc2967.client:device:";
pub const SCOPE_PREFIX_API: &str = "urn:matrix:org.matrix.msc2967.client:api:";

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
