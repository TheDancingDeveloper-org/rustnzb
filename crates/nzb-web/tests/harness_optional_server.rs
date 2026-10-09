//! An unavailable optional server must not be required before an article can
//! be declared missing.

mod harness;

use std::collections::HashMap;
use std::time::Duration;

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::models::JobStatus;

#[tokio::test]
async fn down_optional_server_does_not_block_missing_articles() {
    let fixture = NzbFixture::new("optional-down")
        .add_file(
            "payload.bin",
            &[
                ("optional-down-ok", b"present"),
                ("optional-down-missing", b"missing"),
            ],
        )
        .build();
    let triples = fixture
        .articles
        .iter()
        .map(|(id, bytes, file)| (*id, *bytes, file.as_str()))
        .collect::<Vec<_>>();
    let mut overrides = HashMap::new();
    overrides.insert("optional-down-missing".to_string(), 430);
    let primary = ServerProfile::start(
        "primary",
        MockConfig {
            articles: yenc_articles(&triples),
            article_response_overrides: overrides,
            ..MockConfig::default()
        },
        1,
    )
    .await;
    // The optional fill server refuses every connection (502 on connect), so
    // its circuit breaker opens and it never reports an outcome.
    let mut optional = ServerProfile::start(
        "optional",
        MockConfig {
            service_unavailable: true,
            ..MockConfig::default()
        },
        1,
    )
    .await
    .with_priority(1);
    optional.config.optional = true;

    let engine = HarnessBuilder::new()
        .with_server(primary)
        .with_server(optional)
        .article_timeout(120)
        .build();
    let job_id = engine
        .submit_nzb_xml("optional-down", fixture.xml)
        .expect("submit");

    assert!(
        engine
            .wait_for(Duration::from_secs(15), |snapshot| {
                snapshot.job(&job_id).is_none_or(|job| {
                    job.status != JobStatus::Downloading
                        || job.articles_downloaded + job.articles_failed == 2
                })
            })
            .await,
        "article missing on the primary stayed unresolved while the optional server was down"
    );
    if let Some(job) = engine.job(&job_id) {
        assert_eq!(job.articles_downloaded, 1);
        assert_eq!(job.articles_failed, 1);
    }
}
