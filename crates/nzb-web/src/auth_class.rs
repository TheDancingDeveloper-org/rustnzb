//! Auth class for settings that the GUI and the native API can change.
//!
//! Stopgap until a config registry exists. Sensitive keys need an interactive
//! session plus step-up re-authentication and are never writable through
//! `X-Api-Key` or the SABnzbd API. See
//! `docs/gui-config-design-2026-10-16.md` §8.2.

/// Who may change a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthClass {
    /// Session, HTTP Basic, or `X-Api-Key`.
    Normal,
    /// Interactive session (or HTTP Basic) plus a recent password confirmation.
    /// `X-Api-Key` is refused.
    Sensitive,
}

/// Sensitive setting keys, dotted as `section.field`.
///
/// `fetch_allow_private` and `fetch_allowed_hosts` are classified here even
/// though `PUT /api/config/general` does not accept them yet: any future
/// writer must treat them as sensitive.
pub const SENSITIVE_KEYS: &[&str] = &[
    "general.api_key",
    "dav.password",
    "dav.api_key",
    "general.fetch_allow_private",
    "general.fetch_allowed_hosts",
    "general.scripts_dir",
    "general.script_success",
    "general.script_failure",
    "general.script_timeout_secs",
    "general.script_max_output_bytes",
    "general.listen_addr",
    "general.port",
    "general.data_dir",
    "config.backup.restore",
    "config.backup.export_secrets",
];

pub fn class_of(key: &str) -> AuthClass {
    if SENSITIVE_KEYS.contains(&key) {
        AuthClass::Sensitive
    } else {
        AuthClass::Normal
    }
}

/// JSON field names on `PUT /api/config/general` that are sensitive.
pub fn general_update_sensitive_fields(body_has: impl Fn(&str) -> bool) -> bool {
    const FIELDS: &[&str] = &[
        "data_dir",
        "scripts_dir",
        "script_success",
        "script_failure",
        "script_timeout_secs",
        "script_max_output_bytes",
        "listen_addr",
        "port",
    ];
    FIELDS.iter().any(|field| body_has(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_set_matches_the_design_list() {
        let expected = [
            "general.api_key",
            "dav.password",
            "dav.api_key",
            "general.fetch_allow_private",
            "general.fetch_allowed_hosts",
            "general.scripts_dir",
            "general.script_success",
            "general.script_failure",
            "general.script_timeout_secs",
            "general.script_max_output_bytes",
            "general.listen_addr",
            "general.port",
            "general.data_dir",
            "config.backup.restore",
            "config.backup.export_secrets",
        ];
        assert_eq!(SENSITIVE_KEYS, &expected);
        for key in expected {
            assert_eq!(class_of(key), AuthClass::Sensitive, "{key}");
        }
        assert_eq!(class_of("general.log_level"), AuthClass::Normal);
        assert_eq!(class_of("general.max_active_downloads"), AuthClass::Normal);
    }
}
