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
    AppConfig, CategoryConfig, DavConfig, NotificationConfig, ServerConfig,
    normalize_history_retention, normalize_rss_expiry_days,
};

/// Schema version written as `backup_version` in every backup file.
pub const BACKUP_VERSION: u32 = 1;

/// Placeholder the API and a masked backup use in place of a stored secret.
pub const SECRET_MASK: &str = "********";

/// The `[backup]` header of a backup document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupMeta {
    /// Schema version of the backup envelope, independent of the config's own
    /// `config_version`. A file from a newer build is refused, not guessed at.
    pub backup_version: u32,
    /// When the file was written, RFC 3339 in UTC.
    pub created_at: String,
    /// The rustnzb build that wrote it.
    pub app_version: String,
    /// Whether real credentials are in the file or [`SECRET_MASK`] is.
    pub secrets_included: bool,
}

impl Default for BackupMeta {
    fn default() -> Self {
        Self {
            backup_version: BACKUP_VERSION,
            created_at: String::new(),
            app_version: String::new(),
            secrets_included: false,
        }
    }
}

/// One backup file: the header plus the configuration it carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    let document: BackupDocument =
        toml::from_str(text).map_err(|e| format!("not a valid rustnzb backup: {e}"))?;
    validate_document(&document)?;
    Ok(document)
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
    problems
}

/// Replace every stored secret in `config` with [`SECRET_MASK`].
///
/// Covers the SABnzbd API key, DAV credentials, server passwords, the
/// `user:pass@` part of a server proxy URL, and the notification URLs and ntfy
/// token, which commonly carry a secret in their path or query.
pub fn mask_secrets(config: &AppConfig) -> AppConfig {
    let mut config = config.clone();
    if config.general.api_key.is_some() {
        config.general.api_key = Some(SECRET_MASK.to_string());
    }
    mask_dav(&mut config.dav);
    mask_notifications(&mut config.notifications);
    for server in &mut config.servers {
        if server.password.is_some() {
            server.password = Some(SECRET_MASK.to_string());
        }
        if let Some(url) = server.proxy_url.as_deref() {
            server.proxy_url = Some(mask_proxy_userinfo(url));
        }
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
    ] {
        if !secret.is_empty() {
            *secret = SECRET_MASK.to_string();
        }
    }
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
        if let Some(url) = server.proxy_url.as_deref()
            && Some(url)
                == existing
                    .proxy_url
                    .as_deref()
                    .map(mask_proxy_userinfo)
                    .as_deref()
        {
            server.proxy_url = existing.proxy_url.clone();
            summary.secrets_kept += 1;
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
        config
    }

    fn assert_no_canary(text: &str) {
        for secret in [
            API_KEY,
            SERVER_PASSWORD,
            DAV_PASSWORD,
            DAV_KEY,
            PROXY_SECRET,
            WEBHOOK,
            NTFY,
        ] {
            assert!(
                !text.contains(secret),
                "a masked backup leaked {secret}: {text}"
            );
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
        assert_eq!(summary.secrets_kept, 7, "{summary:?}");
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
        assert_eq!(summary.warnings.len(), 7, "{summary:?}");
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
