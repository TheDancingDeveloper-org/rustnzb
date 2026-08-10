//! SABnzbd configuration importer.
//!
//! Parses SABnzbd INI files and API responses into a preview structure
//! that can be reviewed, edited, and applied to rustnzb.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::{CategoryConfig, RssFeedConfig, ServerConfig};

// ---------------------------------------------------------------------------
// Public structs
// ---------------------------------------------------------------------------

/// Preview returned by both INI and API import — same shape for both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SabnzbdImportPreview {
    pub servers: Vec<ImportedServer>,
    pub categories: Vec<CategoryConfig>,
    pub general: ImportedGeneral,
    pub rss_feeds: Vec<RssFeedConfig>,
    /// Warnings about partially-imported features.
    pub warnings: Vec<String>,
    /// Fields/sections that were skipped entirely.
    pub skipped_fields: Vec<String>,
}

/// A server imported from SABnzbd.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedServer {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub ssl: bool,
    pub ssl_verify: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    /// True when the password was masked (imported via API).
    pub password_masked: bool,
    pub connections: u16,
    pub priority: u8,
    pub enabled: bool,
    pub retention: u32,
    pub optional: bool,
}

impl ImportedServer {
    /// Convert to a rustnzb `ServerConfig`, generating a new UUID.
    pub fn to_server_config(&self) -> ServerConfig {
        let mut c = ServerConfig::default();
        c.id = uuid::Uuid::new_v4().to_string();
        c.host = self.host.clone();
        c.name = self.name.clone();
        c.port = self.port;
        c.ssl = self.ssl;
        c.ssl_verify = self.ssl_verify;
        c.username = self.username.clone();
        c.password = self.password.clone();
        c.connections = self.connections;
        c.priority = self.priority;
        c.enabled = self.enabled;
        c.retention = self.retention;
        c.optional = self.optional;
        c
    }
}

/// General settings imported from SABnzbd.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedGeneral {
    pub api_key: Option<String>,
    pub complete_dir: Option<String>,
    pub incomplete_dir: Option<String>,
    pub speed_limit_bps: u64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Warn when an imported complete_dir/incomplete_dir isn't an absolute path.
/// rustnzb creates these directories eagerly at startup via `create_dir_all`,
/// which resolves a relative path against the process's working directory
/// rather than any intended download volume — applying such a path produces
/// a confusing crash far from the actual misconfiguration (see issue #62).
fn warn_relative_dirs(general: &ImportedGeneral, warnings: &mut Vec<String>) {
    if let Some(ref dir) = general.complete_dir
        && !std::path::Path::new(dir).is_absolute()
    {
        warnings.push(format!(
            "complete_dir '{dir}' is a relative path and must be made absolute before this import can be applied"
        ));
    }
    if let Some(ref dir) = general.incomplete_dir
        && !std::path::Path::new(dir).is_absolute()
    {
        warnings.push(format!(
            "incomplete_dir '{dir}' is a relative path and must be made absolute before this import can be applied"
        ));
    }
}

/// Parse SABnzbd bandwidth limit string (e.g. "50M", "1G", "500K", "0", "").
pub fn parse_bandwidth_limit(s: &str) -> u64 {
    let s = s.trim().trim_matches('"');
    if s.is_empty() || s == "0" {
        return 0;
    }
    let (num_part, multiplier) = if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1024u64)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['G', 'g']) {
        (n, 1024 * 1024 * 1024)
    } else {
        // Plain number = bytes/sec in SABnzbd (KB/s)
        (s, 1024u64)
    };
    num_part.trim().parse::<u64>().unwrap_or(0) * multiplier
}

/// Parse SABnzbd-style boolean ("0"/"1").
pub fn parse_ini_bool(s: &str) -> bool {
    matches!(s.trim(), "1" | "yes" | "true" | "True")
}

/// Read SABnzbd API booleans, which may be encoded as JSON booleans, numbers,
/// or strings depending on the server version.
fn parse_api_bool(value: &serde_json::Value, default: bool) -> bool {
    value
        .as_bool()
        .or_else(|| value.as_u64().map(|number| number != 0))
        .or_else(|| value.as_str().map(parse_ini_bool))
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// INI Parser
// ---------------------------------------------------------------------------

type SectionMap = HashMap<(String, String), HashMap<String, String>>;

/// Parse a raw SABnzbd INI file into section/subsection key-value maps.
fn parse_ini_sections(content: &str) -> SectionMap {
    let mut sections: SectionMap = HashMap::new();
    let mut current_section = String::new();
    let mut current_subsection = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        // [[subsection]]
        if line.starts_with("[[") && line.ends_with("]]") {
            current_subsection = line[2..line.len() - 2].to_string();
            continue;
        }

        // [section]
        if line.starts_with('[') && line.ends_with(']') {
            current_section = line[1..line.len() - 1].to_string();
            current_subsection.clear();
            continue;
        }

        // key = value
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim();
            // Strip surrounding quotes (SABnzbd uses dir = "" for empty values)
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value)
                .to_string();
            sections
                .entry((current_section.clone(), current_subsection.clone()))
                .or_default()
                .insert(key, value);
        }
    }

    sections
}

/// Known SABnzbd features we don't import.
const SKIPPED_SECTIONS: &[&str] = &["sorting", "notifications", "schedules"];

/// Parse a SABnzbd INI file into an import preview.
pub fn parse_sabnzbd_ini(content: &str) -> SabnzbdImportPreview {
    let sections = parse_ini_sections(content);
    let mut warnings = Vec::new();
    let mut skipped_fields = Vec::new();

    // --- General (from [misc]) ---
    let misc = sections
        .get(&("misc".into(), String::new()))
        .cloned()
        .unwrap_or_default();

    let general = ImportedGeneral {
        api_key: misc.get("api_key").cloned().filter(|s| !s.is_empty()),
        complete_dir: misc.get("complete_dir").cloned().filter(|s| !s.is_empty()),
        incomplete_dir: misc.get("download_dir").cloned().filter(|s| !s.is_empty()),
        speed_limit_bps: misc
            .get("bandwidth_limit")
            .map(|s| parse_bandwidth_limit(s))
            .unwrap_or(0),
    };
    warn_relative_dirs(&general, &mut warnings);

    // --- Servers (from [servers] → [[name]]) ---
    let servers: Vec<ImportedServer> = sections
        .iter()
        .filter(|((section, subsection), _)| section == "servers" && !subsection.is_empty())
        .map(|((_, _), kv)| build_imported_server(kv, false))
        .collect();

    // --- Categories (from [categories] → [[name]]) ---
    let categories: Vec<CategoryConfig> = sections
        .iter()
        .filter(|((section, subsection), _)| section == "categories" && !subsection.is_empty())
        .map(|((_, _), kv)| {
            let name = kv.get("name").map(|s| s.as_str()).unwrap_or("*");
            let name = if name == "*" { "Default" } else { name };

            // Check for scripts
            if let Some(script) = kv.get("script")
                && script != "Default"
                && !script.is_empty()
            {
                warnings.push(format!(
                    "Category '{name}': script '{script}' not imported (rustnzb doesn't support scripts)"
                ));
            }

            CategoryConfig {
                name: name.to_string(),
                output_dir: kv
                    .get("dir")
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from),
                post_processing: kv.get("pp").and_then(|s| s.parse().ok()).unwrap_or(3),
            }
        })
        .collect();

    // --- RSS feeds (from [rss] → [[name]]) ---
    let rss_feeds: Vec<RssFeedConfig> = sections
        .iter()
        .filter(|((section, subsection), _)| section == "rss" && !subsection.is_empty())
        .filter_map(|((_, subsection), kv)| {
            let url = kv
                .get("uri")
                .or_else(|| kv.get("url"))
                .cloned()
                .filter(|s| !s.is_empty())?;

            let filter_regex = kv
                .get("filter")
                .or_else(|| kv.get("filters"))
                .cloned()
                .filter(|s| !s.is_empty());

            if filter_regex.is_some() {
                warnings.push(format!(
                    "RSS feed '{subsection}': complex filter simplified to first include pattern"
                ));
            }

            Some(RssFeedConfig {
                name: subsection.clone(),
                url,
                poll_interval_secs: 900,
                category: kv.get("cat").cloned().filter(|s| !s.is_empty() && s != "*"),
                filter_regex,
                enabled: kv.get("enable").map(|s| parse_ini_bool(s)).unwrap_or(true),
                auto_download: false,
            })
        })
        .collect();

    // --- Skipped fields ---
    for &section in SKIPPED_SECTIONS {
        if sections.keys().any(|(s, _)| s == section) {
            skipped_fields.push(format!("[{section}] — not supported by rustnzb"));
        }
    }

    // Check for duplicate-detection settings
    if misc.get("no_dupes").is_some_and(|v| v != "0") {
        skipped_fields.push("Duplicate detection settings — not yet supported".into());
    }

    SabnzbdImportPreview {
        servers,
        categories,
        general,
        rss_feeds,
        warnings,
        skipped_fields,
    }
}

// ---------------------------------------------------------------------------
// API Response Parser
// ---------------------------------------------------------------------------

/// Parse the JSON response from SABnzbd's `get_config` API mode.
pub fn parse_sabnzbd_api_response(json: &serde_json::Value) -> SabnzbdImportPreview {
    let config = &json["config"];
    let misc = &config["misc"];
    let mut warnings = Vec::new();
    let mut skipped_fields = Vec::new();

    // --- General ---
    let general = ImportedGeneral {
        api_key: misc["api_key"]
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        complete_dir: misc["complete_dir"]
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        incomplete_dir: misc["download_dir"]
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        speed_limit_bps: misc["bandwidth_limit"]
            .as_str()
            .map(parse_bandwidth_limit)
            .or_else(|| misc["bandwidth_limit"].as_u64())
            .unwrap_or(0),
    };
    warn_relative_dirs(&general, &mut warnings);

    // --- Servers ---
    let servers: Vec<ImportedServer> = config["servers"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    let password = s["password"].as_str().map(|p| p.to_string());
                    let password_masked = password
                        .as_ref()
                        .is_some_and(|p| p.contains('*'));

                    if password_masked {
                        let name = s["displayname"]
                            .as_str()
                            .or(s["name"].as_str())
                            .unwrap_or("unknown");
                        warnings.push(format!(
                            "Server '{name}': password is masked (***) — you'll need to enter it manually"
                        ));
                    }

                    ImportedServer {
                        name: s["displayname"]
                            .as_str()
                            .or(s["name"].as_str())
                            .unwrap_or("")
                            .to_string(),
                        host: s["host"].as_str().unwrap_or("").to_string(),
                        port: s["port"]
                            .as_u64()
                            .or_else(|| s["port"].as_str().and_then(|p| p.parse().ok()))
                            .unwrap_or(563) as u16,
                        ssl: parse_api_bool(&s["ssl"], false),
                        ssl_verify: parse_api_bool(&s["ssl_verify"], false),
                        username: s["username"]
                            .as_str()
                            .map(|u| u.to_string())
                            .filter(|u| !u.is_empty()),
                        password: password.filter(|p| !p.is_empty()),
                        password_masked,
                        connections: s["connections"]
                            .as_u64()
                            .or_else(|| s["connections"].as_str().and_then(|c| c.parse().ok()))
                            .unwrap_or(8) as u16,
                        priority: s["priority"]
                            .as_u64()
                            .or_else(|| s["priority"].as_str().and_then(|p| p.parse().ok()))
                            .unwrap_or(0) as u8,
                        enabled: parse_api_bool(&s["enable"], true),
                        retention: s["retention"]
                            .as_u64()
                            .or_else(|| s["retention"].as_str().and_then(|r| r.parse().ok()))
                            .unwrap_or(0) as u32,
                        optional: parse_api_bool(&s["optional"], false),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // --- Categories ---
    let categories: Vec<CategoryConfig> = config["categories"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    let name = c["name"].as_str().unwrap_or("*");
                    let name = if name == "*" { "Default" } else { name };

                    if let Some(script) = c["script"].as_str()
                        && script != "Default"
                        && !script.is_empty()
                    {
                        warnings.push(format!("Category '{name}': script '{script}' not imported"));
                    }

                    CategoryConfig {
                        name: name.to_string(),
                        output_dir: c["dir"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(std::path::PathBuf::from),
                        post_processing: c["pp"]
                            .as_u64()
                            .or_else(|| c["pp"].as_str().and_then(|p| p.parse().ok()))
                            .unwrap_or(3) as u8,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // --- Skipped ---
    for &section in SKIPPED_SECTIONS {
        if config[section].is_object() || config[section].is_array() {
            skipped_fields.push(format!("[{section}] — not supported by rustnzb"));
        }
    }

    SabnzbdImportPreview {
        servers,
        categories,
        general,
        rss_feeds: Vec::new(), // SABnzbd API doesn't return RSS config in get_config
        warnings,
        skipped_fields,
    }
}

// ---------------------------------------------------------------------------
// Helpers for building servers from INI key-value maps
// ---------------------------------------------------------------------------

fn build_imported_server(kv: &HashMap<String, String>, from_api: bool) -> ImportedServer {
    let password = kv.get("password").cloned().filter(|s| !s.is_empty());
    let password_masked = from_api && password.as_ref().is_some_and(|p| p.contains('*'));

    ImportedServer {
        name: kv
            .get("displayname")
            .or(kv.get("name"))
            .cloned()
            .unwrap_or_default(),
        host: kv.get("host").cloned().unwrap_or_default(),
        port: kv.get("port").and_then(|s| s.parse().ok()).unwrap_or(563),
        ssl: kv.get("ssl").map(|s| parse_ini_bool(s)).unwrap_or(false),
        ssl_verify: kv
            .get("ssl_verify")
            .map(|s| parse_ini_bool(s))
            .unwrap_or(false),
        username: kv.get("username").cloned().filter(|s| !s.is_empty()),
        password: password.clone(),
        password_masked,
        connections: kv
            .get("connections")
            .and_then(|s| s.parse().ok())
            .unwrap_or(8),
        priority: kv.get("priority").and_then(|s| s.parse().ok()).unwrap_or(0),
        enabled: kv.get("enable").map(|s| parse_ini_bool(s)).unwrap_or(true),
        retention: kv
            .get("retention")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        optional: kv
            .get("optional")
            .map(|s| parse_ini_bool(s))
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn imports_ini_fields_and_reports_unsupported_sections() {
        let preview = parse_sabnzbd_ini(
            r#"
            [misc]
            api_key = key
            complete_dir = "/complete"
            download_dir = "/incomplete"
            bandwidth_limit = 2M
            no_dupes = 1
            [servers]
            [[primary]]
            name = Primary
            host = news.example.test
            port = 563
            ssl = 1
            username = user
            password = pass
            [categories]
            [[tv]]
            name = tv
            dir = /complete/tv
            pp = 2
            script = cleanup.py
            [rss]
            [[daily]]
            uri = https://example.test/feed
            cat = tv
            enable = yes
            [sorting]
            enabled = 1
            "#,
        );

        assert_eq!(preview.general.speed_limit_bps, 2 * 1024 * 1024);
        assert_eq!(preview.servers.len(), 1);
        assert_eq!(preview.servers[0].host, "news.example.test");
        assert_eq!(preview.categories[0].name, "tv");
        assert_eq!(preview.rss_feeds[0].category.as_deref(), Some("tv"));
        assert!(
            preview
                .warnings
                .iter()
                .any(|warning| warning.contains("cleanup.py"))
        );
        assert!(
            preview
                .skipped_fields
                .iter()
                .any(|field| field.contains("sorting"))
        );
        assert!(
            preview
                .skipped_fields
                .iter()
                .any(|field| field.contains("Duplicate"))
        );
    }

    #[test]
    fn api_import_handles_string_numbers_and_masked_passwords() {
        let preview = parse_sabnzbd_api_response(&serde_json::json!({
            "config": {
                "misc": { "bandwidth_limit": "500K" },
                "servers": [{
                    "displayname": "Primary", "host": "news.example.test",
                    "port": "563", "ssl": true, "password": "***",
                    "connections": "12", "enable": 0
                }]
            }
        }));

        assert_eq!(preview.general.speed_limit_bps, 500 * 1024);
        assert_eq!(preview.servers[0].connections, 12);
        assert!(!preview.servers[0].enabled);
        assert!(preview.servers[0].password_masked);
        assert_eq!(preview.warnings.len(), 1);
    }

    #[test]
    fn ini_import_warns_on_relative_complete_and_incomplete_dir() {
        let preview = parse_sabnzbd_ini(
            r#"
            [misc]
            complete_dir = Downloads
            download_dir = Downloads/incomplete
            "#,
        );

        assert!(
            preview
                .warnings
                .iter()
                .any(|w| w.contains("complete_dir") && w.contains("Downloads")),
            "expected a relative complete_dir warning, got: {:?}",
            preview.warnings
        );
        assert!(
            preview
                .warnings
                .iter()
                .any(|w| w.contains("incomplete_dir") && w.contains("Downloads/incomplete")),
            "expected a relative incomplete_dir warning, got: {:?}",
            preview.warnings
        );
    }

    #[test]
    fn ini_import_does_not_warn_on_absolute_complete_and_incomplete_dir() {
        let preview = parse_sabnzbd_ini(
            r#"
            [misc]
            complete_dir = /downloads/complete
            download_dir = /downloads/incomplete
            "#,
        );

        assert!(
            preview.warnings.is_empty(),
            "expected no warnings for absolute dirs, got: {:?}",
            preview.warnings
        );
    }

    #[test]
    fn api_import_warns_on_relative_complete_and_incomplete_dir() {
        let preview = parse_sabnzbd_api_response(&serde_json::json!({
            "config": {
                "misc": { "complete_dir": "Downloads", "download_dir": "Downloads/incomplete" }
            }
        }));

        assert!(
            preview
                .warnings
                .iter()
                .any(|w| w.contains("complete_dir") && w.contains("Downloads")),
            "expected a relative complete_dir warning, got: {:?}",
            preview.warnings
        );
        assert!(
            preview
                .warnings
                .iter()
                .any(|w| w.contains("incomplete_dir") && w.contains("Downloads/incomplete")),
            "expected a relative incomplete_dir warning, got: {:?}",
            preview.warnings
        );
    }

    proptest! {
        #[test]
        fn arbitrary_ini_input_never_panics(input in ".{0,4096}") {
            let preview = parse_sabnzbd_ini(&input);
            prop_assert!(preview.servers.len() <= input.lines().count());
        }

        #[test]
        fn bandwidth_parser_is_case_insensitive_for_units(value in 0u32..1_000_000) {
            prop_assert_eq!(parse_bandwidth_limit(&format!("{value}m")), value as u64 * 1024 * 1024);
            prop_assert_eq!(parse_bandwidth_limit(&format!("{value}M")), value as u64 * 1024 * 1024);
        }
    }
}
