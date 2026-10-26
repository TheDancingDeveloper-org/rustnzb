//! Values that the command line, the environment or a desktop shell set for
//! the life of one process.
//!
//! An override is applied on top of the file layer to produce the effective
//! configuration. It is never written to `config.toml`, so the file keeps the
//! value the user chose, and the override disappears when its variable does.

use std::path::PathBuf;

use crate::config::AppConfig;

/// Where an override came from, shown to the user as `cli`, `env:NAME` or
/// `desktop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideSource {
    Cli,
    Env(&'static str),
    Desktop,
}

impl std::fmt::Display for OverrideSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cli => f.write_str("cli"),
            Self::Env(name) => write!(f, "env:{name}"),
            Self::Desktop => f.write_str("desktop"),
        }
    }
}

/// One overridden setting and its value.
#[derive(Debug, Clone, PartialEq)]
pub enum OverrideValue {
    ListenAddr(String),
    Port(u16),
    DataDir(PathBuf),
    OtelEnabled(bool),
    OtelEndpoint(String),
    OtelLogsEnabled(bool),
    OtelLogsEndpoint(String),
    OtelMetricsEnabled(bool),
    OtelMetricsEndpoint(String),
    OtelServiceName(String),
    DavEnabled(bool),
    /// A scalar registry leaf set by `RUSTNZB__SECTION__KEY`.
    Json {
        key: &'static str,
        value: serde_json::Value,
    },
}

impl OverrideValue {
    /// The dotted config key this value overrides, as shown in the API.
    pub fn key(&self) -> &'static str {
        match self {
            Self::ListenAddr(_) => "general.listen_addr",
            Self::Port(_) => "general.port",
            Self::DataDir(_) => "general.data_dir",
            Self::OtelEnabled(_) => "otel.enabled",
            Self::OtelEndpoint(_) => "otel.endpoint",
            Self::OtelLogsEnabled(_) => "otel.logs_enabled",
            Self::OtelLogsEndpoint(_) => "otel.logs_endpoint",
            Self::OtelMetricsEnabled(_) => "otel.metrics_enabled",
            Self::OtelMetricsEndpoint(_) => "otel.metrics_endpoint",
            Self::OtelServiceName(_) => "otel.service_name",
            Self::DavEnabled(_) => "dav.enabled",
            Self::Json { key, .. } => key,
        }
    }
}

/// Parse `RUSTNZB__SECTION__KEY` (and a secret `…_FILE`) into a registry override.
///
/// Returns `Ok(None)` when the name is not a generic override, or when the
/// key is unknown, not a scalar, or the value does not match the registry
/// kind. `warning` is set in those ignore cases so the caller can log it.
/// A name the registry already lists in `Setting::env` is `Ok(None)` with no
/// warning: the dedicated startup mapping owns those aliases.
pub fn generic_env_override(name: &str, raw: &str) -> Result<Option<OverrideValue>, String> {
    let Some(rest) = name.strip_prefix("RUSTNZB__") else {
        return Ok(None);
    };
    if rest.is_empty() || !rest.contains("__") {
        return Ok(None);
    }
    let (key_name, from_file) = match rest.strip_suffix("_FILE") {
        Some(without) if without.contains("__") => (without, true),
        _ => (rest, false),
    };
    let dotted = key_name.to_ascii_lowercase().replace("__", ".");
    let Some(setting) = crate::config_registry::get(&dotted) else {
        return Err(format!("ignoring {name}: {dotted} is not a known setting"));
    };
    if setting.env.contains(&name) {
        return Ok(None);
    }
    if matches!(setting.kind, crate::config_registry::Kind::StringList)
        || dotted
            .split('.')
            .next()
            .is_some_and(|section| matches!(section, "servers" | "categories" | "rss_feeds"))
    {
        return Err(format!(
            "ignoring {name}: {dotted} is a list and stays file-only"
        ));
    }
    let text = if from_file {
        if !matches!(setting.kind, crate::config_registry::Kind::Secret { .. }) {
            return Err(format!("ignoring {name}: _FILE is only for secrets"));
        }
        match std::fs::read_to_string(raw.trim()) {
            Ok(contents) => contents.trim_end_matches(['\n', '\r']).to_string(),
            Err(error) => {
                return Err(format!("ignoring {name}: cannot read secret file: {error}"));
            }
        }
    } else {
        raw.to_string()
    };
    let parsed = parse_env_text(&setting.kind, &text)
        .map_err(|message| format!("ignoring {name}: {dotted} {message}"))?;
    let checked = crate::config_registry::validate_value(&setting.kind, &parsed)
        .map_err(|message| format!("ignoring {name}: {dotted} {message}"))?;
    // Leak the dotted key so OverrideValue can stay 'static, matching Env names.
    let key: &'static str = Box::leak(dotted.into_boxed_str());
    Ok(Some(OverrideValue::Json {
        key,
        value: checked,
    }))
}

fn parse_env_text(
    kind: &crate::config_registry::Kind,
    text: &str,
) -> Result<serde_json::Value, String> {
    use crate::config_registry::Kind;
    let trimmed = text.trim();
    match kind {
        Kind::Bool => {
            let on = matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
            let off = matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            );
            if on {
                Ok(serde_json::json!(true))
            } else if off {
                Ok(serde_json::json!(false))
            } else {
                Err("expected a boolean".to_string())
            }
        }
        Kind::U64 { .. } | Kind::Usize { .. } | Kind::U16 { .. } | Kind::U8 { .. } => {
            let number: u64 = trimmed
                .parse()
                .map_err(|_| "expected an integer".to_string())?;
            Ok(serde_json::json!(number))
        }
        Kind::F64 { .. } => {
            let number: f64 = trimmed
                .parse()
                .map_err(|_| "expected a number".to_string())?;
            Ok(serde_json::json!(number))
        }
        Kind::Path { .. } | Kind::String { .. } | Kind::Secret { .. } | Kind::Enum { .. } => {
            Ok(serde_json::Value::String(trimmed.to_string()))
        }
        Kind::StringList => Err("lists stay file-only".to_string()),
    }
}

/// `RUSTNZB_CONFIG_MANAGED` is on when the value is `1`, `true`, `yes` or `on`.
pub fn config_managed(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Override {
    pub value: OverrideValue,
    pub source: OverrideSource,
}

/// The overrides in force for one process. Built once at startup and never
/// changed, because the environment cannot change under a running process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverrideSet {
    entries: Vec<Override>,
}

fn apply_json_leaf(config: &mut AppConfig, key: &str, value: &serde_json::Value) {
    let Ok(mut document) = serde_json::to_value(&*config) else {
        return;
    };
    if crate::config_registry::set_leaf(&mut document, key, value.clone()).is_err() {
        return;
    }
    if let Ok(next) = serde_json::from_value::<AppConfig>(document) {
        *config = next;
    }
}

fn copy_json_leaf(next: &mut AppConfig, file: &AppConfig, key: &str) {
    let Ok(file_value) = serde_json::to_value(file) else {
        return;
    };
    let mut current = &file_value;
    for part in key.split('.') {
        match current.get(part) {
            Some(child) => current = child,
            None => return,
        }
    }
    apply_json_leaf(next, key, current);
}

impl OverrideSet {
    /// Add an override, replacing any earlier one for the same key.
    pub fn set(&mut self, value: OverrideValue, source: OverrideSource) {
        let key = value.key();
        self.entries.retain(|o| o.value.key() != key);
        self.entries.push(Override { value, source });
    }

    /// The override for a dotted config key, if one is in force.
    pub fn get(&self, key: &str) -> Option<&Override> {
        self.entries.iter().find(|o| o.value.key() == key)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Override> {
        self.entries.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write every override into `config`.
    pub fn apply(&self, config: &mut AppConfig) {
        for entry in &self.entries {
            match &entry.value {
                OverrideValue::ListenAddr(v) => config.general.listen_addr = v.clone(),
                OverrideValue::Port(v) => config.general.port = *v,
                OverrideValue::DataDir(v) => config.general.data_dir = v.clone(),
                OverrideValue::OtelEnabled(v) => config.otel.enabled = *v,
                OverrideValue::OtelEndpoint(v) => config.otel.endpoint = v.clone(),
                OverrideValue::OtelLogsEnabled(v) => config.otel.logs_enabled = Some(*v),
                OverrideValue::OtelLogsEndpoint(v) => config.otel.logs_endpoint = Some(v.clone()),
                OverrideValue::OtelMetricsEnabled(v) => config.otel.metrics_enabled = Some(*v),
                OverrideValue::OtelMetricsEndpoint(v) => {
                    config.otel.metrics_endpoint = Some(v.clone());
                }
                OverrideValue::OtelServiceName(v) => config.otel.service_name = v.clone(),
                OverrideValue::DavEnabled(v) => config.dav.enabled = *v,
                OverrideValue::Json { key, value } => apply_json_leaf(config, key, value),
            }
        }
    }

    /// Copy `file`'s value into `next` for every overridden key, so a save
    /// never writes an override value. A GUI round-trip sends the effective
    /// value back, and this drops it again.
    pub fn keep_file_values(&self, next: &mut AppConfig, file: &AppConfig) {
        for entry in &self.entries {
            match &entry.value {
                OverrideValue::ListenAddr(_) => {
                    next.general.listen_addr = file.general.listen_addr.clone();
                }
                OverrideValue::Port(_) => next.general.port = file.general.port,
                OverrideValue::DataDir(_) => {
                    next.general.data_dir = file.general.data_dir.clone();
                }
                OverrideValue::OtelEnabled(_) => next.otel.enabled = file.otel.enabled,
                OverrideValue::OtelEndpoint(_) => {
                    next.otel.endpoint = file.otel.endpoint.clone();
                }
                OverrideValue::OtelLogsEnabled(_) => {
                    next.otel.logs_enabled = file.otel.logs_enabled;
                }
                OverrideValue::OtelLogsEndpoint(_) => {
                    next.otel.logs_endpoint = file.otel.logs_endpoint.clone();
                }
                OverrideValue::OtelMetricsEnabled(_) => {
                    next.otel.metrics_enabled = file.otel.metrics_enabled;
                }
                OverrideValue::OtelMetricsEndpoint(_) => {
                    next.otel.metrics_endpoint = file.otel.metrics_endpoint.clone();
                }
                OverrideValue::OtelServiceName(_) => {
                    next.otel.service_name = file.otel.service_name.clone();
                }
                OverrideValue::DavEnabled(_) => next.dav.enabled = file.dav.enabled,
                OverrideValue::Json { key, .. } => copy_json_leaf(next, file, key),
            }
        }
    }

    /// The effective configuration: `file` with every override applied.
    pub fn overlay(&self, file: &AppConfig) -> AppConfig {
        let mut effective = file.clone();
        self.apply(&mut effective);
        effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_override_for_same_key_replaces_earlier() {
        let mut set = OverrideSet::default();
        set.set(OverrideValue::Port(8000), OverrideSource::Cli);
        set.set(
            OverrideValue::Port(8001),
            OverrideSource::Env("RUSTNZB_PORT"),
        );
        assert_eq!(set.iter().count(), 1);
        let entry = set.get("general.port").unwrap();
        assert_eq!(entry.value, OverrideValue::Port(8001));
        assert_eq!(entry.source.to_string(), "env:RUSTNZB_PORT");
    }

    #[test]
    fn overlay_leaves_the_file_layer_untouched() {
        let mut file = AppConfig::default();
        file.general.port = 9090;
        let mut set = OverrideSet::default();
        set.set(OverrideValue::Port(7777), OverrideSource::Cli);
        set.set(OverrideValue::DavEnabled(true), OverrideSource::Desktop);

        let effective = set.overlay(&file);
        assert_eq!(effective.general.port, 7777);
        assert!(effective.dav.enabled);
        assert_eq!(file.general.port, 9090);
        assert!(!file.dav.enabled);
    }

    #[test]
    fn keep_file_values_drops_override_values_from_a_save() {
        let mut file = AppConfig::default();
        file.general.data_dir = "/srv/file".into();
        file.dav.enabled = false;
        let mut set = OverrideSet::default();
        set.set(OverrideValue::DataDir("/data".into()), OverrideSource::Cli);
        set.set(
            OverrideValue::DavEnabled(true),
            OverrideSource::Env("RUSTNZB_DAV_ENABLED"),
        );

        // A GUI save carries the effective values back, plus a real edit.
        let mut next = set.overlay(&file);
        next.general.speed_limit_bps = 42;
        set.keep_file_values(&mut next, &file);

        assert_eq!(next.general.data_dir, PathBuf::from("/srv/file"));
        assert!(!next.dav.enabled);
        assert_eq!(next.general.speed_limit_bps, 42);
    }

    #[test]
    fn source_labels_match_the_api_vocabulary() {
        assert_eq!(OverrideSource::Cli.to_string(), "cli");
        assert_eq!(OverrideSource::Desktop.to_string(), "desktop");
        assert_eq!(
            OverrideSource::Env("OTEL_ENABLED").to_string(),
            "env:OTEL_ENABLED"
        );
    }
}

#[cfg(test)]
mod generic_env {
    use super::*;

    #[test]
    fn generic_speed_limit_parses() {
        let value = generic_env_override("RUSTNZB__GENERAL__SPEED_LIMIT_BPS", "2048")
            .expect("valid")
            .expect("some");
        assert_eq!(value.key(), "general.speed_limit_bps");
        match value {
            OverrideValue::Json { value, .. } => assert_eq!(value, serde_json::json!(2048)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn invalid_and_unknown_are_errors() {
        let invalid = generic_env_override("RUSTNZB__GENERAL__SPEED_LIMIT_BPS", "fast")
            .expect_err("bad type");
        assert!(invalid.contains("ignoring"), "{invalid}");
        let unknown =
            generic_env_override("RUSTNZB__GENERAL__NOT_A_SETTING", "1").expect_err("unknown");
        assert!(unknown.contains("not a known setting"), "{unknown}");
    }

    #[test]
    fn legacy_alias_is_left_to_the_dedicated_mapping() {
        assert!(
            generic_env_override("RUSTNZB_PORT", "8080")
                .unwrap()
                .is_none()
                || generic_env_override("RUSTNZB__GENERAL__PORT", "8080")
                    .ok()
                    .flatten()
                    .is_some()
        );
        // The short name is not the double-underscore form.
        assert!(
            generic_env_override("RUSTNZB_PORT", "8080")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn managed_flag_is_truthy_only() {
        assert!(config_managed(Some("1")));
        assert!(config_managed(Some(" true ")));
        assert!(!config_managed(Some("0")));
        assert!(!config_managed(None));
    }
}
