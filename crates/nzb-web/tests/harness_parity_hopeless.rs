//! WI-145: a job with a PAR2 set is hopeless only when the slices confirmed
//! missing exceed the recovery slices the release carries, not after a fixed
//! number of failed articles. Each test serves a deterministic damage release
//! (WI-151) through a real `QueueManager`.

mod harness;

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use harness::{HarnessBuilder, ServerProfile};
use mock_nntp_server::SAMPLE_GROUP;
use mock_nntp_server::damage::DamageRelease;
use nzb_nntp::testutil::{ArticleResponseSequences, MockConfig};
use nzb_web::nzb_core::models::JobStatus;

const TERMINAL: &[JobStatus] = &[JobStatus::Completed, JobStatus::Failed];

/// What both servers do with the release's articles.
#[derive(Default)]
struct Plan {
    /// Articles neither server has.
    missing: BTreeSet<String>,
    /// Articles both servers refuse with 430 this many times, then serve.
    refuse_first: HashMap<String, usize>,
}

fn config(release: &DamageRelease, plan: &Plan) -> MockConfig {
    let mut articles = release.encoded_articles();
    for mid in &plan.missing {
        articles.remove(mid);
    }
    let count = articles.len() as u64;
    let sequences = (!plan.refuse_first.is_empty()).then(|| {
        let sequences = ArticleResponseSequences::default();
        for (mid, n) in &plan.refuse_first {
            sequences.lock().insert(
                mid.clone(),
                (0..*n)
                    .map(|_| (430u16, format!("No article: <{mid}>")))
                    .collect(),
            );
        }
        sequences
    });
    MockConfig {
        articles,
        groups: HashMap::from([(SAMPLE_GROUP.to_string(), (count, 1, count))]),
        article_response_sequences: sequences,
        ..MockConfig::default()
    }
}

/// Run `release` with `plan` to a terminal state; return its status and
/// recorded error message.
async fn run(name: &str, release: &DamageRelease, plan: &Plan) -> (JobStatus, Option<String>) {
    let mut builder = HarnessBuilder::new().article_timeout(2);
    for (priority, id) in ["A", "B"].into_iter().enumerate() {
        builder = builder.with_server(
            ServerProfile::start(id, config(release, plan), 2)
                .await
                .with_priority(priority as u8),
        );
    }
    let engine = builder.build();
    let job_id = engine
        .submit_nzb_xml(name, release.nzb_xml(&[]))
        .expect("submit");
    assert!(
        engine
            .wait_for_status(&job_id, Duration::from_secs(90), TERMINAL)
            .await,
        "{name}: job never finished"
    );
    match engine.job(&job_id) {
        Some(job) => (job.status, job.error_message),
        None => {
            let entry = engine
                .queue_manager
                .history_get(&job_id)
                .expect("history")
                .expect("history entry");
            (entry.status, entry.error_message)
        }
    }
}

fn mkv_ids(release: &DamageRelease, numbers: impl IntoIterator<Item = u32>) -> Vec<String> {
    numbers
        .into_iter()
        .map(|n| release.data_message_id(0, n))
        .collect()
}

/// (a) Six articles missing everywhere, more than the old five-article
/// grace, but the release carries enough recovery slices: not aborted, and
/// PAR2 repairs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn six_missing_within_recovery_is_repaired() {
    let release = DamageRelease::generate_with_volumes("parity-ok", 0x5EED_1450, &[1, 2, 4, 8, 16]);
    let missing = mkv_ids(&release, [1, 3, 5, 7, 9, 11]);
    let damaged: BTreeSet<u32> = missing
        .iter()
        .flat_map(|mid| release.slices_touched(release.segment(mid).unwrap()))
        .collect();
    assert!(
        damaged.len() as u32 <= release.recovery_blocks(),
        "fixture must be repairable: {} damaged, {} recovery",
        damaged.len(),
        release.recovery_blocks()
    );
    let plan = Plan {
        missing: missing.into_iter().collect(),
        ..Plan::default()
    };
    let (status, error) = run("parity-ok", &release, &plan).await;
    assert_eq!(status, JobStatus::Completed, "error: {error:?}");
}

/// (b) Three articles missing everywhere and no recovery slices at all:
/// aborted, with a diagnosis that names slices and recovery slices.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_missing_without_recovery_is_aborted_with_diagnosis() {
    let release = DamageRelease::generate_with_volumes("parity-none", 0x5EED_1451, &[]);
    assert_eq!(release.recovery_blocks(), 0);
    let plan = Plan {
        missing: mkv_ids(&release, [2, 6, 10]).into_iter().collect(),
        ..Plan::default()
    };
    let (status, error) = run("parity-none", &release, &plan).await;
    assert_eq!(status, JobStatus::Failed);
    let error = error.expect("diagnosis recorded");
    assert!(
        error.contains("slice(s) missing but the release only carries 0 recovery slice(s)"),
        "unexpected diagnosis: {error}"
    );
}

/// (c) Six articles answered 430 twice by both servers and then served.
/// The refusals are transient, never confirmed missing, so even with no
/// recovery slices the job is not aborted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_refusals_that_clear_are_not_aborted() {
    let release = DamageRelease::generate_with_volumes("parity-busy", 0x5EED_1452, &[]);
    let plan = Plan {
        refuse_first: mkv_ids(&release, [1, 3, 5, 7, 9, 11])
            .into_iter()
            .map(|mid| (mid, mock_nntp_server::damage::BUSY_PROVIDER_REFUSALS))
            .collect(),
        ..Plan::default()
    };
    let (status, error) = run("parity-busy", &release, &plan).await;
    assert_eq!(status, JobStatus::Completed, "error: {error:?}");
}
