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
use nzb_web::nzb_core::config::EncryptedAction;
use nzb_web::nzb_core::models::{JobFailureCode, JobStatus};
use nzb_web::queue_manager::HistoryRetryOutcome;

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
async fn password_file_unpacks_a_protected_archive() {
    let fake = fake_unrar::install();
    let (release, volumes) = rar_release("e2epwfile", 0x1757_1005, &[1, 2]);
    fake.register("e2epwfile", &refs(&volumes), Some("letmein"));
    let engine = engine(&release, &BTreeSet::new()).await;
    let dir = tempfile::tempdir().unwrap();
    let passwords = dir.path().join("passwords.txt");
    std::fs::write(&passwords, "wrong\n\nletmein\nunused\n").unwrap();
    engine.queue_manager.set_password_file(Some(passwords));

    let xml = with_password(release.nzb_xml(&[]), "alsowrong");
    let (status, code) = run(&engine, "e2epwfile", xml).await;

    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
    let files = files_under(&engine.complete_dir);
    assert_eq!(named(&files, "e2epwfile.mkv").len(), 1, "{files:?}");
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

/// Like [`rar_release`], with each volume starting as an encrypted RAR5 does.
fn encrypted_release(stem: &str, seed: u64) -> (DamageRelease, Vec<(String, Vec<u8>)>) {
    let volumes: Vec<(String, Vec<u8>)> = (1..=3)
        .map(|n| {
            let mut bytes = RAR5_MAGIC.to_vec();
            // Archive encryption header: CRC, size, type 4, then its fields.
            bytes.extend([0, 0, 0, 0, 5, 4, 0, 0, 0, 15]);
            bytes.extend(seeded_bytes(seed + n, 24_000));
            (format!("{stem}.part{n:02}.rar"), bytes)
        })
        .collect();
    (
        DamageRelease::from_files(stem, volumes.clone(), &[1]),
        volumes,
    )
}

/// An engine that downloads slowly enough for the encryption check to act
/// before the last article.
async fn slow_engine(release: &DamageRelease) -> TestEngine {
    let engine = engine(release, &BTreeSet::new()).await;
    engine.queue_manager.set_speed_limit(20_000);
    engine
}

async fn history_status(engine: &TestEngine, id: &str) -> (JobStatus, Option<JobFailureCode>) {
    for _ in 0..1200 {
        if let Some(entry) = engine.queue_manager.history_get(id).unwrap() {
            return (entry.status, entry.failure_code);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{id}: no history entry");
}

/// A password failure keeps the download; a retry with the password extracts
/// it without fetching anything, and deleting the entry reclaims the files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_with_password_reuses_the_kept_files() {
    let fake = fake_unrar::install();
    let (release, volumes) = rar_release("e2ekept", 0x1757_1006, &[1]);
    fake.register("e2ekept", &refs(&volumes), Some("letmein"));
    let engine = engine(&release, &BTreeSet::new()).await;
    let qm = &engine.queue_manager;

    let id = engine
        .submit_nzb_xml("e2ekept", release.nzb_xml(&[]))
        .unwrap();
    assert_eq!(
        history_status(&engine, &id).await.1,
        Some(JobFailureCode::ArchivePasswordRequired)
    );
    let kept = engine.incomplete_dir.join(&id);
    let volume = kept.join("e2ekept.part01.rar");
    assert!(volume.is_file(), "volumes are kept");
    assert!(files_under(&engine.complete_dir).is_empty());

    let entry = qm.history_get(&id).unwrap().unwrap();
    let retry = |password: &str| match qm
        .retry_history_entry_with_password(&entry, Some(password.to_string()))
        .unwrap()
    {
        HistoryRetryOutcome::Started(id) => id,
        _ => panic!("retry did not start"),
    };
    // A wrong password keeps the files for another try.
    let wrong = retry("wrong");
    let (status, code) = history_status(&engine, &wrong).await;
    assert_eq!(status, JobStatus::Failed);
    assert_eq!(code, Some(JobFailureCode::ArchivePasswordRequired));
    assert!(volume.is_file());
    // Both entries name the directory; deleting one leaves it to the other.
    qm.history_remove(&wrong).unwrap();
    assert!(volume.is_file());

    // The finished retry stays in the queue view for some seconds, during
    // which the entry counts as being retried.
    for _ in 0..400 {
        if !qm.get_jobs().iter().any(|job| job.id == wrong) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let right = retry("letmein");
    let (status, code) = history_status(&engine, &right).await;
    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
    let files = files_under(&engine.complete_dir);
    assert_eq!(
        files,
        [engine.complete_dir.join("Default/e2ekept/e2ekept.mkv")],
        "output goes to the plain folder name"
    );
    let done = qm.history_get(&right).unwrap().unwrap();
    assert_eq!(
        done.server_stats
            .iter()
            .map(|server| server.articles_downloaded)
            .sum::<usize>(),
        0,
        "nothing is downloaded again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_password_failure_removes_the_kept_files() {
    let fake = fake_unrar::install();
    let (release, volumes) = rar_release("e2edrop", 0x1757_1007, &[1]);
    fake.register("e2edrop", &refs(&volumes), Some("letmein"));
    let engine = engine(&release, &BTreeSet::new()).await;
    let id = engine
        .submit_nzb_xml("e2edrop", release.nzb_xml(&[]))
        .unwrap();
    history_status(&engine, &id).await;
    let kept = engine.incomplete_dir.join(&id);
    assert!(kept.is_dir());
    engine.queue_manager.history_remove(&id).unwrap();
    assert!(!kept.exists());
}

/// Every password the NZB lists is tried, and the job ends up with the one
/// that worked, which is what scripts see.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_nzb_password_is_tried() {
    let fake = fake_unrar::install();
    let (release, volumes) = rar_release("e2emulti", 0x1757_1008, &[1]);
    fake.register("e2emulti", &refs(&volumes), Some("first"));
    let engine = engine(&release, &BTreeSet::new()).await;
    let xml = with_password(with_password(release.nzb_xml(&[]), "last"), "first");
    let id = engine.submit_nzb_xml("e2emulti", xml).unwrap();
    let (status, code) = history_status(&engine, &id).await;
    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
    let job = engine
        .queue_manager
        .get_jobs()
        .into_iter()
        .find(|job| job.id == id)
        .expect("a finished job stays in the queue view briefly");
    assert_eq!(job.password.as_deref(), Some("first"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_encrypted_job_is_aborted_and_its_retry_completes() {
    let fake = fake_unrar::install();
    let (release, volumes) = encrypted_release("e2eabort", 0x1757_1009);
    fake.register("e2eabort", &refs(&volumes), Some("letmein"));
    let engine = engine(&release, &BTreeSet::new()).await;
    let qm = &engine.queue_manager;
    qm.set_encrypted_action(EncryptedAction::Abort);
    let id = engine
        .submit_nzb_xml("e2eabort", release.nzb_xml(&[]))
        .unwrap();
    // Aborted early or, if the download won the race, failed at extraction.
    assert_eq!(
        history_status(&engine, &id).await,
        (
            JobStatus::Failed,
            Some(JobFailureCode::ArchivePasswordRequired)
        )
    );
    let entry = qm.history_get(&id).unwrap().unwrap();
    let HistoryRetryOutcome::Started(retry) = qm
        .retry_history_entry_with_password(&entry, Some("letmein".into()))
        .unwrap()
    else {
        panic!("retry did not start");
    };
    let (status, code) = history_status(&engine, &retry).await;
    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_encrypted_job_pauses_until_its_password_is_set() {
    let fake = fake_unrar::install();
    let (release, volumes) = encrypted_release("e2epause", 0x1757_100a);
    fake.register("e2epause", &refs(&volumes), Some("letmein"));
    let engine = slow_engine(&release).await;
    let qm = &engine.queue_manager;
    qm.set_encrypted_action(EncryptedAction::Pause);
    let id = engine
        .submit_nzb_xml("e2epause", release.nzb_xml(&[]))
        .unwrap();
    assert!(
        engine
            .wait_for_status(&id, Duration::from_secs(60), &[JobStatus::Paused])
            .await,
        "encrypted job was not paused"
    );
    for _ in 0..100 {
        let job = qm.get_jobs().into_iter().find(|job| job.id == id).unwrap();
        if job.error_message.is_some_and(|m| m.contains("Encrypted")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    qm.set_job_password(&id, Some("letmein".into())).unwrap();
    qm.resume_job(&id).unwrap();
    let (status, code) = history_status(&engine, &id).await;
    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
}

/// A job whose known password opens the archive is not stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_known_password_keeps_an_encrypted_job_downloading() {
    let fake = fake_unrar::install();
    let (release, volumes) = encrypted_release("e2eknown", 0x1757_100b);
    fake.register("e2eknown", &refs(&volumes), Some("letmein"));
    let engine = slow_engine(&release).await;
    engine
        .queue_manager
        .set_encrypted_action(EncryptedAction::Abort);
    let xml = with_password(with_password(release.nzb_xml(&[]), "wrong"), "letmein");
    let (status, code) = run(&engine, "e2eknown", xml).await;
    assert_eq!(status, JobStatus::Completed, "failure code: {code:?}");
}
