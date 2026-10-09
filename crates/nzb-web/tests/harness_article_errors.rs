//! Article-level NNTP errors other than 430 must fail over instead of being
//! retried forever on the same server.

mod harness;

use std::collections::HashMap;
use std::time::Duration;

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::models::JobStatus;

async fn run_with_pipelining(name: &str, pipelining: u8) {
    let ok_id = format!("{name}-ok");
    let no_number_id = format!("{name}-423");
    let rejected_id = format!("{name}-451");
    let fixture = NzbFixture::new(name)
        .add_file(
            "payload.bin",
            &[
                (ok_id.as_str(), b"ok-body"),
                (no_number_id.as_str(), b"missing-423"),
                (rejected_id.as_str(), b"missing-451"),
            ],
        )
        .build();
    let triples = fixture
        .articles
        .iter()
        .map(|(id, bytes, file)| (*id, *bytes, file.as_str()))
        .collect::<Vec<_>>();
    let mut overrides = HashMap::new();
    // 423 is an article-level "not here" answer; 451 is an unexpected
    // response the client classifies as a protocol error.
    overrides.insert(no_number_id.clone(), 423);
    overrides.insert(rejected_id.clone(), 451);
    let mut server = ServerProfile::start(
        name,
        MockConfig {
            articles: yenc_articles(&triples),
            article_response_overrides: overrides,
            ..MockConfig::default()
        },
        1,
    )
    .await;
    server.config.pipelining = pipelining;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .article_timeout(120)
        .build();
    let job_id = engine.submit_nzb_xml(name, fixture.xml).expect("submit");

    assert!(
        engine
            .wait_for(Duration::from_secs(20), |snapshot| {
                snapshot.job(&job_id).is_none_or(|job| {
                    job.status != JobStatus::Downloading
                        || job.articles_downloaded + job.articles_failed == 3
                })
            })
            .await,
        "articles answered with 423/451 were retried forever on the only server"
    );
    if let Some(job) = engine.job(&job_id) {
        assert_eq!(job.articles_downloaded, 1);
        assert_eq!(job.articles_failed, 2);
    }
}

#[tokio::test]
async fn article_errors_fail_over_serial() {
    run_with_pipelining("article-errors-serial", 1).await;
}

#[tokio::test]
async fn article_errors_fail_over_pipelined() {
    run_with_pipelining("article-errors-pipe", 4).await;
}

/// Pipelined article-level errors (here an unexpected 451) must not count
/// against the server's circuit breaker. When several workers each hit one
/// before any of them reconnects, three such answers used to trip the breaker
/// and pause the only server for 30 s, stalling every other article.
#[tokio::test]
async fn pipelined_article_errors_do_not_trip_circuit_breaker() {
    const CONNECTIONS: u16 = 3;
    const PIPELINING: u8 = 2;
    // Enough rejected articles at the front of the queue that every worker's
    // first pipelined batch is all 451s, so all workers report an error
    // before any of their reconnects completes.
    const REJECTED_ARTICLES: usize = CONNECTIONS as usize * PIPELINING as usize;
    const OK_ARTICLES: usize = 12;
    let name = "article-errors-breaker";
    let rejected_ids = (0..REJECTED_ARTICLES)
        .map(|i| format!("{name}-451-{i}"))
        .collect::<Vec<_>>();
    let ok_ids = (0..OK_ARTICLES)
        .map(|i| format!("{name}-ok-{i}"))
        .collect::<Vec<_>>();
    let mut segments: Vec<(&str, &[u8])> = rejected_ids
        .iter()
        .map(|id| (id.as_str(), b"rejected".as_slice()))
        .collect();
    segments.extend(ok_ids.iter().map(|id| (id.as_str(), b"ok-body".as_slice())));
    let fixture = NzbFixture::new(name)
        .add_file("payload.bin", &segments)
        .build();
    let triples = fixture
        .articles
        .iter()
        .map(|(id, bytes, file)| (*id, *bytes, file.as_str()))
        .collect::<Vec<_>>();
    let overrides = rejected_ids
        .iter()
        .map(|id| (id.clone(), 451))
        .collect::<HashMap<_, _>>();
    let mut server = ServerProfile::start(
        name,
        MockConfig {
            articles: yenc_articles(&triples),
            article_response_overrides: overrides,
            // A reconnect (banner + CAPABILITIES) takes several delayed
            // writes, longer than the worker start-up stagger, so the workers'
            // first errors land before any reconnect resets the failure count.
            response_delay: Some(Duration::from_millis(80)),
            ..MockConfig::default()
        },
        CONNECTIONS,
    )
    .await;
    server.config.pipelining = PIPELINING;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .article_timeout(120)
        .abort_hopeless(false)
        .early_failure_check(false)
        .build();
    let job_id = engine.submit_nzb_xml(name, fixture.xml).expect("submit");

    // Under the 30 s transient circuit-breaker cooldown, so a tripped breaker
    // fails the test.
    assert!(
        engine
            .wait_for(Duration::from_secs(25), |snapshot| {
                snapshot.job(&job_id).is_none_or(|job| {
                    job.articles_downloaded == OK_ARTICLES
                        && job.articles_failed == REJECTED_ARTICLES
                })
            })
            .await,
        "pipelined 451s paused the server: {:?}",
        engine.job(&job_id)
    );
    match engine.job(&job_id) {
        Some(job) => {
            assert_eq!(job.articles_downloaded, OK_ARTICLES);
            assert_eq!(job.articles_failed, REJECTED_ARTICLES);
        }
        None => assert!(engine.history_status(&job_id).is_some()),
    }
}
