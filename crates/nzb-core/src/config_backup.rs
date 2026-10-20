//! Export and restore of the configuration as one TOML document.
//!
//! A backup is a `config.toml` under a `[config]` table, with a `[backup]`
//! header describing the file:
//!
//! ```toml
//! [backup]
//! backup_version = 1
//! created_at = "2026-10-20T09:00:00Z"
//! app_version = "1.6.0"
//! secrets_included = false
//!
//! [config]
//! config_version = 1
//!
//! [config.general]
//! ...
//! ```
//!
//! # Secrets
//!
//! The default export replaces every stored secret with [`SECRET_MASK`]
//! (`********`) and sets `secrets_included = false`. That file can be shared,
//! committed or attached to an issue without exposing a credential. An export
//! with real secrets is opt-in, and the document then says so.
//!
//! On restore the mask is a *keep token*: a masked secret keeps the value the
//! target already holds, so restoring a masked backup onto a fresh install
//! leaves those credentials unset instead of writing the literal mask. A
//! secret that is absent means absent — restoring a real backup therefore
//! clears a secret that was not in it, which is what a restore means. A mask
//! with nothing to keep is reported as a warning, never written through.

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::{
    AppConfig, CategoryConfig, DavConfig, NotificationConfig, OtelConfig, ServerConfig,
    normalize_history_retention, normalize_rss_expiry_days,
};

/// Schema version written as `backup_version` in every backup file.
pub const BACKUP_VERSION: u32 = 1;

/// Placeholder the API and a masked backup use in place of a stored secret.
pub const SECRET_MASK: &str = "********";

/// The `[backup]` header of a backup document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupMeta {
    /// Schema version of the backup envelope, independent of the config's own
    /// `config_version`. A file from a newer build is refused, not guessed at.
    /// The serde default is 0, so a document that omits it is refused rather
    /// than silently read as the current version.
    #[serde(default)]
    pub backup_version: u32,
    /// When the file was written, RFC 3339 in UTC.
    pub created_at: String,
    /// The rustnzb build that wrote it.
    pub app_version: String,
    /// Whether real credentials are in the file or [`SECRET_MASK`] is.
    pub secrets_included: bool,
}

/// One backup file: the header plus the configuration it carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupDocument {
    pub backup: BackupMeta,
    pub config: AppConfig,
}

/// What a restore did, reported to the caller.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreSummary {
    pub server_count: usize,
    pub category_count: usize,
    pub rss_feed_count: usize,
    /// Stored secrets the masked file kept in place of its masks.
    pub secrets_kept: usize,
    /// Things the operator should know about the restored file.
    pub warnings: Vec<String>,
}

/// Render `config` as a backup file. With `include_secrets` false every stored
/// secret is replaced by [`SECRET_MASK`].
pub fn export(
    config: &AppConfig,
    app_version: &str,
    created_at: &str,
    include_secrets: bool,
) -> anyhow::Result<String> {
    let body = if include_secrets {
        config.clone()
    } else {
        mask_secrets(config)
    };
    let mut body = body;
    // A backup is a portable document, not this process: drop the remembered
    // pre-debug level, which only means something alongside the process that
    // toggled it. The saved level itself is kept.
    body.general.log_level_before_debug = None;
    let document = BackupDocument {
        backup: BackupMeta {
            backup_version: BACKUP_VERSION,
            created_at: created_at.to_string(),
            app_version: app_version.to_string(),
            secrets_included: include_secrets,
        },
        config: body,
    };
    toml::to_string_pretty(&document).context("serialize the config backup")
}

/// Parse a backup file. Refuses a newer envelope, a newer config schema, and
/// any key the running build does not know.
pub fn parse(text: &str) -> Result<BackupDocument, String> {
    let document: BackupDocument = toml::from_str(text).map_err(|e| parse_error(text, &e))?;
    validate_document(&document)?;
    Ok(document)
}

/// Describe a parse failure without echoing the offending source line: a
/// syntax error next to a password in a secrets-included file must not be
/// copied into the response. The message and its position are enough.
fn parse_error(text: &str, error: &toml::de::Error) -> String {
    let location = error.span().and_then(|span| {
        let prefix = text.get(..span.start)?;
        let line = prefix.matches('\n').count() + 1;
        let column = prefix
            .rsplit('\n')
            .next()
            .map_or(1, |s| s.chars().count() + 1);
        Some(format!(" at line {line}, column {column}"))
    });
    match location {
        Some(location) => format!("not a valid rustnzb backup{location}: {}", error.message()),
        None => format!("not a valid rustnzb backup: {}", error.message()),
    }
}

fn validate_document(document: &BackupDocument) -> Result<(), String> {
    let version = document.backup.backup_version;
    if version == 0 {
        return Err("backup_version is missing; this is not a rustnzb backup".into());
    }
    if version > BACKUP_VERSION {
        return Err(format!(
            "backup_version {version} was written by a newer rustnzb; this build reads up to {BACKUP_VERSION}"
        ));
    }
    if document.config.config_version > crate::config_store::CONFIG_VERSION {
        return Err(format!(
            "config_version {} is newer than this build supports ({}); upgrade before restoring",
            document.config.config_version,
            crate::config_store::CONFIG_VERSION
        ));
    }
    Ok(())
}

/// Normalize a restored configuration the same way `AppConfig::read` does, so
/// a hand-written backup behaves exactly like a hand-written `config.toml`.
pub fn normalize(config: &mut AppConfig) {
    config.assign_missing_server_ids();
    config.general.history_retention =
        normalize_history_retention(config.general.history_retention);
    config.general.rss_downloaded_item_expiry_days =
        normalize_rss_expiry_days(config.general.rss_downloaded_item_expiry_days);
    crate::config_store::migrate(config);
}

/// Every problem that would stop the restored configuration from running.
/// The messages are phrased with the setting's path, as `AppConfig::validate`
/// does, and nothing is written when the list is non-empty.
pub fn validate(config: &AppConfig) -> Vec<String> {
    let mut problems = config.validate();
    for (i, category) in config.categories.iter().enumerate() {
        if let Err(reason) = category.validate() {
            problems.push(format!("categories[{i}] ({}): {reason}", category.name));
        }
    }
    problems.extend(check_masks(config));
    problems
}

/// Refuse a document that still carries a mask after the merge. Simple
/// secrets are cleared or kept by then; a mask left in a URL means this
/// install had no stored value to keep, and writing `********` into a feed or
/// collector URL would only move the breakage into the saved configuration.
fn check_masks(config: &AppConfig) -> Vec<String> {
    let mut problems = Vec::new();
    {
        let mut exact = |field: &str, value: Option<&str>| {
            if value == Some(SECRET_MASK) {
                problems.push(format!(
                    "{field} still contains the mask; this install has no stored value to keep"
                ));
            }
        };
        exact("general.api_key", config.general.api_key.as_deref());
        exact("dav.password", config.dav.password.as_deref());
        exact("dav.api_key", config.dav.api_key.as_deref());
        exact(
            "notifications.webhook_url",
            Some(config.notifications.webhook_url.as_str()),
        );
        exact(
            "notifications.apprise_api_url",
            Some(config.notifications.apprise_api_url.as_str()),
        );
        exact(
            "notifications.apprise_urls",
            Some(config.notifications.apprise_urls.as_str()),
        );
        exact(
            "notifications.ntfy_token",
            Some(config.notifications.ntfy_token.as_str()),
        );
        exact(
            "notifications.ntfy_topic",
            Some(config.notifications.ntfy_topic.as_str()),
        );
        for (i, server) in config.servers.iter().enumerate() {
            exact(
                &format!("servers[{i}].password"),
                server.password.as_deref(),
            );
        }
    }
    for (i, server) in config.servers.iter().enumerate() {
        if server
            .proxy_url
            .as_deref()
            .is_some_and(|url| url.contains(&format!("{SECRET_MASK}@")))
        {
            problems.push(format!(
                "servers[{i}].proxy_url still contains the mask; this install has no matching proxy to keep"
            ));
        }
    }
    for (i, feed) in config.rss_feeds.iter().enumerate() {
        if feed.url.contains(SECRET_MASK) {
            problems.push(format!(
                "rss_feeds[{i}] ({}): the URL still contains the mask and this install has no matching feed URL to keep",
                feed.name
            ));
        }
    }
    for (field, value) in [
        ("otel.endpoint", Some(config.otel.endpoint.as_str())),
        ("otel.logs_endpoint", config.otel.logs_endpoint.as_deref()),
        (
            "otel.metrics_endpoint",
            config.otel.metrics_endpoint.as_deref(),
        ),
    ] {
        if value.is_some_and(|url| url.contains(&format!("{SECRET_MASK}@"))) {
            problems.push(format!(
                "{field} still contains the mask; this install has no matching endpoint to keep"
            ));
        }
    }
    problems
}

/// Replace every stored secret in `config` with [`SECRET_MASK`].
///
/// Covers the SABnzbd API key, DAV credentials, server passwords, the
/// `user:pass@` part of a server proxy URL, notification URLs and the ntfy
/// token and topic, RSS feed URLs (whose query string normally carries the
/// indexer key), and the userinfo of OTLP endpoints.
pub fn mask_secrets(config: &AppConfig) -> AppConfig {
    let mut config = config.clone();
    if config.general.api_key.is_some() {
        config.general.api_key = Some(SECRET_MASK.to_string());
    }
    mask_dav(&mut config.dav);
    mask_notifications(&mut config.notifications);
    mask_otel(&mut config.otel);
    for server in &mut config.servers {
        if server.password.is_some() {
            server.password = Some(SECRET_MASK.to_string());
        }
        if let Some(url) = server.proxy_url.as_deref() {
            server.proxy_url = Some(mask_proxy_userinfo(url));
        }
    }
    for feed in &mut config.rss_feeds {
        feed.url = mask_url_secrets(&feed.url);
    }
    config
}

fn mask_dav(dav: &mut DavConfig) {
    if dav.password.is_some() {
        dav.password = Some(SECRET_MASK.to_string());
    }
    if dav.api_key.is_some() {
        dav.api_key = Some(SECRET_MASK.to_string());
    }
}

fn mask_notifications(notifications: &mut NotificationConfig) {
    for secret in [
        &mut notifications.webhook_url,
        &mut notifications.apprise_api_url,
        &mut notifications.apprise_urls,
        &mut notifications.ntfy_token,
        &mut notifications.ntfy_topic,
    ] {
        if !secret.is_empty() {
            *secret = SECRET_MASK.to_string();
        }
    }
}

fn mask_otel(otel: &mut OtelConfig) {
    otel.endpoint = mask_proxy_userinfo(&otel.endpoint);
    if let Some(endpoint) = otel.logs_endpoint.clone() {
        otel.logs_endpoint = Some(mask_proxy_userinfo(&endpoint));
    }
    if let Some(endpoint) = otel.metrics_endpoint.clone() {
        otel.metrics_endpoint = Some(mask_proxy_userinfo(&endpoint));
    }
}

/// Replace the `user:pass@` part and the query string of a URL with the mask.
/// Scheme, host and path survive, so a file still says which feed it is.
/// A URL without userinfo or a query is returned unchanged.
pub fn mask_url_secrets(url: &str) -> String {
    let masked = mask_proxy_userinfo(url);
    let (before_fragment, fragment) = match masked.split_once('#') {
        Some((head, tail)) => (head, Some(tail)),
        None => (masked.as_str(), None),
    };
    let mut out = match before_fragment.find('?') {
        Some(query_start) => format!("{}?{SECRET_MASK}", &before_fragment[..query_start]),
        None => before_fragment.to_string(),
    };
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Replace the `user:pass@` part of a proxy URL with the mask. A URL without
/// userinfo, or without a scheme, is returned unchanged.
pub fn mask_proxy_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://").map(|i| i + 3) else {
        return url.to_string();
    };
    let rest = &url[scheme_end..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return url.to_string();
    };
    format!("{}{SECRET_MASK}@{}", &url[..scheme_end], &rest[at + 1..])
}

/// Whether a restored value is the mask, meaning "keep the stored secret".
fn is_mask(value: Option<&str>) -> bool {
    value == Some(SECRET_MASK)
}

/// Keep the target's stored secrets wherever `incoming` carries a mask.
///
/// The returned configuration is what will be saved; `summary.secrets_kept`
/// counts the masks that found something to keep, and each mask that found
/// nothing becomes a warning rather than being written through as a literal
/// `********` password.
pub fn merge_secrets(incoming: &mut AppConfig, stored: &AppConfig, summary: &mut RestoreSummary) {
    if is_mask(incoming.general.api_key.as_deref()) {
        match &stored.general.api_key {
            Some(existing) => {
                incoming.general.api_key = Some(existing.clone());
                summary.secrets_kept += 1;
            }
            None => {
                incoming.general.api_key = None;
                summary
                    .warnings
                    .push("general.api_key was masked and this install has no key to keep; it stays unset".into());
            }
        }
    }
    if is_mask(incoming.dav.password.as_deref()) {
        match &stored.dav.password {
            Some(existing) => {
                incoming.dav.password = Some(existing.clone());
                summary.secrets_kept += 1;
            }
            None => {
                incoming.dav.password = None;
                summary.warnings.push(
                    "dav.password was masked and this install has no password to keep; it stays unset"
                        .into(),
                );
            }
        }
    }
    if is_mask(incoming.dav.api_key.as_deref()) {
        match &stored.dav.api_key {
            Some(existing) => {
                incoming.dav.api_key = Some(existing.clone());
                summary.secrets_kept += 1;
            }
            None => {
                incoming.dav.api_key = None;
                summary.warnings.push(
                    "dav.api_key was masked and this install has no key to keep; it stays unset"
                        .into(),
                );
            }
        }
    }
    for (field, incoming_value, stored_value) in [
        (
            "notifications.webhook_url",
            &mut incoming.notifications.webhook_url,
            &stored.notifications.webhook_url,
        ),
        (
            "notifications.apprise_api_url",
            &mut incoming.notifications.apprise_api_url,
            &stored.notifications.apprise_api_url,
        ),
        (
            "notifications.apprise_urls",
            &mut incoming.notifications.apprise_urls,
            &stored.notifications.apprise_urls,
        ),
        (
            "notifications.ntfy_token",
            &mut incoming.notifications.ntfy_token,
            &stored.notifications.ntfy_token,
        ),
        (
            "notifications.ntfy_topic",
            &mut incoming.notifications.ntfy_topic,
            &stored.notifications.ntfy_topic,
        ),
    ] {
        if incoming_value == SECRET_MASK {
            if stored_value.is_empty() {
                incoming_value.clear();
                summary.warnings.push(format!(
                    "{field} was masked and this install has no value to keep; it stays empty"
                ));
            } else {
                *incoming_value = stored_value.clone();
                summary.secrets_kept += 1;
            }
        }
    }
    if incoming.otel.endpoint.contains(SECRET_MASK) {
        if incoming.otel.endpoint == mask_proxy_userinfo(&stored.otel.endpoint) {
            incoming.otel.endpoint = stored.otel.endpoint.clone();
            summary.secrets_kept += 1;
        } else {
            incoming.otel.endpoint = OtelConfig::default().endpoint;
            summary.warnings.push(
                "otel.endpoint is masked and this install has no matching endpoint to keep; it returns to the default"
                    .into(),
            );
        }
    }
    for (field, incoming_endpoint, stored_endpoint) in [
        (
            "otel.logs_endpoint",
            &mut incoming.otel.logs_endpoint,
            stored.otel.logs_endpoint.as_deref(),
        ),
        (
            "otel.metrics_endpoint",
            &mut incoming.otel.metrics_endpoint,
            stored.otel.metrics_endpoint.as_deref(),
        ),
    ] {
        let Some(current) = incoming_endpoint.as_deref() else {
            continue;
        };
        if !current.contains(SECRET_MASK) {
            continue;
        }
        match stored_endpoint {
            Some(stored_url) if current == mask_proxy_userinfo(stored_url) => {
                *incoming_endpoint = Some(stored_url.to_string());
                summary.secrets_kept += 1;
            }
            _ => {
                *incoming_endpoint = None;
                summary
                    .warnings
                    .push(format!("{field} is masked and this install has no matching endpoint to keep; it stays unset"));
            }
        }
    }
    // A feed whose URL carries a masked key keeps the stored URL when the
    // masked form names the same feed. Without one there is nothing to restore
    // and an empty URL would only break polling, so the feed is dropped rather
    // than saved half-configured; the warning says so.
    let mut kept_feeds = Vec::with_capacity(incoming.rss_feeds.len());
    for mut feed in std::mem::take(&mut incoming.rss_feeds) {
        if !feed.url.contains(SECRET_MASK) {
            kept_feeds.push(feed);
            continue;
        }
        match stored.rss_feeds.iter().find(|stored_feed| {
            stored_feed.name == feed.name && mask_url_secrets(&stored_feed.url) == feed.url
        }) {
            Some(stored_feed) => {
                feed.url = stored_feed.url.clone();
                summary.secrets_kept += 1;
                kept_feeds.push(feed);
            }
            None => summary.warnings.push(format!(
                "RSS feed `{}` has a masked URL and this install has no matching feed; the feed was dropped, so add it again with its URL",
                feed.name
            )),
        }
    }
    incoming.rss_feeds = kept_feeds;
    for server in &mut incoming.servers {
        let Some(existing) = stored.servers.iter().find(|s| s.id == server.id) else {
            if is_mask(server.password.as_deref()) {
                server.password = None;
                summary.warnings.push(format!(
                    "server `{}` was masked and is not configured here; its password stays unset",
                    server_label(server)
                ));
            }
            if server
                .proxy_url
                .as_deref()
                .is_some_and(|url| url.contains(&format!("{SECRET_MASK}@")))
            {
                server.proxy_url = None;
                summary.warnings.push(format!(
                    "server `{}` has a masked proxy URL and is not configured here; the proxy stays unset",
                    server_label(server)
                ));
            }
            continue;
        };
        if is_mask(server.password.as_deref()) {
            match &existing.password {
                Some(password) => {
                    server.password = Some(password.clone());
                    summary.secrets_kept += 1;
                }
                None => {
                    server.password = None;
                    summary.warnings.push(format!(
                        "server `{}` was masked and has no stored password to keep",
                        server_label(server)
                    ));
                }
            }
        }
        if let Some(url) = server.proxy_url.as_deref() {
            if Some(url)
                == existing
                    .proxy_url
                    .as_deref()
                    .map(mask_proxy_userinfo)
                    .as_deref()
            {
                server.proxy_url = existing.proxy_url.clone();
                summary.secrets_kept += 1;
            } else if url.contains(&format!("{SECRET_MASK}@")) {
                server.proxy_url = None;
                summary.warnings.push(format!(
                    "server `{}` has a masked proxy URL that does not match this install's; the proxy stays unset",
                    server_label(server)
                ));
            }
        }
    }
}

fn server_label(server: &ServerConfig) -> String {
    if server.name.trim().is_empty() {
        server.id.clone()
    } else {
        format!("{} ({})", server.name, server.id)
    }
}

/// Build the summary for a configuration that is about to be restored or,
/// with `dry_run`, merely previewed.
pub fn summarize(config: &AppConfig, warnings: Vec<String>) -> RestoreSummary {
    RestoreSummary {
        server_count: config.servers.len(),
        category_count: config.categories.len(),
        rss_feed_count: config.rss_feeds.len(),
        secrets_kept: 0,
        warnings,
    }
}

/// Whether a parsed category list is one the queue manager can use: at least
/// one entry, and `AppConfig`'s invariant is that a job always resolves to a
/// category. An empty list is refused instead of being silently repaired,
/// because the next enqueue would otherwise panic.
pub fn check_categories(categories: &[CategoryConfig]) -> Result<(), String> {
    if categories.is_empty() {
        return Err("categories must contain at least one entry (the default `Default`)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const API_KEY: &str = "canary-api-key-111";
    const SERVER_PASSWORD: &str = "canary-server-password-222";
    const DAV_PASSWORD: &str = "canary-dav-password-333";
    const DAV_KEY: &str = "canary-dav-key-444";
    const PROXY_SECRET: &str = "canary-proxy-secret-555";
    const WEBHOOK: &str = "https://hooks.example/canary-webhook-666";
    const NTFY: &str = "canary-ntfy-token-777";
    const NTFY_TOPIC: &str = "canary-ntfy-topic-788";
    const RSS_KEY: &str = "canary-rss-key-888";
    const OTEL_SECRET: &str = "canary-otel-secret-999";

    fn seeded() -> AppConfig {
        let mut config = AppConfig::default();
        config.general.api_key = Some(API_KEY.into());
        config.general.log_level_before_debug = Some("info".into());
        let mut server = ServerConfig::new("srv-1", "news.example.com");
        server.name = "Primary".into();
        server.password = Some(SERVER_PASSWORD.into());
        server.proxy_url = Some(format!(
            "socks5://user:{PROXY_SECRET}@proxy.example.net:1080"
        ));
        config.servers.push(server);
        config.categories.push(CategoryConfig {
            name: "TV".into(),
            output_dir: Some("tv".into()),
            ..CategoryConfig::default()
        });
        config.dav.password = Some(DAV_PASSWORD.into());
        config.dav.api_key = Some(DAV_KEY.into());
        config.notifications.webhook_url = WEBHOOK.into();
        config.notifications.ntfy_token = NTFY.into();
        config.notifications.ntfy_topic = NTFY_TOPIC.into();
        config.rss_feeds.push(crate::config::RssFeedConfig {
            name: "daily".into(),
            url: format!("https://indexer.example/rss?t=2000&apikey={RSS_KEY}&i=42"),
            poll_interval_secs: 900,
            category: None,
            filter_regex: None,
            enabled: true,
            auto_download: false,
            max_age_days: None,
        });
        config.otel.endpoint = format!("https://collector:{OTEL_SECRET}@otel.example:4317");
        config
    }

    fn assert_no_canary(text: &str) {
        // Name the field that leaked, never the value: a failing assertion must
        // not put a credential in the test log.
        for (field, secret) in [
            ("api_key", API_KEY),
            ("server_password", SERVER_PASSWORD),
            ("dav_password", DAV_PASSWORD),
            ("dav_api_key", DAV_KEY),
            ("proxy_secret", PROXY_SECRET),
            ("webhook_url", WEBHOOK),
            ("ntfy_token", NTFY),
            ("ntfy_topic", NTFY_TOPIC),
            ("rss_key", RSS_KEY),
            ("otel_secret", OTEL_SECRET),
        ] {
            assert!(!text.contains(secret), "a masked backup leaked {field}");
        }
    }

    #[test]
    fn masked_export_carries_no_secret_and_says_so() {
        let text = export(&seeded(), "1.6.0", "2026-10-20T00:00:00Z", false).unwrap();
        assert_no_canary(&text);
        assert!(text.contains("secrets_included = false"));
        assert!(text.contains(SECRET_MASK));

        let document = parse(&text).unwrap();
        assert!(!document.backup.secrets_included);
        assert_eq!(document.backup.backup_version, BACKUP_VERSION);
        assert_eq!(document.backup.app_version, "1.6.0");
        assert_eq!(document.backup.created_at, "2026-10-20T00:00:00Z");
        // Non-secret settings survive.
        assert_eq!(document.config.general.port, 9090);
        assert_eq!(document.config.servers[0].host, "news.example.com");
        assert_eq!(document.config.categories.len(), 2);
        // The process-local pre-debug level is not part of a backup.
        assert!(document.config.general.log_level_before_debug.is_none());
    }

    #[test]
    fn export_with_secrets_round_trips_every_value() {
        let original = seeded();
        let text = export(&original, "1.6.0", "2026-10-20T00:00:00Z", true).unwrap();
        assert!(text.contains("secrets_included = true"));
        let restored = parse(&text).unwrap().config;
        assert_eq!(restored.general.api_key.as_deref(), Some(API_KEY));
        assert_eq!(
            restored.servers[0].password.as_deref(),
            Some(SERVER_PASSWORD)
        );
        assert_eq!(
            restored.servers[0].proxy_url.as_deref(),
            original.servers[0].proxy_url.as_deref()
        );
        assert_eq!(restored.dav.password.as_deref(), Some(DAV_PASSWORD));
        assert_eq!(restored.notifications.webhook_url, WEBHOOK);
        assert_eq!(restored.notifications.ntfy_topic, NTFY_TOPIC);
        assert_eq!(restored.rss_feeds[0].url, original.rss_feeds[0].url);
        assert_eq!(restored.otel.endpoint, original.otel.endpoint);
    }

    #[test]
    fn masked_export_masks_feed_keys_and_keeps_the_feed_recognisable() {
        let text = export(&seeded(), "1.6.0", "", false).unwrap();
        assert_no_canary(&text);
        let document = parse(&text).unwrap();
        let url = &document.config.rss_feeds[0].url;
        assert!(
            url.starts_with("https://indexer.example/rss?********"),
            "{url}"
        );
        assert!(!url.contains("t=2000"), "the whole query is masked: {url}");
        assert_eq!(
            document.config.otel.endpoint,
            "https://********@otel.example:4317"
        );
        assert_eq!(document.config.notifications.ntfy_topic, SECRET_MASK);
    }

    #[test]
    fn a_masked_feed_url_keeps_the_stored_url_only_for_the_same_feed() {
        let stored = seeded();
        let mut incoming = parse(&export(&stored, "1.6.0", "", false).unwrap())
            .unwrap()
            .config;
        let mut summary = summarize(&incoming, Vec::new());
        merge_secrets(&mut incoming, &stored, &mut summary);
        assert_eq!(incoming.rss_feeds[0].url, stored.rss_feeds[0].url);
        assert!(!incoming.rss_feeds[0].url.contains(SECRET_MASK));
        assert_eq!(incoming.otel.endpoint, stored.otel.endpoint);
        assert_eq!(incoming.notifications.ntfy_topic, NTFY_TOPIC);
        assert!(summary.warnings.is_empty(), "{summary:?}");

        // A masked feed the target does not know is dropped, not saved with a
        // literal mask that would break every poll.
        let mut incoming = parse(&export(&stored, "1.6.0", "", false).unwrap())
            .unwrap()
            .config;
        incoming.rss_feeds[0].name = "other".into();
        let mut summary = summarize(&incoming, Vec::new());
        merge_secrets(&mut incoming, &AppConfig::default(), &mut summary);
        assert!(incoming.rss_feeds.is_empty());
        assert!(
            summary.warnings.iter().any(|w| w.contains("RSS feed")),
            "{summary:?}"
        );
        assert!(validate(&incoming).is_empty(), "{:?}", validate(&incoming));
    }

    #[test]
    fn a_masked_url_with_no_stored_value_is_refused_rather_than_saved() {
        let mut config = AppConfig::default();
        config.rss_feeds.push(crate::config::RssFeedConfig {
            name: "daily".into(),
            url: "https://indexer.example/rss?********".into(),
            poll_interval_secs: 900,
            category: None,
            filter_regex: None,
            enabled: true,
            auto_download: false,
            max_age_days: None,
        });
        config.notifications.ntfy_topic = SECRET_MASK.into();
        let problems = validate(&config);
        assert!(
            problems.iter().any(|p| p.contains("rss_feeds[0]")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("notifications.ntfy_topic")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_masked_backup_keeps_the_secrets_the_target_already_has() {
        let stored = seeded();
        let mut incoming = parse(&export(&stored, "1.6.0", "", false).unwrap())
            .unwrap()
            .config;
        let mut summary = summarize(&incoming, Vec::new());
        merge_secrets(&mut incoming, &stored, &mut summary);

        assert_eq!(incoming.general.api_key.as_deref(), Some(API_KEY));
        assert_eq!(
            incoming.servers[0].password.as_deref(),
            Some(SERVER_PASSWORD)
        );
        assert_eq!(
            incoming.servers[0].proxy_url.as_deref(),
            stored.servers[0].proxy_url.as_deref()
        );
        assert_eq!(incoming.dav.password.as_deref(), Some(DAV_PASSWORD));
        assert_eq!(incoming.notifications.webhook_url, WEBHOOK);
        assert_eq!(incoming.notifications.ntfy_token, NTFY);
        assert_eq!(incoming.notifications.ntfy_topic, NTFY_TOPIC);
        assert_eq!(summary.secrets_kept, 10, "{summary:?}");
        assert!(summary.warnings.is_empty(), "{summary:?}");
        assert!(
            !incoming.servers[0]
                .proxy_url
                .as_deref()
                .unwrap()
                .contains(SECRET_MASK)
        );
    }

    #[test]
    fn a_masked_backup_onto_a_fresh_install_warns_and_leaves_secrets_unset() {
        let stored = AppConfig::default();
        let mut incoming = parse(&export(&seeded(), "1.6.0", "", false).unwrap())
            .unwrap()
            .config;
        let mut summary = summarize(&incoming, Vec::new());
        merge_secrets(&mut incoming, &stored, &mut summary);

        assert!(incoming.general.api_key.is_none());
        assert!(incoming.servers[0].password.is_none());
        assert!(incoming.servers[0].proxy_url.is_none());
        assert!(incoming.notifications.webhook_url.is_empty());
        assert_eq!(summary.secrets_kept, 0);
        // One warning per mask with nothing behind it.
        assert_eq!(summary.warnings.len(), 10, "{summary:?}");
        assert!(summary.warnings.iter().all(|w| w.contains("masked")));
        // The literal mask is never written through.
        for server in &incoming.servers {
            assert_ne!(server.password.as_deref(), Some(SECRET_MASK));
        }
    }

    #[test]
    fn secrets_absent_from_a_real_backup_are_cleared() {
        let stored = seeded();
        let mut incoming = AppConfig::default();
        incoming
            .servers
            .push(ServerConfig::new("srv-1", "news.example.com"));
        let mut summary = summarize(&incoming, Vec::new());
        merge_secrets(&mut incoming, &stored, &mut summary);
        assert!(incoming.general.api_key.is_none());
        assert!(incoming.servers[0].password.is_none());
        assert_eq!(summary.secrets_kept, 0);
        // Nothing was masked, so nothing is reported.
        assert!(summary.warnings.is_empty(), "{summary:?}");
    }

    #[test]
    fn a_masked_server_that_does_not_exist_here_is_not_given_another_servers_password() {
        let stored = seeded();
        let mut incoming = stored.clone();
        incoming.servers[0].id = "other-id".into();
        incoming.servers[0].password = Some(SECRET_MASK.into());
        incoming.servers[0].proxy_url =
            Some(format!("socks5://{SECRET_MASK}@proxy.example.net:1080"));
        let mut summary = summarize(&incoming, Vec::new());
        merge_secrets(&mut incoming, &stored, &mut summary);
        assert!(incoming.servers[0].password.is_none());
        assert!(incoming.servers[0].proxy_url.is_none());
        assert_eq!(summary.warnings.len(), 2, "{summary:?}");
    }

    #[test]
    fn a_masked_proxy_that_does_not_match_is_cleared_not_written_through() {
        let stored = seeded();
        let mut incoming = parse(&export(&stored, "1.6.0", "", false).unwrap())
            .unwrap()
            .config;
        incoming.servers[0].proxy_url =
            Some(format!("socks5://{SECRET_MASK}@elsewhere.example:1080"));
        let mut summary = summarize(&incoming, Vec::new());
        merge_secrets(&mut incoming, &stored, &mut summary);
        assert!(incoming.servers[0].proxy_url.is_none());
        assert!(
            summary.warnings.iter().any(|w| w.contains("proxy")),
            "{summary:?}"
        );
    }

    #[test]
    fn envelope_requires_a_version_and_refuses_unknown_header_keys() {
        let text = export(&AppConfig::default(), "1.6.0", "", false).unwrap();
        let without_version = text.replace("backup_version = 1\n", "");
        let error = parse(&without_version).unwrap_err();
        assert!(error.contains("backup_version is missing"), "{error}");

        let typo = text.replace(
            "secrets_included = false",
            "secrets_included = false\ntypo = 1",
        );
        let error = parse(&typo).unwrap_err();
        assert!(error.contains("not a valid rustnzb backup"), "{error}");
    }

    #[test]
    fn a_parse_error_does_not_echo_the_offending_line() {
        let text =
            format!("[backup]\nbackup_version = 1\n\n[config.general]\napi_key = \"{API_KEY}\n");
        let error = parse(&text).unwrap_err();
        assert!(error.contains("not a valid rustnzb backup"), "{error}");
        assert!(
            !error.contains(API_KEY),
            "the source line was echoed: {error}"
        );
    }

    #[test]
    fn parse_refuses_a_newer_envelope_or_config_schema() {
        let mut text = export(&AppConfig::default(), "1.6.0", "", false).unwrap();
        text = text.replace("backup_version = 1", "backup_version = 99");
        let error = parse(&text).unwrap_err();
        assert!(error.contains("newer rustnzb"), "{error}");

        let mut text = export(&AppConfig::default(), "1.6.0", "", false).unwrap();
        text = text.replace("config_version = 1", "config_version = 99");
        let error = parse(&text).unwrap_err();
        assert!(error.contains("config_version"), "{error}");

        let error = parse("[general]\nport = 9090\n").unwrap_err();
        assert!(error.contains("not a valid rustnzb backup"), "{error}");
    }

    #[test]
    fn parse_refuses_an_unknown_key_instead_of_dropping_it() {
        let text = export(&AppConfig::default(), "1.6.0", "", false)
            .unwrap()
            .replace("port = 9090", "port = 9090\ntypo_setting = true");
        let error = parse(&text).unwrap_err();
        assert!(error.contains("typo_setting"), "{error}");
    }

    #[test]
    fn validate_reports_bad_values_and_empty_categories() {
        let mut config = AppConfig::default();
        config.general.port = 0;
        config.servers.push(ServerConfig::new("", ""));
        config.categories.clear();
        let problems = validate(&config);
        assert!(
            problems.iter().any(|p| p.contains("general.port")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("host must not be empty")),
            "{problems:?}"
        );
        assert!(check_categories(&config.categories).is_err());
    }

    #[test]
    fn normalize_derives_missing_server_ids_and_folds_zero_retention() {
        let mut config = AppConfig::default();
        config
            .servers
            .push(ServerConfig::new("", "news.example.com"));
        config.general.history_retention = Some(0);
        normalize(&mut config);
        assert_eq!(config.servers[0].id, "news.example.com-563");
        assert!(config.general.history_retention.is_none());
        assert_eq!(config.config_version, crate::config_store::CONFIG_VERSION);
    }

    #[test]
    fn proxy_masking_only_touches_userinfo() {
        assert_eq!(
            mask_proxy_userinfo("socks5://user:pass@proxy.example.net:1080"),
            "socks5://********@proxy.example.net:1080"
        );
        assert_eq!(
            mask_proxy_userinfo("socks5://proxy.example.net:1080"),
            "socks5://proxy.example.net:1080"
        );
        assert_eq!(mask_proxy_userinfo("not a url"), "not a url");
    }
}
