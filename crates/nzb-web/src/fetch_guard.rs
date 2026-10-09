//! SSRF guard for server-side URL fetches.
//!
//! Any endpoint that fetches a caller-supplied URL (the native
//! `POST /api/queue/add-url`, RSS feed fetches, and the SABnzbd-compatible
//! `mode=addurl`) must route through [`validate_fetch_url`] before issuing the
//! request. The guard rejects non-http(s) schemes and any host that resolves
//! to a private, loopback, or otherwise non-globally-routable address, and
//! [`build_fetch_client`] pins the connection to the exact addresses that were
//! validated so a hostname cannot re-resolve to an internal address between
//! the check and the request (DNS rebinding).
//!
//! Self-hosted indexers (NZBHydra2, Prowlarr) usually live on the LAN or a
//! Docker network, so the guard can be relaxed with a [`FetchPolicy`] built
//! from `general.fetch_allow_private` and `general.fetch_allowed_hosts`. The
//! default policy is strict. `fetch_allow_private` admits RFC 1918, CGNAT and
//! IPv6 ULA only: loopback and link-local addresses (the 169.254.169.254
//! cloud-metadata endpoint) stay blocked unless explicitly listed.

use std::net::{IpAddr, SocketAddr};

use crate::error::ApiError;
use crate::nzb_core::config::GeneralConfig;

/// Which non-public destinations a server-side fetch may reach.
#[derive(Debug, Clone, Default)]
pub struct FetchPolicy {
    allow_private: bool,
    allowed_hostnames: Vec<String>,
    allowed_networks: Vec<(IpAddr, u8)>,
}

impl FetchPolicy {
    /// The default policy: only globally routable addresses are allowed.
    pub fn strict() -> Self {
        Self::default()
    }

    /// Build a policy from `fetch_allow_private` and `fetch_allowed_hosts`.
    /// Entries that parse as an IP address or CIDR block become networks;
    /// anything else is matched case-insensitively against the URL host.
    pub fn from_config(general: &GeneralConfig) -> Self {
        Self::new(general.fetch_allow_private, &general.fetch_allowed_hosts)
    }

    pub fn new(allow_private: bool, allowed_hosts: &[String]) -> Self {
        let mut policy = Self {
            allow_private,
            ..Self::default()
        };
        for entry in allowed_hosts {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            match parse_network(entry) {
                Some(network) => policy.allowed_networks.push(network),
                None => policy
                    .allowed_hostnames
                    .push(entry.trim_end_matches('.').to_ascii_lowercase()),
            }
        }
        policy
    }

    fn host_is_listed(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.allowed_hostnames.contains(&host)
    }

    fn ip_is_allowed(&self, ip: IpAddr) -> bool {
        if is_cloud_metadata(ip) {
            return self.ip_is_explicitly_allowed(ip);
        }
        is_globally_routable(ip)
            || self
                .allowed_networks
                .iter()
                .any(|(net, prefix)| network_contains(*net, *prefix, ip))
            || (self.allow_private && is_private_lan(ip))
    }

    /// An explicit IP or CIDR entry, which is the only way to admit a
    /// cloud-metadata address. A hostname entry does not count.
    fn ip_is_explicitly_allowed(&self, ip: IpAddr) -> bool {
        self.allowed_networks
            .iter()
            .any(|(net, prefix)| network_contains(*net, *prefix, ip))
    }
}

/// Parse `addr` or `addr/prefix` into a network. Returns `None` for
/// anything that is not an IP address (treated as a hostname).
fn parse_network(entry: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = match entry.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (entry, None),
    };
    let addr = addr.trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = addr.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)?,
        None => max,
    };
    Some((ip, prefix))
}

fn network_contains(net: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    let ip = match (net, ip) {
        // Let an IPv4 network match an IPv4-mapped IPv6 address.
        (IpAddr::V4(_), IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => return false,
        },
        _ => ip,
    };
    match (net, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(net) & mask == u32::from(ip) & mask
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(net) & mask == u128::from(ip) & mask
        }
        _ => false,
    }
}

/// Cloud instance metadata endpoints. These sit inside ranges that
/// `fetch_allow_private` otherwise admits (CGNAT, unique-local, link-local),
/// so they are refused even when that flag is on and even when a hostname
/// that resolves to them is listed. Only an explicit IP or CIDR entry in
/// `fetch_allowed_hosts` admits them.
fn is_cloud_metadata(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_cloud_metadata_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_cloud_metadata_v4(v4),
            None => v6 == "fd00:ec2::254".parse::<std::net::Ipv6Addr>().unwrap(),
        },
    }
}

fn is_cloud_metadata_v4(v4: std::net::Ipv4Addr) -> bool {
    v4 == std::net::Ipv4Addr::new(169, 254, 169, 254)
        || v4 == std::net::Ipv4Addr::new(100, 100, 100, 200)
}

/// Private LAN ranges a self-hosted indexer may live on: RFC 1918,
/// carrier-grade NAT and IPv6 unique-local. Loopback is not included:
/// `fetch_allow_private` must not open 127.0.0.0/8 or ::1, which are
/// reachable only through an explicit `fetch_allowed_hosts` entry.
/// Link-local (cloud metadata), multicast, broadcast and unspecified stay
/// out as well.
fn is_private_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [first, second, ..] = v4.octets();
            v4.is_private() || (first == 100 && (64..=127).contains(&second))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_private_lan(IpAddr::V4(v4)),
            None => v6.is_unique_local(),
        },
    }
}

/// Maximum body size accepted by URL-backed NZB and feed workflows.
pub const MAX_FETCH_BODY_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug)]
pub struct FetchUrlPlan {
    pub url: reqwest::Url,
    resolved_addrs: Option<(String, Vec<SocketAddr>)>,
}

impl FetchUrlPlan {
    /// Whether a request for this URL needs a client pinned to the validated
    /// addresses. True when the host was a resolved hostname (guards against
    /// DNS rebinding); false for an IP-literal URL, where a shared pooled
    /// client is safe to reuse.
    pub fn requires_pinned_client(&self) -> bool {
        self.resolved_addrs.is_some()
    }
}

/// Returns `Err` if `raw_url` is not http/https or resolves to a
/// private/reserved address. Uses the strict default policy; see
/// [`validate_fetch_url_with`] to honour the configured allow-list.
pub async fn validate_fetch_url(raw_url: &str) -> Result<FetchUrlPlan, ApiError> {
    validate_fetch_url_with(raw_url, &FetchPolicy::strict()).await
}

/// Returns `Err` if `raw_url` is not http/https or resolves to an address
/// that `policy` does not allow.
pub async fn validate_fetch_url_with(
    raw_url: &str,
    policy: &FetchPolicy,
) -> Result<FetchUrlPlan, ApiError> {
    match validate_inner(raw_url, policy).await {
        Ok(plan) => Ok(plan),
        Err(GuardError::Rejected(e) | GuardError::Unresolved(e)) => Err(e),
    }
}

/// Check that `raw_url` is permitted by `policy` without requiring that it
/// resolve right now. Used when saving a URL (an RSS feed) that will be
/// fetched later: a policy violation is an error, a transient DNS failure is
/// not, because the fetch re-validates on every poll anyway.
pub async fn check_fetch_url_allowed(raw_url: &str, policy: &FetchPolicy) -> Result<(), ApiError> {
    match validate_inner(raw_url, policy).await {
        Ok(_) | Err(GuardError::Unresolved(_)) => Ok(()),
        Err(GuardError::Rejected(e)) => Err(e),
    }
}

enum GuardError {
    /// The URL is not permitted.
    Rejected(ApiError),
    /// The hostname could not be resolved; permission is undetermined.
    Unresolved(ApiError),
}

async fn validate_inner(raw_url: &str, policy: &FetchPolicy) -> Result<FetchUrlPlan, GuardError> {
    let reject = |e: anyhow::Error| GuardError::Rejected(ApiError::from(e));
    let url =
        reqwest::Url::parse(raw_url).map_err(|e| reject(anyhow::anyhow!("Invalid URL: {e}")))?;

    match url.scheme() {
        "http" | "https" => {}
        s => {
            return Err(reject(anyhow::anyhow!(
                "URL scheme '{s}' not allowed (must be http or https)"
            )));
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| reject(anyhow::anyhow!("URL has no host")))?
        .to_string();

    // IP literal: validate directly without a DNS round-trip.
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if is_cloud_metadata(ip) && !policy.ip_is_explicitly_allowed(ip) {
            return Err(reject(anyhow::anyhow!(
                "URL targets a cloud metadata address (allow it only with an \
                 explicit IP or CIDR in general.fetch_allowed_hosts)"
            )));
        }
        if !policy.ip_is_allowed(ip) {
            return Err(reject(anyhow::anyhow!(
                "URL targets a private/reserved address (allow it with \
                 general.fetch_allow_private or general.fetch_allowed_hosts)"
            )));
        }
        return Ok(FetchUrlPlan {
            url,
            resolved_addrs: None,
        });
    }

    // Hostname: resolve and check every returned address. An explicitly
    // listed hostname may resolve anywhere, but the connection is still
    // pinned to the addresses resolved here.
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<_> = tokio::net::lookup_host(format!("{host}:{port}"))
        .await
        .map_err(|e| {
            GuardError::Unresolved(ApiError::from(anyhow::anyhow!(
                "DNS resolution failed for '{host}': {e}"
            )))
        })?
        .collect();

    if addrs.is_empty() {
        return Err(GuardError::Unresolved(ApiError::from(anyhow::anyhow!(
            "DNS resolution returned no addresses for '{host}'"
        ))));
    }

    if !policy.host_is_listed(&host) {
        for addr in &addrs {
            if !policy.ip_is_allowed(addr.ip()) {
                return Err(reject(anyhow::anyhow!(
                    "URL resolves to a private/reserved address (allow it with \
                     general.fetch_allow_private or general.fetch_allowed_hosts)"
                )));
            }
        }
    } else {
        // A listed hostname still cannot resolve to a cloud-metadata address
        // unless that address itself is an explicit allowlist entry.
        for addr in &addrs {
            if is_cloud_metadata(addr.ip()) && !policy.ip_is_explicitly_allowed(addr.ip()) {
                return Err(reject(anyhow::anyhow!(
                    "URL resolves to a cloud metadata address (allow it only with \
                     an explicit IP or CIDR in general.fetch_allowed_hosts)"
                )));
            }
        }
    }

    Ok(FetchUrlPlan {
        url,
        resolved_addrs: Some((host, addrs)),
    })
}

/// Build a reqwest client pinned to the addresses validated in `plan`, so a
/// hostname cannot re-resolve to an internal address after the check.
pub fn build_fetch_client(plan: &FetchUrlPlan) -> Result<reqwest::Client, ApiError> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // Redirect targets are not covered by the original DNS validation.
        // Refuse redirects so a public URL cannot bounce into a private host.
        .redirect(reqwest::redirect::Policy::none());
    if let Some((host, addrs)) = &plan.resolved_addrs {
        builder = builder.resolve_to_addrs(host, addrs.as_slice());
    }
    builder
        .build()
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to build fetch client: {e}")))
}

/// Read a response body, failing if it exceeds `max_bytes` (avoids unbounded
/// memory use from a hostile or misconfigured URL).
pub async fn read_response_bytes_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ApiError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ApiError::from(anyhow::anyhow!(
            "Fetched body exceeds the {} MB limit",
            max_bytes / 1024 / 1024
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to read response: {e}")))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ApiError::from(anyhow::anyhow!(
                "Fetched body exceeds the {} MB limit",
                max_bytes / 1024 / 1024
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn is_globally_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [first, second, ..] = v4.octets();
            let this_network = first == 0;
            let shared_address_space = first == 100 && (64..=127).contains(&second);
            let benchmarking_space = first == 198 && (18..=19).contains(&second);
            let reserved_zero_block = first == 192 && second == 0;
            let multicast = (224..=239).contains(&first);
            let reserved_future_use = first >= 240;
            !this_network
                && !v4.is_loopback()
                && !v4.is_private()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_unspecified()
                && !v4.is_documentation()
                && !shared_address_space
                && !benchmarking_space
                && !reserved_zero_block
                && !multicast
                && !reserved_future_use
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            let mapped_is_global = v6
                .to_ipv4_mapped()
                .is_none_or(|mapped| is_globally_routable(IpAddr::V4(mapped)));
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                && !v6.is_unique_local()
                && (first & 0xffc0) != 0xfe80
                && (first & 0xffc0) != 0xfec0
                && !(first == 0x2001 && v6.segments()[1] == 0x0db8)
                && mapped_is_global
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validate_fetch_url_rejects_private_ip_literals() {
        let err = validate_fetch_url("http://127.0.0.1/file.nzb")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private/reserved"));
    }

    #[tokio::test]
    async fn validate_fetch_url_rejects_localhost_hostname() {
        let err = validate_fetch_url("http://localhost/file.nzb")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private/reserved"));
    }

    #[tokio::test]
    async fn validate_fetch_url_rejects_link_local_metadata() {
        // 169.254.169.254 is the cloud-metadata endpoint; must be refused.
        let err = validate_fetch_url("http://169.254.169.254/latest/meta-data/")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud metadata"));
    }

    #[tokio::test]
    async fn validate_fetch_url_rejects_non_http_scheme() {
        let err = validate_fetch_url("file:///etc/passwd").await.unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[tokio::test]
    async fn validate_fetch_url_rejects_special_use_address_ranges() {
        for url in [
            "http://100.64.0.1/file.nzb",
            "http://198.18.0.1/file.nzb",
            "http://192.0.0.1/file.nzb",
            "http://0.1.2.3/file.nzb",
            "http://224.0.0.1/file.nzb",
            "http://240.0.0.1/file.nzb",
            "http://[fe80::1]/file.nzb",
            "http://[2001:db8::1]/file.nzb",
        ] {
            let error = validate_fetch_url(url)
                .await
                .expect_err("special-use address must be rejected");
            assert!(
                error.to_string().contains("private/reserved"),
                "{url}: {error}"
            );
        }
    }

    fn hosts(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|e| e.to_string()).collect()
    }

    #[tokio::test]
    async fn strict_policy_rejects_lan_and_docker_addresses() {
        for url in ["http://192.168.1.10/api", "http://172.17.0.2:9696/api"] {
            let error = validate_fetch_url_with(url, &FetchPolicy::strict())
                .await
                .expect_err("strict policy must reject private address");
            assert!(error.to_string().contains("private/reserved"), "{url}");
        }
    }

    #[tokio::test]
    async fn allow_private_admits_lan_indexers_but_not_metadata() {
        let policy = FetchPolicy::new(true, &[]);
        for url in [
            "http://192.168.1.10/api",
            "http://172.17.0.2:9696/api",
            "http://10.0.0.5/api",
            "http://100.100.1.1/api",
            "http://[fd00::1]/api",
        ] {
            validate_fetch_url_with(url, &policy)
                .await
                .unwrap_or_else(|e| panic!("{url} must be allowed: {e}"));
        }
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.100.200/latest/meta-data/",
            "http://[fd00:ec2::254]/latest/meta-data/",
            "http://[::ffff:169.254.169.254]/",
            "http://[::ffff:100.100.100.200]/",
            "http://[fe80::1]/",
            "http://127.0.0.1:5076/api",
            "http://[::1]/api",
            "http://224.0.0.1/",
            "http://0.0.0.0/",
            "http://255.255.255.255/",
        ] {
            let error = validate_fetch_url_with(url, &policy)
                .await
                .expect_err("non-LAN special address must stay blocked");
            let message = error.to_string();
            assert!(
                message.contains("private/reserved") || message.contains("cloud metadata"),
                "{url}: {message}"
            );
        }
    }

    #[tokio::test]
    async fn allowed_hosts_admit_only_listed_networks_and_names() {
        let policy = FetchPolicy::new(false, &hosts(&["172.16.0.0/12", "10.1.2.3", "LocalHost"]));
        for url in [
            "http://172.20.0.5/api",
            "http://10.1.2.3/api",
            "http://localhost:9696/api",
        ] {
            validate_fetch_url_with(url, &policy)
                .await
                .unwrap_or_else(|e| panic!("{url} must be allowed: {e}"));
        }
        for url in [
            "http://192.168.1.1/api",
            "http://10.1.2.4/api",
            "http://127.0.0.1/api",
            "http://169.254.169.254/",
        ] {
            validate_fetch_url_with(url, &policy)
                .await
                .expect_err("unlisted private address must be rejected");
        }
    }

    #[tokio::test]
    async fn metadata_endpoint_requires_explicit_listing() {
        let policy = FetchPolicy::new(true, &hosts(&["169.254.169.254/32"]));
        validate_fetch_url_with("http://169.254.169.254/", &policy)
            .await
            .expect("explicitly listed metadata address is allowed");
    }

    #[tokio::test]
    async fn listed_hostname_still_cannot_resolve_to_metadata() {
        let policy = FetchPolicy::new(true, &hosts(&["metadata.example.test"]));
        assert!(policy.host_is_listed("metadata.example.test"));
        for ip in [
            "100.100.100.200".parse::<IpAddr>().unwrap(),
            "fd00:ec2::254".parse().unwrap(),
            "::ffff:100.100.100.200".parse().unwrap(),
        ] {
            assert!(
                is_cloud_metadata(ip) && !policy.ip_is_explicitly_allowed(ip),
                "{ip} must stay denied when only a hostname is listed"
            );
        }
        let explicit = FetchPolicy::new(false, &hosts(&["100.100.100.200"]));
        validate_fetch_url_with("http://100.100.100.200/latest", &explicit)
            .await
            .expect("an explicit IP entry admits the metadata address");
    }

    #[tokio::test]
    async fn hostname_plan_stays_pinned_when_allowed() {
        let policy = FetchPolicy::new(false, &hosts(&["localhost"]));
        let plan = validate_fetch_url_with("http://localhost:5076/api", &policy)
            .await
            .expect("listed hostname allowed");
        assert!(plan.requires_pinned_client());
    }

    #[test]
    fn policy_from_config_reads_general_settings() {
        let general = GeneralConfig {
            fetch_allow_private: false,
            fetch_allowed_hosts: hosts(&["192.168.0.0/16", "prowlarr", "bogus/99"]),
            ..GeneralConfig::default()
        };
        let policy = FetchPolicy::from_config(&general);
        assert!(policy.ip_is_allowed("192.168.4.4".parse().unwrap()));
        assert!(!policy.ip_is_allowed("10.0.0.1".parse().unwrap()));
        assert!(policy.host_is_listed("Prowlarr."));
        assert!(
            !FetchPolicy::from_config(&GeneralConfig::default())
                .ip_is_allowed("192.168.4.4".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn check_fetch_url_allowed_rejects_policy_violations_only() {
        let strict = FetchPolicy::strict();
        check_fetch_url_allowed("http://192.168.1.10/rss", &strict)
            .await
            .expect_err("private feed must be rejected at save time");
        check_fetch_url_allowed("ftp://example.com/rss", &strict)
            .await
            .expect_err("bad scheme must be rejected at save time");
        // Unresolvable names are not a policy violation; polling re-validates.
        check_fetch_url_allowed("https://feed.invalid/rss", &strict)
            .await
            .expect("unresolvable host is accepted at save time");
        check_fetch_url_allowed("http://192.168.1.10/rss", &FetchPolicy::new(true, &[]))
            .await
            .expect("private feed allowed by policy");
    }

    async fn one_shot_http_response(response: &'static str) -> reqwest::Url {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local fixture");
        let address = listener.local_addr().expect("local fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept local fixture");
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{address}/fixture").parse().unwrap()
    }

    #[tokio::test]
    async fn pinned_client_uses_validated_address_and_does_not_follow_redirects() {
        let url = one_shot_http_response(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let address = url.port().expect("fixture port");
        let plan = FetchUrlPlan {
            url: url.clone(),
            resolved_addrs: Some((
                "fixture.invalid".into(),
                vec![std::net::SocketAddr::from(([127, 0, 0, 1], address))],
            )),
        };
        let client = build_fetch_client(&plan).expect("build pinned client");
        let response = client
            .get(url)
            .header("host", "fixture.invalid")
            .send()
            .await
            .expect("request local fixture");
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    }

    #[tokio::test]
    async fn response_body_limit_is_enforced_incrementally() {
        let url = one_shot_http_response(
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n12345678",
        )
        .await;
        let plan = FetchUrlPlan {
            url: url.clone(),
            resolved_addrs: None,
        };
        let response = build_fetch_client(&plan)
            .expect("build fixture client")
            .get(url)
            .send()
            .await
            .expect("request body fixture");
        let error = read_response_bytes_limited(response, 4)
            .await
            .expect_err("oversized body must be rejected");
        assert!(error.to_string().contains("exceeds"));
    }
}
