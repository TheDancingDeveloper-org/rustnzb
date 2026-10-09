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
