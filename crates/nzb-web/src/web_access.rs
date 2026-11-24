//! SABnzbd's access rules, for `general.auth_method = "optional"`.
//!
//! These mirror `check_access`, `check_hostname` and `check_login` in
//! SABnzbd's `interface.py`. With the default `auth_method = "login"` nothing
//! here applies.

use std::net::{IpAddr, SocketAddr};

use http::{HeaderMap, Method};

use crate::fetch_guard::network_contains;
use crate::nzb_core::config::{GeneralConfig, WebAuthMethod, parse_ip_network};
use crate::state::AppState;

/// `inet_exposure` level that admits non-local clients to the native API.
pub const ACCESS_API: u8 = 3;
/// `inet_exposure` level that admits non-local clients to the web UI.
pub const ACCESS_WEB_UI: u8 = 4;
/// Above every level, so only local clients pass.
const ACCESS_LOCAL_ONLY: u8 = 6;

/// Header the web UI sends on writes. A foreign page cannot add it without a
/// CORS preflight, which rustnzb never grants.
pub const REQUESTED_WITH_HEADER: &str = "x-requested-with";

/// Whether this process started with `auth_method = "optional"`.
pub fn optional(state: &AppState) -> bool {
    state.startup_config().general.auth_method == WebAuthMethod::Optional
}

fn peer(extensions: &http::Extensions) -> Option<IpAddr> {
    extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
}

/// Whether the client's address (and, while no password is set, the host name
/// it used) is admitted at `access`. Always true with the mandatory login.
pub fn admitted(
    state: &AppState,
    extensions: &http::Extensions,
    headers: &HeaderMap,
    access: u8,
) -> bool {
    if !optional(state) {
        return true;
    }
    let general = &state.config().general;
    let allowed = check_access(general, peer(extensions), headers, access)
        && (state.credential_store.has_credentials() || check_hostname(general, headers));
    if !allowed && warn_due() {
        tracing::warn!(
            "Refused connection from {}",
            peer(extensions).map_or_else(|| "an unknown address".to_owned(), |ip| ip.to_string())
        );
    }
    allowed
}

/// Whether an admitted client needs no login: no password is set, or
/// `inet_exposure` is 5 and the client is local. An unusable
/// `credentials.json` counts as a password, so damage never opens the UI.
pub fn login_optional(
    state: &AppState,
    extensions: &http::Extensions,
    headers: &HeaderMap,
) -> bool {
    if !optional(state) {
        return false;
    }
    let general = &state.config().general;
    !state.credential_store.has_credentials()
        || (general.inet_exposure == 5
            && check_access(general, peer(extensions), headers, ACCESS_LOCAL_ONLY))
}

/// Refusals are logged at most once a minute, so an exposed port cannot flood
/// the log.
fn warn_due() -> bool {
    static LAST: parking_lot::Mutex<Option<std::time::Instant>> = parking_lot::Mutex::new(None);
    let mut last = LAST.lock();
    let due = last.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(60));
    if due {
        *last = Some(std::time::Instant::now());
    }
    due
}

fn check_access(
    general: &GeneralConfig,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    access: u8,
) -> bool {
    if access <= general.inet_exposure {
        return true;
    }
    // X-Forwarded-For can only take access away, never grant it.
    peer.is_some_and(|ip| is_local(general, ip))
        && (!general.verify_xff_header
            || headers.get_all("x-forwarded-for").iter().all(|value| {
                value.to_str().is_ok_and(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|hop| !hop.is_empty())
                        .all(|hop| hop.parse().is_ok_and(|ip| is_local(general, ip)))
                })
            }))
}

fn is_local(general: &GeneralConfig, ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    if ip.is_loopback() {
        return true;
    }
    if general.local_ranges.is_empty() {
        return match ip {
            IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
            IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local(),
        };
    }
    general
        .local_ranges
        .iter()
        .filter_map(|entry| parse_ip_network(entry.trim()))
        .any(|(net, prefix)| network_contains(net, prefix, ip))
}

/// Mitigates DNS rebinding, which an open UI is exposed to.
fn check_hostname(general: &GeneralConfig, headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(http::header::HOST)
        .and_then(|host| host.to_str().ok())
    else {
        return false;
    };
    let host = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    }
    .to_ascii_lowercase();
    host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok()
        || host.ends_with(".local")
        || host.ends_with(".local.")
        || general
            .host_whitelist
            .iter()
            .any(|entry| entry.trim().eq_ignore_ascii_case(&host))
        || machine_name().is_some_and(|name| name.eq_ignore_ascii_case(&host))
}

#[cfg(unix)]
fn machine_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the pointer and length describe `buf`, which outlives the call.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let name = std::ffi::CStr::from_bytes_until_nul(&buf).ok()?;
    name.to_str().ok().map(str::to_owned)
}

#[cfg(not(unix))]
fn machine_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

/// Whether a request accepted without a token may be a foreign page's doing.
/// SABnzbd requires its API key on every write for this reason; here, writes
/// from a browser need [`REQUESTED_WITH_HEADER`] or a same-origin
/// `Sec-Fetch-Site`. Other clients send neither `Sec-Fetch-Site` nor `Origin`.
pub fn cross_site_write(method: &Method, headers: &HeaderMap) -> bool {
    if method.is_safe() || headers.contains_key(REQUESTED_WITH_HEADER) {
        return false;
    }
    match headers.get("sec-fetch-site") {
        Some(site) => site != "same-origin" && site != "none",
        None => headers.contains_key(http::header::ORIGIN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for &(name, value) in pairs {
            headers.append(name, value.parse().unwrap());
        }
        headers
    }

    fn access(general: &GeneralConfig, peer: &str, xff: &[&str], level: u8) -> bool {
        let mut map = HeaderMap::new();
        for value in xff {
            map.append("x-forwarded-for", value.parse().unwrap());
        }
        check_access(general, peer.parse().ok(), &map, level)
    }

    #[test]
    fn only_local_clients_are_admitted_by_default() {
        let general = GeneralConfig::default();
        for ip in [
            "127.0.0.1",
            "::1",
            "10.1.2.3",
            "172.18.0.2",
            "::ffff:192.168.1.2",
            "fd00::1",
        ] {
            assert!(access(&general, ip, &[], ACCESS_WEB_UI), "{ip}");
        }
        for ip in ["203.0.113.5", "0.0.0.0", "100.64.0.1", "2001:4860::1", ""] {
            assert!(!access(&general, ip, &[], ACCESS_WEB_UI), "{ip}");
        }
    }

    #[test]
    fn forwarded_addresses_must_all_be_local() {
        let mut general = GeneralConfig::default();
        let check = |general: &GeneralConfig, peer: &str, xff: &[&str]| {
            access(general, peer, xff, ACCESS_WEB_UI)
        };
        assert!(check(&general, "172.18.0.2", &["192.168.1.5, 10.0.0.1"]));
        assert!(!check(&general, "172.18.0.2", &["192.168.1.5, 8.8.8.8"]));
        assert!(!check(&general, "172.18.0.2", &["192.168.1.5", "8.8.8.8"]));
        assert!(!check(&general, "172.18.0.2", &["unknown"]));
        // It never grants what the peer alone would not get.
        assert!(!check(&general, "8.8.8.8", &["192.168.1.5"]));
        general.verify_xff_header = false;
        assert!(check(&general, "172.18.0.2", &["8.8.8.8"]));
    }

    #[test]
    fn exposure_and_local_ranges_change_who_is_admitted() {
        let mut general = GeneralConfig {
            local_ranges: vec!["192.168.1.0/24".into()],
            ..GeneralConfig::default()
        };
        assert!(access(&general, "192.168.1.9", &[], ACCESS_WEB_UI));
        assert!(!access(&general, "192.168.2.9", &[], ACCESS_WEB_UI));
        assert!(access(&general, "127.0.0.1", &[], ACCESS_WEB_UI));
        general.inet_exposure = 3;
        assert!(access(&general, "8.8.8.8", &[], ACCESS_API));
        assert!(!access(&general, "8.8.8.8", &[], ACCESS_WEB_UI));
        general.inet_exposure = 5;
        assert!(access(&general, "8.8.8.8", &[], ACCESS_WEB_UI));
        assert!(!access(&general, "8.8.8.8", &[], ACCESS_LOCAL_ONLY));
    }

    #[test]
    fn host_names_must_be_known() {
        let general = GeneralConfig {
            host_whitelist: vec!["NZB.example.com".into()],
            ..GeneralConfig::default()
        };
        for host in [
            "localhost:9090",
            "192.168.1.2:9090",
            "[fd00::1]:9090",
            "[fd00::1]",
            "nas.local",
            "nzb.example.com",
        ] {
            assert!(
                check_hostname(&general, &headers(&[("host", host)])),
                "{host}"
            );
        }
        assert!(!check_hostname(
            &general,
            &headers(&[("host", "evil.example")])
        ));
        assert!(!check_hostname(&general, &HeaderMap::new()));
    }

    #[test]
    fn only_browser_writes_from_another_site_are_cross_site() {
        let check = |method: Method, pairs: &[(&'static str, &str)]| {
            cross_site_write(&method, &headers(pairs))
        };
        assert!(!check(Method::GET, &[("sec-fetch-site", "cross-site")]));
        assert!(!check(Method::POST, &[]), "not a browser");
        assert!(!check(Method::POST, &[("sec-fetch-site", "same-origin")]));
        assert!(check(Method::POST, &[("sec-fetch-site", "cross-site")]));
        // Plain HTTP: browsers send Origin but no Sec-Fetch-Site.
        assert!(check(Method::POST, &[("origin", "http://evil.example")]));
        assert!(!check(
            Method::POST,
            &[
                ("origin", "http://nzb.lan"),
                ("x-requested-with", "rustnzb")
            ]
        ));
    }
}
