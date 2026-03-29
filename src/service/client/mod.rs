use std::{sync::Arc, time::Duration};

use conduwuit::{Config, Result, err, implement, trace};
use either::Either;
use ipaddress::IPAddress;
use regex::RegexSet;
use reqwest::redirect;

use crate::{resolver, service};

pub enum ClientType {
	Default,
	UrlPreview,
	ExternMedia,
	WellKnown,
	Federation,
	Synapse,
	Sender,
	Appservice,
	Pusher,
}

pub struct Service {
	default: reqwest::Client,
	url_preview: reqwest::Client,
	extern_media: reqwest::Client,
	well_known: reqwest::Client,
	federation: reqwest::Client,
	synapse: reqwest::Client,
	sender: reqwest::Client,
	appservice: reqwest::Client,
	pusher: reqwest::Client,

	// this client is used if the destination matches insecure_skip_tls_validation_for_servers
	// WHY: this is for servers who are on an overlay network like TOR/I2P who dont need or cant
	// get      a https certificate
	default_no_tls_validation: reqwest::Client,
	url_preview_no_tls_validation: reqwest::Client,
	extern_media_no_tls_validation: reqwest::Client,
	well_known_no_tls_validation: reqwest::Client,
	federation_no_tls_validation: reqwest::Client,
	synapse_no_tls_validation: reqwest::Client,
	sender_no_tls_validation: reqwest::Client,
	appservice_no_tls_validation: reqwest::Client,
	pusher_no_tls_validation: reqwest::Client,

	no_tls_validation_host_regex: RegexSet,
	pub cidr_range_denylist: Vec<IPAddress>,
}

impl Service {
	fn secure_client(&self, client_type: &ClientType) -> &reqwest::Client {
		match client_type {
			| ClientType::Default => &self.default,
			| ClientType::UrlPreview => &self.url_preview,
			| ClientType::ExternMedia => &self.extern_media,
			| ClientType::WellKnown => &self.well_known,
			| ClientType::Federation => &self.federation,
			| ClientType::Synapse => &self.synapse,
			| ClientType::Sender => &self.sender,
			| ClientType::Appservice => &self.appservice,
			| ClientType::Pusher => &self.pusher,
		}
	}

	fn insecure_client(&self, client_type: &ClientType) -> &reqwest::Client {
		match client_type {
			| ClientType::Default => &self.default_no_tls_validation,
			| ClientType::UrlPreview => &self.url_preview_no_tls_validation,
			| ClientType::ExternMedia => &self.extern_media_no_tls_validation,
			| ClientType::WellKnown => &self.well_known_no_tls_validation,
			| ClientType::Federation => &self.federation_no_tls_validation,
			| ClientType::Synapse => &self.synapse_no_tls_validation,
			| ClientType::Sender => &self.sender_no_tls_validation,
			| ClientType::Appservice => &self.appservice_no_tls_validation,
			| ClientType::Pusher => &self.pusher_no_tls_validation,
		}
	}

	#[must_use]
	pub fn get_client(&self, client_type: &ClientType, url: &reqwest::Url) -> &reqwest::Client {
		if let Some(host) = url.host_str() {
			if self.no_tls_validation_host_regex.is_match(host) {
				self.insecure_client(client_type)
			} else {
				self.secure_client(client_type)
			}
		} else {
			// If the URL has no host, fall back to the secure client rather than panicking.
			self.secure_client(client_type)
		}
	}
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let resolver = args.require::<resolver::Service>("resolver");

		let url_preview_bind_addr = config
			.url_preview_bound_interface
			.clone()
			.and_then(Either::left);

		let url_preview_bind_iface = config
			.url_preview_bound_interface
			.clone()
			.and_then(Either::right);

		let url_preview_user_agent = config
			.url_preview_user_agent
			.clone()
			.unwrap_or_else(|| conduwuit::version::user_agent_media().to_owned());

		let cidr_range_denylist = config
			.ip_range_denylist
			.iter()
			.map(IPAddress::parse)
			.inspect(|cidr| trace!("Denied CIDR range: {cidr:?}"))
			.collect::<Result<_, String>>()
			.map_err(|e| err!(Config("ip_range_denylist", e)))?;

		Ok(Arc::new(Self {
			no_tls_validation_host_regex: config.insecure_skip_tls_validation_for_servers.clone(),
			default: build_default(config, &resolver)?,
			url_preview: build_url_preview(
				config,
				&resolver,
				url_preview_bind_iface.as_deref(),
				url_preview_bind_addr,
				&url_preview_user_agent,
				false,
			)?,
			extern_media: build_extern_media(config, &resolver, false)?,
			well_known: build_well_known(config, &resolver, false)?,
			federation: build_federation(config, &resolver, false)?,
			synapse: build_synapse(config, &resolver, false)?,
			sender: build_sender(config, &resolver, false)?,
			appservice: build_appservice(config, &resolver, false)?,
			pusher: build_pusher(config, &resolver, false)?,

			default_no_tls_validation: build_default_no_tls(config, &resolver)?,
			url_preview_no_tls_validation: build_url_preview(
				config,
				&resolver,
				url_preview_bind_iface.as_deref(),
				url_preview_bind_addr,
				&url_preview_user_agent,
				true,
			)?,
			extern_media_no_tls_validation: build_extern_media(config, &resolver, true)?,
			well_known_no_tls_validation: build_well_known(config, &resolver, true)?,
			federation_no_tls_validation: build_federation(config, &resolver, true)?,
			synapse_no_tls_validation: build_synapse(config, &resolver, true)?,
			sender_no_tls_validation: build_sender(config, &resolver, true)?,
			appservice_no_tls_validation: build_appservice(config, &resolver, true)?,
			pusher_no_tls_validation: build_pusher(config, &resolver, true)?,

			cidr_range_denylist,
		}))
	}

	fn name(&self) -> &str { service::make_name(std::module_path!()) }
}

#[inline(never)]
fn build_default(config: &Config, resolver: &resolver::Service) -> Result<reqwest::Client> {
	base(config)?
		.dns_resolver(resolver.resolver.clone())
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_default_no_tls(
	config: &Config,
	resolver: &resolver::Service,
) -> Result<reqwest::Client> {
	base(config)?
		.danger_accept_invalid_certs(true)
		.dns_resolver(resolver.resolver.clone())
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_url_preview(
	config: &Config,
	resolver: &resolver::Service,
	iface: Option<&str>,
	addr: Option<std::net::IpAddr>,
	ua: &str,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config).and_then(|builder| builder_interface(builder, iface))?;

	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}

	builder
		.local_address(addr)
		.dns_resolver(resolver.resolver.clone())
		.timeout(Duration::from_secs(config.url_preview_timeout))
		.redirect(redirect::Policy::limited(3))
		.user_agent(ua)
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_extern_media(
	config: &Config,
	resolver: &resolver::Service,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config)?;
	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}
	builder
		.dns_resolver(resolver.resolver.clone())
		.redirect(redirect::Policy::limited(3))
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_well_known(
	config: &Config,
	resolver: &resolver::Service,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config)?;
	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}
	builder
		.dns_resolver(resolver.resolver.clone())
		.connect_timeout(Duration::from_secs(config.well_known_conn_timeout))
		.read_timeout(Duration::from_secs(config.well_known_timeout))
		.timeout(Duration::from_secs(config.well_known_timeout))
		.pool_max_idle_per_host(0)
		.redirect(redirect::Policy::limited(4))
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_federation(
	config: &Config,
	resolver: &resolver::Service,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config)?;
	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}
	builder
		.dns_resolver(resolver.resolver.hooked.clone())
		.connect_timeout(Duration::from_secs(config.federation_conn_timeout))
		.read_timeout(Duration::from_secs(config.federation_timeout))
		.timeout(Duration::from_secs(
			config
				.federation_timeout
				.saturating_add(config.federation_conn_timeout),
		))
		.pool_max_idle_per_host(config.federation_idle_per_host.into())
		.pool_idle_timeout(Duration::from_secs(config.federation_idle_timeout))
		.redirect(redirect::Policy::limited(3))
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_synapse(
	config: &Config,
	resolver: &resolver::Service,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config)?;
	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}
	builder
		.dns_resolver(resolver.resolver.hooked.clone())
		.connect_timeout(Duration::from_secs(config.federation_conn_timeout))
		.read_timeout(Duration::from_secs(config.federation_timeout.saturating_mul(6)))
		.timeout(Duration::from_secs(
			config
				.federation_timeout
				.saturating_mul(6)
				.saturating_add(config.federation_conn_timeout),
		))
		.pool_max_idle_per_host(0)
		.redirect(redirect::Policy::limited(3))
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_sender(
	config: &Config,
	resolver: &resolver::Service,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config)?;
	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}
	builder
		.dns_resolver(resolver.resolver.hooked.clone())
		.connect_timeout(Duration::from_secs(config.federation_conn_timeout))
		.read_timeout(Duration::from_secs(config.sender_timeout))
		.timeout(Duration::from_secs(config.sender_timeout))
		.pool_max_idle_per_host(1)
		.pool_idle_timeout(Duration::from_secs(config.sender_idle_timeout))
		.redirect(redirect::Policy::limited(2))
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_appservice(
	config: &Config,
	resolver: &resolver::Service,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config)?;
	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}
	builder
		.dns_resolver(resolver.resolver.clone())
		.connect_timeout(Duration::from_secs(5))
		.read_timeout(Duration::from_secs(config.appservice_timeout))
		.timeout(Duration::from_secs(config.appservice_timeout))
		.pool_max_idle_per_host(1)
		.pool_idle_timeout(Duration::from_secs(config.appservice_idle_timeout))
		.redirect(redirect::Policy::limited(2))
		.build()
		.map_err(Into::into)
}

#[inline(never)]
fn build_pusher(
	config: &Config,
	resolver: &resolver::Service,
	insecure: bool,
) -> Result<reqwest::Client> {
	let mut builder = base(config)?;
	if insecure {
		builder = builder.danger_accept_invalid_certs(true);
	}
	builder
		.dns_resolver(resolver.resolver.clone())
		.connect_timeout(Duration::from_secs(config.pusher_conn_timeout))
		.timeout(Duration::from_secs(config.pusher_timeout))
		.pool_max_idle_per_host(1)
		.pool_idle_timeout(Duration::from_secs(config.pusher_idle_timeout))
		.redirect(redirect::Policy::limited(2))
		.build()
		.map_err(Into::into)
}

fn base(config: &Config) -> Result<reqwest::ClientBuilder> {
	let mut builder = reqwest::Client::builder()
		.hickory_dns(true)
		.connect_timeout(Duration::from_secs(config.request_conn_timeout))
		.read_timeout(Duration::from_secs(config.request_timeout))
		.timeout(Duration::from_secs(config.request_total_timeout))
		.pool_idle_timeout(Duration::from_secs(config.request_idle_timeout))
		.pool_max_idle_per_host(config.request_idle_per_host.into())
		.user_agent(conduwuit::version::user_agent())
		.redirect(redirect::Policy::limited(6))
        .danger_accept_invalid_certs(config.allow_invalid_tls_certificates_yes_i_know_what_the_fuck_i_am_doing_with_this_and_i_know_this_is_insecure)
		.connection_verbose(cfg!(debug_assertions));

	#[cfg(feature = "gzip_compression")]
	{
		builder = if config.gzip_compression {
			builder.gzip(true)
		} else {
			builder.gzip(false).no_gzip()
		};
	};

	#[cfg(feature = "brotli_compression")]
	{
		builder = if config.brotli_compression {
			builder.brotli(true)
		} else {
			builder.brotli(false).no_brotli()
		};
	};

	#[cfg(feature = "zstd_compression")]
	{
		builder = if config.zstd_compression {
			builder.zstd(true)
		} else {
			builder.zstd(false).no_zstd()
		};
	};

	#[cfg(not(feature = "gzip_compression"))]
	{
		builder = builder.no_gzip();
	};

	#[cfg(not(feature = "brotli_compression"))]
	{
		builder = builder.no_brotli();
	};

	#[cfg(not(feature = "zstd_compression"))]
	{
		builder = builder.no_zstd();
	};

	match config.proxy.to_proxy()? {
		| Some(proxy) => Ok(builder.proxy(proxy)),
		| _ => Ok(builder),
	}
}

#[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
fn builder_interface(
	builder: reqwest::ClientBuilder,
	config: Option<&str>,
) -> Result<reqwest::ClientBuilder> {
	if let Some(iface) = config {
		Ok(builder.interface(iface))
	} else {
		Ok(builder)
	}
}

#[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
fn builder_interface(
	builder: reqwest::ClientBuilder,
	config: Option<&str>,
) -> Result<reqwest::ClientBuilder> {
	use conduwuit::Err;

	if let Some(iface) = config {
		Err!("Binding to network-interface {iface:?} by name is not supported on this platform.")
	} else {
		Ok(builder)
	}
}

#[inline]
#[must_use]
#[implement(Service)]
pub fn valid_cidr_range(&self, ip: &IPAddress) -> bool {
	self.cidr_range_denylist
		.iter()
		.all(|cidr| !cidr.includes(ip))
}
