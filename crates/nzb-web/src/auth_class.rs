//! Auth class for settings that the GUI and the native API can change.
//!
//! Sensitivity comes from `nzb_core::config_registry`. Sensitive keys need an
//! interactive session plus step-up re-authentication and are never writable
//! through `X-Api-Key` or the SABnzbd API. See
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

/// Actions that are not registry rows but still need step-up.
///
/// `config.backup.restore` and `config.backup.export_secrets` are operations,
/// not settings, so the registry does not classify them.
pub const ACTION_SENSITIVE_KEYS: &[&str] =
    &["config.backup.restore", "config.backup.export_secrets"];

/// Registry keys whose class is sensitive, plus the action keys above.
pub fn sensitive_keys() -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = crate::nzb_core::config_registry::settings()
        .into_iter()
        .filter(|setting| {
            matches!(
                setting.class,
                crate::nzb_core::config_registry::AuthClass::Sensitive
            )
        })
        .map(|setting| setting.key)
        .collect();
    keys.extend_from_slice(ACTION_SENSITIVE_KEYS);
    keys.sort_unstable();
    keys.dedup();
    keys
}

pub fn class_of(key: &str) -> AuthClass {
    if sensitive_keys().contains(&key) {
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
    fn sensitive_set_matches_the_registry() {
        let registry_sensitive: Vec<&str> = crate::nzb_core::config_registry::settings()
            .into_iter()
            .filter(|setting| {
                matches!(
                    setting.class,
                    crate::nzb_core::config_registry::AuthClass::Sensitive
                )
            })
            .map(|setting| setting.key)
            .collect();
        for key in [
            "general.trusted_proxies",
            "general.local_import_dir",
            "general.step_up_ttl_secs",
            "notifications.webhook_url",
            "notifications.apprise_api_url",
            "notifications.apprise_urls",
            "notifications.ntfy_token",
        ] {
            assert!(
                registry_sensitive.contains(&key),
                "{key} must stay Sensitive in the registry"
            );
        }
        let mut expected = registry_sensitive;
        expected.extend_from_slice(ACTION_SENSITIVE_KEYS);
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(sensitive_keys(), expected);
        for key in expected {
            assert_eq!(class_of(key), AuthClass::Sensitive, "{key}");
        }
        assert_eq!(class_of("general.log_level"), AuthClass::Normal);
        assert_eq!(class_of("general.max_active_downloads"), AuthClass::Normal);
        assert!(
            !crate::nzb_core::config_registry::settings()
                .iter()
                .any(|setting| ACTION_SENSITIVE_KEYS.contains(&setting.key)),
            "action keys are not registry rows"
        );
    }
}
