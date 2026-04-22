use super::fed::{FedDest, add_port_to_hostname, get_ip_with_port};

#[test]
fn ips_get_default_ports() {
	assert_eq!(
		get_ip_with_port("1.1.1.1"),
		Some(FedDest::Literal("1.1.1.1:8448".parse().unwrap()))
	);
	assert_eq!(
		get_ip_with_port("dead:beef::"),
		Some(FedDest::Literal("[dead:beef::]:8448".parse().unwrap()))
	);
}

#[test]
fn ips_keep_custom_ports() {
	assert_eq!(
		get_ip_with_port("1.1.1.1:1234"),
		Some(FedDest::Literal("1.1.1.1:1234".parse().unwrap()))
	);
	assert_eq!(
		get_ip_with_port("[dead::beef]:8933"),
		Some(FedDest::Literal("[dead::beef]:8933".parse().unwrap()))
	);
}

#[test]
fn hostnames_get_default_ports() {
	assert_eq!(
		add_port_to_hostname("example.com"),
		FedDest::Named(String::from("example.com"), ":8448".try_into().unwrap())
	);
}

#[test]
fn hostnames_keep_custom_ports() {
	assert_eq!(
		add_port_to_hostname("example.com:1337"),
		FedDest::Named(String::from("example.com"), ":1337".try_into().unwrap())
	);
}

#[tokio::test]
async fn test_dns_resolution_integration() {
	// This is a minimal test that verifies the hickory-resolver can initialize
	// and perform a lookup. We can't easily run a full Server here, but we
	// can verify the configuration logic we use in Resolver::build.
	let (sys_conf, mut opts) = hickory_resolver::system_conf::read_system_conf().unwrap();
	opts.use_hosts_file = hickory_resolver::config::ResolveHosts::Always;

	let rt_prov = hickory_resolver::proto::runtime::TokioRuntimeProvider::new();
	let conn_prov = hickory_resolver::name_server::TokioConnectionProvider::new(rt_prov);
	let mut builder = hickory_resolver::TokioResolver::builder_with_config(sys_conf, conn_prov);
	*builder.options_mut() = opts;
	let resolver = builder.build();

	// Test resolving localhost, which should be in /etc/hosts on almost any system
	let result = resolver.lookup_ip("localhost").await;
	assert!(result.is_ok(), "Failed to resolve localhost: {:?}", result.err());

	// Test a custom host if provided via environment (used in CI)
	if let Ok(custom_host) = std::env::var("CONDUWUIT_TEST_DNS_HOST") {
		let result = resolver.lookup_ip(&custom_host).await;
		assert!(
			result.is_ok(),
			"Failed to resolve custom host {}: {:?}",
			custom_host,
			result.err()
		);
	}
}

#[tokio::test]
async fn test_dns_fallback_logic() {
	// Simulate missing system configuration (common in scratch containers)
	let _sys_conf = hickory_resolver::config::ResolverConfig::new();
	let mut opts = hickory_resolver::config::ResolverOpts::default();
	opts.use_hosts_file = hickory_resolver::config::ResolveHosts::Always;

	// This mimics our logic in Resolver::build when nameservers.is_empty()
	let mut conf = hickory_resolver::config::ResolverConfig::new();

	// Use Quad9 fallback (simulating config.dns_fallbacks)
	let fallback_addr: std::net::SocketAddr = "9.9.9.9:53".parse().unwrap();
	let ns = hickory_resolver::config::NameServerConfig::new(
		fallback_addr,
		hickory_resolver::proto::xfer::Protocol::Udp,
	);
	conf.add_name_server(ns);

	let rt_prov = hickory_resolver::proto::runtime::TokioRuntimeProvider::new();
	let conn_prov = hickory_resolver::name_server::TokioConnectionProvider::new(rt_prov);
	let builder = hickory_resolver::TokioResolver::builder_with_config(conf, conn_prov);
	let resolver = builder.build();

	// Test resolving a well-known public domain via our fallback
	let result = resolver.lookup_ip("google.com").await;
	assert!(result.is_ok(), "Failed to resolve google.com via fallback: {:?}", result.err());
}
