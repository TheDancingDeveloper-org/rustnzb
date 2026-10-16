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
        }
    }
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
