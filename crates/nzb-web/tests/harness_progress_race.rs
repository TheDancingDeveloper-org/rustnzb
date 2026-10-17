//! Stress the progress save against concurrent pause, resume and delete.
//!
//! The engine persists job progress on its own schedule. Control paths change
//! the in-memory status and then the database row. The database must never be
//! left holding a status that the in-memory job has already moved past, and a
//! removed job must not reappear because a late save wrote it back.

mod harness;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile, TestEngine, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::models::JobStatus;

const SEGMENTS: usize = 120;

/// One slow single-connection job, so it stays active while the control
/// threads run. Returns the engine and the job id once downloads have started.
async fn active_job(name: &str) -> (TestEngine, String) {
    let bodies: Vec<Vec<u8>> = (0..SEGMENTS)
        .map(|index| format!("{name}-segment-{index}").into_bytes())
        .collect();
    let message_ids: Vec<String> = (0..SEGMENTS).map(|i| format!("{name}-{i}@test")).collect();
    let segments: Vec<(&str, &[u8])> = message_ids
        .iter()
        .zip(&bodies)
        .map(|(id, body)| (id.as_str(), body.as_slice()))
        .collect();
    let fixture = NzbFixture::new(name)
        .add_file("payload.bin", &segments)
        .build();
    let articles: Vec<(&str, &[u8], &str)> = fixture
        .articles
        .iter()
        .map(|(id, body, filename)| (*id, *body, filename.as_str()))
        .collect();
    let server = ServerProfile::start(
        name,
        MockConfig {
            articles: yenc_articles(&articles),
            response_delay: Some(Duration::from_millis(10)),
            ..Default::default()
        },
        1,
    )
    .await;
    let engine = HarnessBuilder::new().with_server(server).build();
    let job_id = engine
        .submit_nzb_xml(name, fixture.xml)
        .expect("submit job");
    assert!(
        engine
            .wait_for(Duration::from_secs(5), |snapshot| {
                snapshot
                    .job(&job_id)
                    .is_some_and(|job| job.articles_downloaded >= 1)
            })
            .await,
        "download never started"
    );
    (engine, job_id)
}

/// The status of a job's queue row as the database holds it right now.
fn db_status(engine: &TestEngine, job_id: &str) -> Option<JobStatus> {
    engine
        .queue_manager
        .with_db(|db| db.queue_list())
        .expect("list queue rows")
        .into_iter()
        .find(|job| job.id == job_id)
        .map(|job| job.status)
}

fn memory_status(engine: &TestEngine, job_id: &str) -> Option<JobStatus> {
    engine.queue_manager.get_job(job_id).map(|job| job.status)
}

/// Poll until `condition` holds or the deadline passes. Returns the result.
async fn settle(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_resume_storm_leaves_the_database_on_the_last_status() {
    let (engine, job_id) = active_job("progress-storm").await;

    let runtime = tokio::runtime::Handle::current();
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let manager = Arc::clone(&engine.queue_manager);
            let job_id = job_id.clone();
            let runtime = runtime.clone();
            thread::spawn(move || {
                // Resuming a job spawns its progress task, which needs a runtime.
                let _entered = runtime.enter();
                for step in 0..40 {
                    // Errors are expected once the job has finished or been
                    // paused and resumed out of order; the invariant is below.
                    if (step + worker) % 2 == 0 {
                        let _ = manager.pause_job(&job_id);
                    } else {
                        let _ = manager.resume_job(&job_id);
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("control thread panicked");
    }

    if memory_status(&engine, &job_id).is_none() {
        // The job finished during the storm, which leaves no queue row to race.
        return;
    }
    engine
        .queue_manager
        .pause_job(&job_id)
        .expect("pause after the storm");
    assert!(
        settle(Duration::from_secs(3), || {
            db_status(&engine, &job_id) == Some(JobStatus::Paused)
        })
        .await,
        "database never recorded the final pause"
    );

    // A save still in flight from the storm must not undo the final pause.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        db_status(&engine, &job_id),
        Some(JobStatus::Paused),
        "a late progress save overwrote the final pause"
    );

    engine
        .queue_manager
        .resume_job(&job_id)
        .expect("resume after the final pause");
    assert!(
        settle(Duration::from_secs(3), || {
            let memory = memory_status(&engine, &job_id);
            memory.is_some()
                && memory != Some(JobStatus::Paused)
                && db_status(&engine, &job_id) == memory
        })
        .await,
        "database never caught up with the resume"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_during_pause_churn_leaves_no_queue_row_behind() {
    let (engine, job_id) = active_job("progress-delete").await;
    let stop = Arc::new(AtomicBool::new(false));

    let churn = {
        let manager = Arc::clone(&engine.queue_manager);
        let job_id = job_id.clone();
        let stop = Arc::clone(&stop);
        let runtime = tokio::runtime::Handle::current();
        thread::spawn(move || {
            let _entered = runtime.enter();
            while !stop.load(Ordering::Relaxed) {
                let _ = manager.pause_job(&job_id);
                let _ = manager.resume_job(&job_id);
            }
        })
    };

    // Let the churn race a few progress saves before the delete lands.
    tokio::time::sleep(Duration::from_millis(50)).await;
    engine
        .queue_manager
        .remove_job(&job_id)
        .expect("remove job");
    stop.store(true, Ordering::Relaxed);
    churn.join().expect("churn thread panicked");

    // Several save intervals later, nothing may have written the row back.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !engine.queue_manager.contains_job(&job_id),
        "removed job is still in memory"
    );
    assert_eq!(
        db_status(&engine, &job_id),
        None,
        "a progress save wrote a removed job back to the queue"
    );
}
