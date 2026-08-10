//! Regression tests for issue #62: rustnzb crashed on startup with a bare
//! `Permission denied (os error 13)` and no indication of which directory or
//! path caused it. `startup::initialize` must now attach the failing path
//! (and a permission hint) to any directory-creation error.

use nzb_web::{StartupConfig, startup};

/// Positive: a custom data/incomplete/complete dir set (not matching the
/// three hardcoded defaults baked into the Docker image's init script)
/// succeeds as long as the parent is writable.
#[tokio::test]
async fn initialize_creates_custom_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    let data_dir = tmp.path().join("custom-data");
    let incomplete_dir = tmp.path().join("custom-incomplete");
    let complete_dir = tmp.path().join("custom-complete");

    let mut startup_cfg = StartupConfig {
        config_path,
        listen_addr: None,
        port: None,
        data_dir: Some(data_dir.clone()),
        log_level: None,
    };

    // `initialize` only overrides data_dir via StartupConfig; incomplete/complete
    // come from the config file, so write one first with our custom paths.
    let mut config = nzb_web::nzb_core::config::AppConfig::default();
    config.general.data_dir = data_dir.clone();
    config.general.incomplete_dir = incomplete_dir.clone();
    config.general.complete_dir = complete_dir.clone();
    config.save(&startup_cfg.config_path).unwrap();
    startup_cfg.data_dir = None; // already set in the saved config

    let result = startup::initialize(startup_cfg, None).await;
    assert!(
        result.is_ok(),
        "expected initialize to succeed, got: {:?}",
        result.err().map(|e| format!("{e:?}"))
    );

    assert!(data_dir.is_dir());
    assert!(incomplete_dir.is_dir());
    assert!(complete_dir.is_dir());
}

/// Negative: a data_dir under a non-writable parent must fail with an error
/// that names the failing path — not a bare `os error 13`.
#[cfg(unix)]
#[tokio::test]
async fn initialize_reports_context_on_unwritable_data_dir() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let locked_parent = tmp.path().join("locked");
    std::fs::create_dir_all(&locked_parent).unwrap();
    std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o000)).unwrap();

    let config_path = tmp.path().join("config.toml");
    let data_dir = locked_parent.join("data");

    let startup_cfg = StartupConfig {
        config_path,
        listen_addr: None,
        port: None,
        data_dir: Some(data_dir.clone()),
        log_level: None,
    };

    let result = startup::initialize(startup_cfg, None).await;

    // Restore permissions so the tempdir can be cleaned up.
    std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o755)).unwrap();

    let err = match result {
        Err(e) => e,
        // Running as root (e.g. CI containers) bypasses the permission
        // check entirely, so there's nothing to assert.
        Ok(_) => return,
    };

    let debug_text = format!("{err:?}");
    assert_ne!(
        debug_text.trim(),
        "Permission denied (os error 13)",
        "regression: error must not be the bare os-error text from issue #62, got: {debug_text}"
    );
    assert!(
        debug_text.contains(&data_dir.display().to_string()),
        "error should mention the failing path, got: {debug_text}"
    );
}
