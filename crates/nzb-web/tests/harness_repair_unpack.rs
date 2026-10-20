//! WI-1757: a RAR release downloaded through a real `QueueManager` from mock
//! NNTP servers, then PAR2-repaired and unpacked end to end.
//!
//! - Articles missing on every server, within the recovery slices: the job
//!   completes and the extracted file lands in the complete directory with
//!   no archives or PAR2 files left beside it.
//! - Damage beyond the recovery slices: the job fails with a typed failure
//!   code and nothing is extracted.
//! - A password from the NZB's `<meta type="password">` unpacks a protected
//!   archive. Without it the job fails with `archive_password_required`.
//!
//! Extraction runs against `mock_nntp_server::fake_unrar`, which fails
//! unless every volume matches its original bytes.
#![cfg(unix)]

mod harness;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use harness::{HarnessBuilder, ServerProfile, TestEngine};
use mock_nntp_server::SAMPLE_GROUP;
use mock_nntp_server::damage::{DamageRelease, seeded_bytes};
use mock_nntp_server::fake_unrar::{self, FakeUnrar};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::models::{JobFailureCode, JobStatus};

const TERMINAL: &[JobStatus] = &[JobStatus::Completed, JobStatus::Failed];
const RAR5_MAGIC: &[u8] = b"Rar!\x1a\x07\x01\x00";

/// A three-volume RAR set as a damage release with `volume_blocks`.
fn rar_release(
    stem: &str,
    seed: u64,
    volume_blocks: &[u32],
) -> (DamageRelease, Vec<(String, Vec<u8>)>) {
    let volumes: Vec<(String, Vec<u8>)> = (1..=3)
        .map(|n| {
            let mut bytes = RAR5_MAGIC.to_vec();
            bytes.extend(seeded_bytes(seed + n, 24_000));
            (format!("{stem}.part{n:02}.rar"), bytes)
        })
        .collect();
    (
        DamageRelease::from_files(stem, volumes.clone(), volume_blocks),
        volumes,
    )
}

fn refs(volumes: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
    volumes
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect()
}

/// Two servers that both lack `missing`.
async fn engine(release: &DamageRelease, missing: &BTreeSet<String>) -> TestEngine {
    let mut builder = HarnessBuilder::new().article_timeout(2);
    for (priority, id) in ["A", "B"].into_iter().enumerate() {
        let mut articles = release.encoded_articles();
        articles.retain(|mid, _| !missing.contains(mid));
        let count = articles.len() as u64;
        let config = MockConfig {
            articles,
            groups: HashMap::from([(SAMPLE_GROUP.to_string(), (count, 1, count))]),
            ..MockConfig::default()
        };
        builder = builder.with_server(
            ServerProfile::start(id, config, 2)
                .await
                .with_priority(priority as u8),
        );
    }
    builder.build()
}

/// Submit `xml`, wait for a terminal state, and return the job's status and
/// failure code.
async fn run(engine: &TestEngine, name: &str, xml: Vec<u8>) -> (JobStatus, Option<JobFailureCode>) {
    let job_id = engine.submit_nzb_xml(name, xml).expect("submit");
    assert!(
        engine
            .wait_for_status(&job_id, Duration::from_secs(90), TERMINAL)
            .await,
        "{name}: job never finished"
    );
    // The history row is written just before the job leaves the queue view.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(entry) = engine.queue_manager.history_get(&job_id).expect("history") {
            return (entry.status, entry.failure_code);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{name}: no history entry"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn named<'a>(files: &'a [PathBuf], suffix: &str) -> Vec<&'a PathBuf> {
    files
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_articles_within_recovery_are_repaired_and_unpacked() {
    let fake = fake_unrar::install();
    let (release, volumes) = rar_release("e2erepair", 0x1757_1001, &[1, 2, 4, 8]);
    fake.register("e2erepair", &refs(&volumes), None);
    // Article 2 of the second volume is gone from both servers.
    let missing = BTreeSet::from([release.data_message_id(1, 2)]);
    let engine = engine(&release, &missing).await;

    let (status, code) = run(&engine, "e2erepair", release.nzb_xml(&[])).await;

    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
    let files = files_under(&engine.complete_dir);
    let outputs = named(&files, "e2erepair.mkv");
    assert_eq!(outputs.len(), 1, "one extracted file: {files:?}");
    assert!(std::fs::read(outputs[0]).unwrap() == FakeUnrar::expected_output(&refs(&volumes)));
    assert!(
        named(&files, ".rar").is_empty(),
        "archives cleaned: {files:?}"
    );
    assert!(named(&files, ".par2").is_empty(), "PAR2 cleaned: {files:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damage_beyond_recovery_fails_typed_without_unpacking() {
    let fake = fake_unrar::install();
    // One recovery slice; three missing articles damage far more.
    let (release, volumes) = rar_release("e2ehopeless", 0x1757_1002, &[1]);
    fake.register("e2ehopeless", &refs(&volumes), None);
    let missing: BTreeSet<String> = (1..=3).map(|n| release.data_message_id(1, n)).collect();
    let engine = engine(&release, &missing).await;

    let (status, code) = run(&engine, "e2ehopeless", release.nzb_xml(&[])).await;

    assert_eq!(status, JobStatus::Failed);
    // Parity-aware hopeless detection (WI-145) may stop the job before
    // post-processing; otherwise PAR2 repair fails. Either is typed.
    assert!(
        matches!(
            code,
            Some(JobFailureCode::ArticlesUnavailable | JobFailureCode::RepairFailed)
        ),
        "unexpected failure code {code:?}"
    );
    let files = files_under(&engine.complete_dir);
    assert!(
        named(&files, ".mkv").is_empty(),
        "nothing extracted: {files:?}"
    );
}

/// The release's NZB with an archive password in its `<head>`.
fn with_password(xml: Vec<u8>, password: &str) -> Vec<u8> {
    let xml = String::from_utf8(xml).unwrap();
    let marker = "<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n";
    assert!(xml.contains(marker));
    xml.replacen(
        marker,
        &format!("{marker}  <head>\n    <meta type=\"password\">{password}</meta>\n  </head>\n"),
        1,
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nzb_password_unpacks_a_protected_archive() {
    let fake = fake_unrar::install();
    let (release, volumes) = rar_release("e2elocked", 0x1757_1003, &[1, 2]);
    fake.register("e2elocked", &refs(&volumes), Some("letmein"));
    let engine = engine(&release, &BTreeSet::new()).await;

    let xml = with_password(release.nzb_xml(&[]), "letmein");
    let (status, code) = run(&engine, "e2elocked", xml).await;

    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
    let files = files_under(&engine.complete_dir);
    assert_eq!(named(&files, "e2elocked.mkv").len(), 1, "{files:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_archive_without_a_password_fails_typed() {
    let fake = fake_unrar::install();
    let (release, volumes) = rar_release("e2enopw", 0x1757_1004, &[1, 2]);
    fake.register("e2enopw", &refs(&volumes), Some("letmein"));
    let engine = engine(&release, &BTreeSet::new()).await;

    let (status, code) = run(&engine, "e2enopw", release.nzb_xml(&[])).await;

    assert_eq!(status, JobStatus::Failed);
    assert_eq!(code, Some(JobFailureCode::ArchivePasswordRequired));
    let files = files_under(&engine.complete_dir);
    assert!(
        named(&files, ".mkv").is_empty(),
        "nothing extracted: {files:?}"
    );
}
