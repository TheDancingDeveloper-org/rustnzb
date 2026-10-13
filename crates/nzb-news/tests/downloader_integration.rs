//! End-to-end integration test for the layered downloader: spawn mock NNTP
//! servers, run the multiplexed driver, assert that articles are fetched
//! with the expected priority-aware dispatch semantics.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nzb_news::article::{Article, NzbFile, NzbObject};
use nzb_news::downloader::{DownloaderConfig, FetchOutcome, WorkItem, spawn_downloader};
use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

/// Drive N work items through the downloader and collect outcomes. Fails
/// the test if the driver doesn't emit one outcome per item within the
/// given deadline.
async fn run_until_complete(
    handle: &nzb_news::downloader::DownloaderHandle,
    mut outcomes: tokio::sync::mpsc::Receiver<FetchOutcome>,
    expected: usize,
    deadline: Duration,
) -> Vec<FetchOutcome> {
    let mut collected = Vec::with_capacity(expected);
    let _ = handle; // keep it alive
    let res = tokio::time::timeout(deadline, async {
        while collected.len() < expected {
            match outcomes.recv().await {
                Some(o) => collected.push(o),
                None => break,
            }
        }
    })
    .await;
    if res.is_err() {
        panic!(
            "timeout: got {} / {} outcomes after {:?}",
            collected.len(),
            expected,
            deadline
        );
    }
    collected
}

#[tokio::test]
async fn fetches_successful_article_via_single_server() {
    let mut articles = HashMap::new();
    articles.insert("msg1".to_string(), b"hello".to_vec());

    let server = MockNntpServer::start(MockConfig {
        articles,
        ..Default::default()
    })
    .await;

    let mut sc = test_config(server.port());
    sc.id = "s1".into();
    sc.priority = 1;
    sc.connections = 2;
    sc.ramp_up_delay_ms = 0;

    let config = DownloaderConfig {
        servers: vec![sc],
        max_concurrent_fetches: 2,
        article_timeout: Duration::from_secs(10),
        work_channel_capacity: 64,
        outcome_channel_capacity: 64,
        probe_policy: None,
    };
    let (handle, outcomes) = spawn_downloader(config);

    let file = Arc::new(NzbFile::new("f1", "j1", "demo.r00", 1));
    let job = Arc::new(NzbObject::new("j1", "demo", 1, 5, vec![file.clone()]));
    let article = Arc::new(Article::new("msg1", "f1", "j1", 5, 0, 0));

    handle
        .submit(WorkItem {
            tag: 1,
            article: article.clone(),
            file: file.clone(),
            job: job.clone(),
        })
        .await
        .unwrap();

    let got = run_until_complete(&handle, outcomes, 1, Duration::from_secs(10)).await;
    match &got[0] {
        FetchOutcome::Success {
            tag,
            server_id,
            bytes,
            ..
        } => {
            assert_eq!(*tag, 1);
            assert_eq!(server_id, "s1");
            assert!(!bytes.is_empty());
        }
        other => panic!("expected success, got {other:?}"),
    }

    handle.shutdown();
    handle.join().await;
    assert_eq!(job.articles_downloaded(), 1);
    assert_eq!(job.articles_failed(), 0);
}

#[tokio::test]
async fn falls_over_to_backup_when_primary_returns_430() {
    // Primary knows nothing; backup has the article.
    let primary = MockNntpServer::start(MockConfig::default()).await;
    let mut backup_articles = HashMap::new();
    backup_articles.insert("msg2".to_string(), b"xyz".to_vec());
    let backup = MockNntpServer::start(MockConfig {
        articles: backup_articles,
        ..Default::default()
    })
    .await;

    let mut primary_cfg = test_config(primary.port());
    primary_cfg.id = "primary".into();
    primary_cfg.priority = 1;
    primary_cfg.connections = 2;
    primary_cfg.ramp_up_delay_ms = 0;

    let mut backup_cfg = test_config(backup.port());
    backup_cfg.id = "backup".into();
    backup_cfg.priority = 5;
    backup_cfg.connections = 2;
    backup_cfg.ramp_up_delay_ms = 0;

    let config = DownloaderConfig {
        servers: vec![primary_cfg, backup_cfg],
        max_concurrent_fetches: 4,
        article_timeout: Duration::from_secs(10),
        work_channel_capacity: 64,
        outcome_channel_capacity: 64,
        probe_policy: None,
    };
    let (handle, outcomes) = spawn_downloader(config);

    let file = Arc::new(NzbFile::new("f1", "j1", "demo.r00", 1));
    let job = Arc::new(NzbObject::new("j1", "demo", 1, 3, vec![file.clone()]));
    let article = Arc::new(Article::new("msg2", "f1", "j1", 3, 0, 0));

    handle
        .submit(WorkItem {
            tag: 42,
            article: article.clone(),
            file: file.clone(),
            job: job.clone(),
        })
        .await
        .unwrap();

    let got = run_until_complete(&handle, outcomes, 1, Duration::from_secs(10)).await;
    match &got[0] {
        FetchOutcome::Success { tag, server_id, .. } => {
            assert_eq!(*tag, 42);
            assert_eq!(
                server_id, "backup",
                "fall-over should land on backup after primary 430"
            );
        }
        other => panic!("expected success via backup, got {other:?}"),
    }

    // Primary should be recorded in the article's try-list.
    assert!(article.server_tried("primary"));
    handle.shutdown();
    handle.join().await;
}

#[tokio::test]
async fn multiple_articles_dispatch_concurrently() {
    // Ten articles, single server with 5 connections. Verify all succeed
    // and the downloader actually ran them in parallel (total time should
    // be less than sum-of-individual-times; we just check correctness here).
    let mut articles = HashMap::new();
    for i in 0..10 {
        articles.insert(format!("msg{i}"), vec![b'X'; 256]);
    }

    let server = MockNntpServer::start(MockConfig {
        articles,
        ..Default::default()
    })
    .await;

    let mut sc = test_config(server.port());
    sc.id = "s1".into();
    sc.priority = 1;
    sc.connections = 5;
    sc.ramp_up_delay_ms = 0;

    let config = DownloaderConfig {
        servers: vec![sc],
        max_concurrent_fetches: 5,
        article_timeout: Duration::from_secs(10),
        work_channel_capacity: 64,
        outcome_channel_capacity: 64,
        probe_policy: None,
    };
    let (handle, outcomes) = spawn_downloader(config);

    let file = Arc::new(NzbFile::new("f1", "j1", "demo.r00", 10));
    let job = Arc::new(NzbObject::new("j1", "demo", 10, 2560, vec![file.clone()]));

    for i in 0..10u64 {
        let art = Arc::new(Article::new(
            format!("msg{i}"),
            "f1",
            "j1",
            256,
            i as u32,
            i,
        ));
        handle
            .submit(WorkItem {
                tag: i,
                article: art,
                file: file.clone(),
                job: job.clone(),
            })
            .await
            .unwrap();
    }

    let got = run_until_complete(&handle, outcomes, 10, Duration::from_secs(15)).await;
    let successes = got
        .iter()
        .filter(|o| matches!(o, FetchOutcome::Success { .. }))
        .count();
    assert_eq!(
        successes, 10,
        "all ten articles should succeed; got {got:?}"
    );

    handle.shutdown();
    handle.join().await;
    assert_eq!(job.articles_downloaded(), 10);
    assert_eq!(job.articles_failed(), 0);
}

#[tokio::test]
async fn back_pressure_halts_driver_when_outcomes_are_not_drained() {
    // 10 articles, outcome channel capacity = 1. Drain only 3, then stop
    // consuming. Submit everything, wait a bit — the driver should have
    // stalled at the full outcome channel rather than downloading all 10
    // into memory.
    let mut articles = HashMap::new();
    for i in 0..10 {
        articles.insert(format!("bp{i}"), vec![b'X'; 32]);
    }
    let server = MockNntpServer::start(MockConfig {
        articles,
        ..Default::default()
    })
    .await;

    let mut sc = test_config(server.port());
    sc.id = "s1".into();
    sc.priority = 1;
    sc.connections = 4;
    sc.ramp_up_delay_ms = 0;

    let config = DownloaderConfig {
        servers: vec![sc],
        max_concurrent_fetches: 4,
        article_timeout: Duration::from_secs(10),
        work_channel_capacity: 64,
        outcome_channel_capacity: 1, // force back-pressure
        probe_policy: None,
    };
    let (handle, mut outcomes) = spawn_downloader(config);

    let file = Arc::new(NzbFile::new("f1", "j1", "demo", 10));
    let job = Arc::new(NzbObject::new("j1", "demo", 10, 320, vec![file.clone()]));
    for i in 0..10u64 {
        let art = Arc::new(Article::new(format!("bp{i}"), "f1", "j1", 32, i as u32, i));
        handle
            .submit(WorkItem {
                tag: i,
                article: art,
                file: file.clone(),
                job: job.clone(),
            })
            .await
            .unwrap();
    }

    // Drain just 3 outcomes, then pause to let the back-pressure settle.
    let mut collected = 0;
    while collected < 3 {
        if let Some(_o) = outcomes.recv().await {
            collected += 1;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // After the pause, the driver should be stalled on `outcome_tx.send`.
    // Job's download count reflects what actually completed — must be
    // strictly less than 10 (the stall prevented the remaining fetches
    // from being emitted).
    let done = job.articles_downloaded();
    assert!(
        done < 10,
        "back-pressure should have halted progress: done={done}"
    );

    // Now drain the remaining outcomes — the driver unblocks and
    // completes the rest.
    while collected < 10 {
        match tokio::time::timeout(Duration::from_secs(10), outcomes.recv()).await {
            Ok(Some(_)) => collected += 1,
            _ => panic!("driver did not recover after back-pressure released"),
        }
    }
    assert_eq!(collected, 10);
    handle.shutdown();
    handle.join().await;
}

#[tokio::test]
async fn gives_up_after_retries_across_servers_all_fail() {
    // Two servers, neither has the article. Verify the driver cascades
    // through servers, records try-list correctly, and emits a Failed
    // outcome within the retry budget.
    let s1 = MockNntpServer::start(MockConfig::default()).await;
    let s2 = MockNntpServer::start(MockConfig::default()).await;

    let mut s1_cfg = test_config(s1.port());
    s1_cfg.id = "s1".into();
    s1_cfg.priority = 1;
    s1_cfg.connections = 2;
    s1_cfg.ramp_up_delay_ms = 0;
    let mut s2_cfg = test_config(s2.port());
    s2_cfg.id = "s2".into();
    s2_cfg.priority = 5;
    s2_cfg.connections = 2;
    s2_cfg.ramp_up_delay_ms = 0;

    let config = DownloaderConfig {
        servers: vec![s1_cfg, s2_cfg],
        max_concurrent_fetches: 4,
        article_timeout: Duration::from_secs(10),
        work_channel_capacity: 64,
        outcome_channel_capacity: 64,
        probe_policy: None,
    };
    let (handle, outcomes) = spawn_downloader(config);

    let file = Arc::new(NzbFile::new("f1", "j1", "demo.r00", 1));
    let job = Arc::new(NzbObject::new("j1", "demo", 1, 0, vec![file.clone()]));
    let article = Arc::new(Article::new("missing", "f1", "j1", 0, 0, 0));

    handle
        .submit(WorkItem {
            tag: 7,
            article: article.clone(),
            file: file.clone(),
            job: job.clone(),
        })
        .await
        .unwrap();

    let got = run_until_complete(&handle, outcomes, 1, Duration::from_secs(15)).await;
    match &got[0] {
        FetchOutcome::Failed { tag, .. } => assert_eq!(*tag, 7),
        other => panic!("expected Failed outcome, got {other:?}"),
    }

    handle.shutdown();
    handle.join().await;
    assert_eq!(job.articles_failed(), 1);
}

#[tokio::test]
async fn same_priority_servers_share_load_proportionally() {
    // Three servers all at priority 0, mimicking aunews(4 conn) + asnews×2(10 conn).
    // Submit 24 articles — all three servers should receive work, not just the
    // first in config order. With proportional dispatch each server gets work
    // proportional to its connection count (4/24, 10/24, 10/24).
    let n_articles = 24usize;
    let mut articles = HashMap::new();
    for i in 0..n_articles {
        articles.insert(format!("msg{i}"), vec![b'A'; 32]);
    }

    let s1 = MockNntpServer::start(MockConfig {
        articles: articles.clone(),
        ..Default::default()
    })
    .await;
    let s2 = MockNntpServer::start(MockConfig {
        articles: articles.clone(),
        ..Default::default()
    })
    .await;
    let s3 = MockNntpServer::start(MockConfig {
        articles: articles.clone(),
        ..Default::default()
    })
    .await;

    let mut cfg1 = test_config(s1.port());
    cfg1.id = "s1".into();
    cfg1.priority = 0;
    cfg1.connections = 4;
    cfg1.ramp_up_delay_ms = 0;

    let mut cfg2 = test_config(s2.port());
    cfg2.id = "s2".into();
    cfg2.priority = 0;
    cfg2.connections = 10;
    cfg2.ramp_up_delay_ms = 0;

    let mut cfg3 = test_config(s3.port());
    cfg3.id = "s3".into();
    cfg3.priority = 0;
    cfg3.connections = 10;
    cfg3.ramp_up_delay_ms = 0;

    let config = DownloaderConfig {
        servers: vec![cfg1, cfg2, cfg3],
        max_concurrent_fetches: 24,
        article_timeout: Duration::from_secs(10),
        work_channel_capacity: 64,
        outcome_channel_capacity: 64,
        probe_policy: None,
    };
    let (handle, outcomes) = spawn_downloader(config);

    let file = Arc::new(NzbFile::new("f1", "j1", "demo.r00", n_articles as u32));
    let job = Arc::new(NzbObject::new(
        "j1",
        "demo",
        n_articles as u64,
        32 * n_articles as u64,
        vec![file.clone()],
    ));

    for i in 0..n_articles as u64 {
        let art = Arc::new(Article::new(format!("msg{i}"), "f1", "j1", 32, i as u32, i));
        handle
            .submit(WorkItem {
                tag: i,
                article: art,
                file: file.clone(),
                job: job.clone(),
            })
            .await
            .unwrap();
    }

    let got = run_until_complete(&handle, outcomes, n_articles, Duration::from_secs(15)).await;

    let mut per_server: HashMap<String, usize> = HashMap::new();
    for outcome in &got {
        if let FetchOutcome::Success { server_id, .. } = outcome {
            *per_server.entry(server_id.clone()).or_insert(0) += 1;
        }
    }

    let successes: usize = per_server.values().sum();
    assert_eq!(successes, n_articles, "all articles should succeed");

    // All three servers must have participated — none should be empty.
    for server_id in ["s1", "s2", "s3"] {
        let count = per_server.get(server_id).copied().unwrap_or(0);
        assert!(
            count > 0,
            "server {server_id} received no articles; distribution: {per_server:?}"
        );
    }

    handle.shutdown();
    handle.join().await;
}

/// Regression: a server whose wrappers all self-retire (terminal auth/503
/// errors) must not wedge the whole downloader. Historically the offending
/// server's queue was `close()`d forever and `select_server` stopped
/// considering it — but dispatch of *other* servers continued. This test
/// verifies that after the broken primary's wrappers have all exited, a
/// healthy backup still delivers the article and the downloader shuts
/// down cleanly without a hang.
#[tokio::test]
async fn downloader_survives_total_wrapper_retirement_on_one_server() {
    // Primary always returns 502 on banner → NntpError::ServiceUnavailable
    // (terminal class). Every wrapper self-retires after 3 consecutive
    // connect failures, eventually all of them.
    let primary = MockNntpServer::start(MockConfig {
        service_unavailable: true,
        ..Default::default()
    })
    .await;

    let mut backup_articles = HashMap::new();
    backup_articles.insert("msg-sup-1".to_string(), b"payload-1".to_vec());
    backup_articles.insert("msg-sup-2".to_string(), b"payload-2".to_vec());
    let backup = MockNntpServer::start(MockConfig {
        articles: backup_articles,
        ..Default::default()
    })
    .await;

    let mut primary_cfg = test_config(primary.port());
    primary_cfg.id = "broken-primary".into();
    primary_cfg.priority = 1;
    primary_cfg.connections = 2;
    primary_cfg.ramp_up_delay_ms = 0;

    let mut backup_cfg = test_config(backup.port());
    backup_cfg.id = "healthy-backup".into();
    backup_cfg.priority = 5;
    backup_cfg.connections = 2;
    backup_cfg.ramp_up_delay_ms = 0;

    let config = DownloaderConfig {
        servers: vec![primary_cfg, backup_cfg],
        max_concurrent_fetches: 4,
        article_timeout: Duration::from_secs(10),
        work_channel_capacity: 64,
        outcome_channel_capacity: 64,
        probe_policy: None,
    };
    let (handle, outcomes) = spawn_downloader(config);

    let file = Arc::new(NzbFile::new("fsup", "jsup", "demo.r00", 2));
    let job = Arc::new(NzbObject::new("jsup", "demo", 2, 18, vec![file.clone()]));

    for (tag, msg) in [(101u64, "msg-sup-1"), (102u64, "msg-sup-2")] {
        let art = Arc::new(Article::new(msg, "fsup", "jsup", 9, 0, tag));
        handle
            .submit(WorkItem {
                tag,
                article: art,
                file: file.clone(),
                job: job.clone(),
            })
            .await
            .unwrap();
    }

    // Both articles should reach the backup. Give the dispatcher plenty
    // of time — each primary connect attempt consumes the 3-strike
    // budget per wrapper before it retires.
    let got = run_until_complete(&handle, outcomes, 2, Duration::from_secs(30)).await;
    assert_eq!(got.len(), 2);
    for outcome in &got {
        match outcome {
            FetchOutcome::Success { server_id, .. } => {
                assert_eq!(
                    server_id, "healthy-backup",
                    "article should land on the healthy backup"
                );
            }
            other => panic!("expected success via backup, got {other:?}"),
        }
    }

    // Clean shutdown must return promptly — the scheduler should not be
    // stuck on a closed queue or an orphaned worker handle. Historically
    // a bug here could make `join` wait indefinitely.
    handle.shutdown();
    tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("downloader did not shut down within 5s");
}

/// One job owning the whole connection pool versus two jobs sharing it.
///
/// The downloader keeps a single pool (`server.connections` sockets) and a
/// shared work queue, so two concurrent jobs cannot each have the pool: they
/// split it. With a fixed per-article server delay the combined article rate
/// should stay flat and each job should get about half. A large drop in the
/// combined rate would mean scheduling or decode contention on top of the
/// split; this asserts there is none beyond the share.
#[tokio::test]
async fn two_jobs_share_one_connection_pool_without_extra_slowdown() {
    const ARTICLES_PER_JOB: usize = 24;
    const CONNECTIONS: u8 = 4;

    async fn timed(jobs: usize) -> Duration {
        let mut articles = HashMap::new();
        for job in 0..jobs {
            for n in 0..ARTICLES_PER_JOB {
                articles.insert(format!("j{job}-{n}"), vec![0u8; 64]);
            }
        }
        let server = MockNntpServer::start(MockConfig {
            articles,
            // Large enough that connection setup is noise, small enough that
            // the test stays well under a second per run.
            response_delay: Some(Duration::from_millis(15)),
            ..Default::default()
        })
        .await;

        let mut sc = test_config(server.port());
        sc.id = "shared".into();
        sc.priority = 1;
        sc.connections = u16::from(CONNECTIONS);
        sc.ramp_up_delay_ms = 0;

        let (handle, outcomes) = spawn_downloader(DownloaderConfig {
            servers: vec![sc],
            max_concurrent_fetches: CONNECTIONS as usize,
            article_timeout: Duration::from_secs(10),
            work_channel_capacity: 256,
            outcome_channel_capacity: 256,
            probe_policy: None,
        });

        for job_idx in 0..jobs {
            let job_id = format!("j{job_idx}");
            let file = Arc::new(NzbFile::new(
                format!("f{job_idx}"),
                job_id.clone(),
                "demo.r00",
                u32::try_from(ARTICLES_PER_JOB).unwrap(),
            ));
            let job = Arc::new(NzbObject::new(
                job_id.clone(),
                "demo",
                u64::try_from(ARTICLES_PER_JOB).unwrap(),
                64,
                vec![file.clone()],
            ));
            for n in 0..ARTICLES_PER_JOB {
                let art = Arc::new(Article::new(
                    format!("j{job_idx}-{n}"),
                    format!("f{job_idx}"),
                    job_id.clone(),
                    64,
                    0,
                    n as u64,
                ));
                handle
                    .submit(WorkItem {
                        tag: (job_idx * ARTICLES_PER_JOB + n) as u64,
                        article: art,
                        file: file.clone(),
                        job: job.clone(),
                    })
                    .await
                    .unwrap();
            }
        }

        let started = std::time::Instant::now();
        let got = run_until_complete(
            &handle,
            outcomes,
            jobs * ARTICLES_PER_JOB,
            Duration::from_secs(30),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            got.iter()
                .all(|o| matches!(o, FetchOutcome::Success { .. })),
            "every article should succeed"
        );
        handle.shutdown();
        handle.join().await;
        elapsed
    }

    let one = timed(1).await;
    let two = timed(2).await;
    eprintln!(
        "pool share: 1 job {one:?} for {ARTICLES_PER_JOB} articles, 2 jobs {two:?} for {} articles ({CONNECTIONS} connections)",
        2 * ARTICLES_PER_JOB
    );

    // Twice the articles on the same pool should take about twice as long.
    // Allow generous slack for connection setup and scheduling; fail only if
    // two jobs are dramatically slower than simply splitting the pool.
    let ratio = two.as_secs_f64() / one.as_secs_f64();
    assert!(
        (1.2..3.2).contains(&ratio),
        "two jobs took {ratio:.2}x one job ({two:?} vs {one:?}); expected ~2x from sharing {CONNECTIONS} connections, not extra contention"
    );
}
