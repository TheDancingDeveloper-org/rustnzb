//! Process-level startup checks for the `rustnzb` binary: a bad, missing or
//! unwritable config, data dir or port must exit non-zero with a message that
//! names the failing thing. Also covers the config path being resolved once,
//! and the atomic, 0600 config save.
//!
//! Every test runs under a temp directory and an OS-assigned port, so nothing
//! touches the operator's real config, data or the live instance.

#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::config_store::{resolve_operator_config_path, write_file_layer};
use serial_test::serial;

/// The built `rustnzb` binary, with any `RUSTNZB_*` overrides from the
/// developer's shell removed so each test controls its own inputs.
fn rustnzb() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rustnzb"));
    for var in [
        "RUSTNZB_CONFIG",
        "RUSTNZB_PORT",
        "RUSTNZB_LISTEN_ADDR",
        "RUSTNZB_DATA_DIR",
        "RUSTNZB_LOG_FILE",
        "RUSTNZB_LOG_LEVEL",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// A config file whose directories all live under `root`, so a run never
/// creates anything at the `/downloads` or `/data` defaults.
fn write_config(root: &Path, port: u16) -> PathBuf {
    let path = root.join("config.toml");
    let body = format!(
        "[general]\n\
         listen_addr = \"127.0.0.1\"\n\
         port = {port}\n\
         data_dir = \"{data}\"\n\
         incomplete_dir = \"{incomplete}\"\n\
         complete_dir = \"{complete}\"\n",
        data = root.join("data").display(),
        incomplete = root.join("incomplete").display(),
        complete = root.join("complete").display(),
    );
    fs::write(&path, body).expect("write config");
    path
}

/// A port nothing is listening on right now. There is a small race before the
/// binary binds it, which is acceptable for these tests.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind probe")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A regular file. A path with this as its parent cannot be created as a
/// directory, which fails the same way for root and non-root users.
fn blocker_file(root: &Path) -> PathBuf {
    let path = root.join("not-a-dir");
    fs::write(&path, b"x").expect("write blocker");
    path
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn malformed_config_exits_1_with_the_parse_error() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, "[general\nport = \n").unwrap();

    let out = rustnzb().arg("--config").arg(&config).output().unwrap();

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("Error:"),
        "expected an error report, got: {}",
        stderr_of(&out)
    );
}

#[test]
fn unwritable_config_directory_exits_1_and_names_the_directory() {
    let root = tempfile::tempdir().unwrap();
    let blocker = blocker_file(root.path());
    let config = blocker.join("config.toml");

    let out = rustnzb().arg("--config").arg(&config).output().unwrap();

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains("Failed to create config directory"),
        "expected config directory context, got: {err}"
    );
    assert!(
        err.contains(&blocker.display().to_string()),
        "expected the failing path in the message, got: {err}"
    );
}

#[test]
fn unwritable_data_dir_exits_1_and_names_the_directory() {
    let root = tempfile::tempdir().unwrap();
    let config = write_config(root.path(), free_port());
    let blocker = blocker_file(root.path());
    let data_dir = blocker.join("data");

    let out = rustnzb()
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains("Failed to create directory"),
        "expected data dir context, got: {err}"
    );
    assert!(
        err.contains(&data_dir.display().to_string()),
        "expected the failing data dir in the message, got: {err}"
    );
}

#[test]
fn non_numeric_port_is_rejected_by_argument_parsing() {
    let root = tempfile::tempdir().unwrap();
    let config = write_config(root.path(), free_port());

    let out = rustnzb()
        .arg("--config")
        .arg(&config)
        .args(["--port", "not-a-port"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("--port"),
        "expected clap to name the --port flag, got: {}",
        stderr_of(&out)
    );
}

#[test]
fn port_already_in_use_exits_1_and_names_the_bind_address() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().unwrap().port();
    let root = tempfile::tempdir().unwrap();
    let config = write_config(root.path(), port);

    let out = rustnzb().arg("--config").arg(&config).output().unwrap();

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains(&format!("127.0.0.1:{port}")),
        "expected the bind address in the message, got: {err}"
    );
    assert!(
        err.contains("Failed to bind HTTP server"),
        "expected bind-failure context, got: {err}"
    );
    drop(occupied);
}

#[test]
fn missing_config_is_created_with_owner_only_permissions() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("fresh").join("config.toml");
    // A bad data dir stops startup right after the default config is written,
    // so the test never reaches the server or the default download paths.
    let blocker = blocker_file(root.path());

    let out = rustnzb()
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(blocker.join("data"))
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr_of(&out));
    assert!(config.exists(), "default config should be written");
    let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "config must be owner-only");
}

/// The config path is resolved once, at startup, against the launch CWD. A
/// later CWD change must not move it.
#[test]
#[serial(cwd)]
fn config_path_resolves_once_when_cwd_changes() {
    let original = std::env::current_dir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    // Restores the process CWD even if an assertion below panics.
    struct RestoreCwd(PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }
    let _restore = RestoreCwd(original);

    std::env::set_current_dir(first.path()).unwrap();
    let resolved = resolve_operator_config_path(Path::new("config.toml")).unwrap();
    let anchored = first.path().canonicalize().unwrap().join("config.toml");
    assert_eq!(
        resolved, anchored,
        "relative path should anchor to launch CWD"
    );

    std::env::set_current_dir(second.path()).unwrap();
    let again = resolve_operator_config_path(&resolved).unwrap();
    assert_eq!(again, anchored, "an already-resolved path must not move");
}

/// End to end: a relative `--config` from one CWD creates the file there, even
/// when the directory is created during startup.
#[test]
fn relative_config_path_is_resolved_against_the_launch_directory() {
    let launch_dir = tempfile::tempdir().unwrap();
    let blocker = blocker_file(launch_dir.path());

    let out = rustnzb()
        .current_dir(launch_dir.path())
        .args(["--config", "nested/config.toml"])
        .arg("--data-dir")
        .arg(blocker.join("data"))
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr_of(&out));
    assert!(
        launch_dir.path().join("nested/config.toml").exists(),
        "config should be created under the launch directory"
    );
}

/// A save replaces the file atomically with owner-only permissions, keeps the
/// previous file as a backup, leaves the user's comments in place, and leaves
/// no temp file behind.
#[test]
fn config_save_is_atomic_and_owner_only() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let original = "# keep me\n[general]\nport = 1111\n";
    fs::write(&path, original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let mut config = AppConfig::default();
    config.general.port = 2222;
    write_file_layer(&config, &path).expect("save");

    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "saved config must be owner-only");

    let saved = fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("# keep me"),
        "comments must survive: {saved}"
    );
    let reloaded = AppConfig::load(&path).expect("saved file reloads");
    assert_eq!(reloaded.general.port, 2222);

    let leftovers: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");

    let backups: Vec<_> = fs::read_dir(root.path().join("config-backups"))
        .expect("previous file kept as a backup")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 1, "exactly one backup expected: {backups:?}");
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), original);
}
