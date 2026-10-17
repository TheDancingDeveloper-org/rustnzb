//! Process-level checks for `--log-file`: the binary writes to a daily-dated
//! file named after the given path, and a restart keeps appending to it.

#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use chrono::NaiveDate;

const STARTED: &str = "HTTP server listening";
const TIMEOUT: Duration = Duration::from_secs(30);

/// Writes a config whose directories all live under `root`, on a free port.
fn write_config(root: &Path) -> PathBuf {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
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
    fs::write(&path, body).unwrap();
    path
}

/// Starts the binary with `--log-file` pointing at `log_path`. Output is
/// discarded so a full pipe can never stall the child.
fn start(config: &Path, log_path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_rustnzb"))
        .env_remove("RUSTNZB_CONFIG")
        .env_remove("RUSTNZB_PORT")
        .env_remove("RUSTNZB_LISTEN_ADDR")
        .env_remove("RUSTNZB_DATA_DIR")
        .env_remove("RUSTNZB_LOG_FILE")
        .env_remove("RUSTNZB_LOG_LEVEL")
        .arg("--config")
        .arg(config)
        .arg("--log-file")
        .arg(log_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rustnzb")
}

/// Stops the child with SIGTERM, so the graceful shutdown flushes the log
/// writer, and waits for it to exit.
fn stop(mut child: Child) {
    // SAFETY: plain libc call on a pid we spawned and still own.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("rustnzb did not exit after SIGTERM");
        }
        sleep(Duration::from_millis(50));
    }
}

/// Polls until the dated log file in `dir` contains `needle`, returning its
/// path. Fails the test if that takes longer than [`TIMEOUT`].
fn wait_for_log_line(dir: &Path, prefix: &str, needle: &str) -> PathBuf {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(path) = dated_files(dir, prefix)
            .into_iter()
            .find(|p| fs::read_to_string(p).is_ok_and(|s| s.contains(needle)))
        {
            return path;
        }
        assert!(
            Instant::now() < deadline,
            "no dated {prefix}* file in {} contained {needle:?}",
            dir.display()
        );
        sleep(Duration::from_millis(50));
    }
}

/// Files in `dir` named `<prefix>.<date>` (the daily rotation's naming).
fn dated_files(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&format!("{prefix}.")))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn log_file_creates_a_dated_file_named_after_the_path() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    fs::create_dir(&logs).unwrap();
    let config = write_config(root.path());
    let log_path = logs.join("rustnzb.log");

    let child = start(&config, &log_path);
    let written = wait_for_log_line(&logs, "rustnzb.log", STARTED);
    stop(child);

    let suffix = written
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_prefix("rustnzb.log.")
        .unwrap()
        .to_owned();
    NaiveDate::parse_from_str(&suffix, "%Y-%m-%d")
        .unwrap_or_else(|e| panic!("expected a YYYY-MM-DD suffix, got {suffix:?}: {e}"));
    assert!(
        !log_path.exists(),
        "the undated name should not be created, only the dated file"
    );
}

#[test]
fn log_file_keeps_appending_across_restarts() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    fs::create_dir(&logs).unwrap();
    let config = write_config(root.path());
    let log_path = logs.join("rustnzb.log");

    let child = start(&config, &log_path);
    let first = wait_for_log_line(&logs, "rustnzb.log", STARTED);
    stop(child);
    let after_first = fs::read_to_string(&first).unwrap();
    let starts_before = after_first.matches(STARTED).count();

    let child = start(&config, &log_path);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = fs::read_to_string(&first).unwrap_or_default();
        if now.matches(STARTED).count() > starts_before {
            break;
        }
        assert!(Instant::now() < deadline, "second run never logged startup");
        sleep(Duration::from_millis(50));
    }
    stop(child);

    let after_second = fs::read_to_string(&first).unwrap();
    assert!(
        after_second.starts_with(&after_first),
        "the first run's lines must be kept, not truncated"
    );
    assert_eq!(
        dated_files(&logs, "rustnzb.log").len(),
        1,
        "two runs on the same day share one dated file"
    );
}
