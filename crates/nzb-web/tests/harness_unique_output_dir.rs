//! Two jobs that share a name must not complete into the same directory.
//!
//! SABnzbd resolves a collision in the complete directory by appending `.1`,
//! `.2`, ... to the later job's folder. Without that, the second job's files
//! overwrite or merge into the first job's completed release.

mod harness;

use std::collections::HashMap;
use std::time::Duration;

use harness::nzb_fixture::NzbFixture;
use harness::{HarnessBuilder, ServerProfile, yenc_articles};
use nzb_nntp::testutil::MockConfig;
use nzb_web::nzb_core::models::{JobStatus, NzbJob};
use nzb_web::nzb_core::nzb_parser;

fn submit_named(engine: &harness::TestEngine, name: &str, xml: Vec<u8>) -> String {
    let mut job: NzbJob = nzb_parser::parse_nzb(name, &xml).expect("parse nzb");
    job.work_dir = engine.incomplete_dir.join(&job.id);
    // Production derives the output folder from the job name, so two jobs
    // with the same name start with the same target directory.
    job.output_dir = engine.complete_dir.join(name);
    let id = job.id.clone();
    engine
        .queue_manager
        .add_job(job, Some(xml))
        .expect("add job");
    id
}

#[tokio::test]
async fn same_named_jobs_complete_into_distinct_directories() {
    let first = NzbFixture::new("SameName")
        .add_file("release.bin", &[("same-name-1@test", b"first release")])
        .build();
    let second = NzbFixture::new("SameName")
        .add_file("release.bin", &[("same-name-2@test", b"second release")])
        .build();
    // Both releases carry a `release.bin`, so encode each NZB on its own:
    // `yenc_articles` treats same-named triples as segments of one file.
    let mut articles = HashMap::new();
    for fixture in [&first, &second] {
        let triples: Vec<(&str, &[u8], &str)> = fixture
            .articles
            .iter()
            .map(|(m, b, f)| (*m, *b, f.as_str()))
            .collect();
        articles.extend(yenc_articles(&triples));
    }
    let server = ServerProfile::start(
        "same-name",
        MockConfig {
            articles,
            ..Default::default()
        },
        4,
    )
    .await;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .max_active_downloads(1)
        .build();

    let first_id = submit_named(&engine, "SameName", first.xml);
    assert!(
        engine
            .wait_for_status(&first_id, Duration::from_secs(20), &[JobStatus::Completed])
            .await,
        "first job did not complete"
    );
    let second_id = submit_named(&engine, "SameName", second.xml);
    assert!(
        engine
            .wait_for_status(&second_id, Duration::from_secs(20), &[JobStatus::Completed])
            .await,
        "second job did not complete"
    );

    let first_entry = engine
        .queue_manager
        .history_get(&first_id)
        .unwrap()
        .expect("first history row");
    let second_entry = engine
        .queue_manager
        .history_get(&second_id)
        .unwrap()
        .expect("second history row");

    assert_eq!(first_entry.output_dir, engine.complete_dir.join("SameName"));
    assert_eq!(
        second_entry.output_dir,
        engine.complete_dir.join("SameName.1"),
        "the second same-named job must complete into a suffixed directory"
    );
    assert_eq!(
        std::fs::read(first_entry.output_dir.join("release.bin")).unwrap(),
        b"first release",
        "the first job's completed files must not be overwritten"
    );
    assert_eq!(
        std::fs::read(second_entry.output_dir.join("release.bin")).unwrap(),
        b"second release"
    );
}

#[tokio::test]
async fn concurrent_same_named_jobs_never_share_a_directory() {
    let names = ["a", "b", "c"];
    let bodies: [&[u8]; 3] = [b"body a", b"body b", b"body c"];
    let msg_ids = ["race-a@test", "race-b@test", "race-c@test"];
    let fixtures: Vec<_> = (0..3)
        .map(|i| {
            NzbFixture::new("Race")
                .add_file(
                    &format!("release-{}.bin", names[i]),
                    &[(msg_ids[i], bodies[i])],
                )
                .build()
        })
        .collect();
    let triples: Vec<(&str, &[u8], &str)> = fixtures
        .iter()
        .flat_map(|f| f.articles.iter())
        .map(|(m, b, f)| (*m, *b, f.as_str()))
        .collect();
    let server = ServerProfile::start(
        "race",
        MockConfig {
            articles: yenc_articles(&triples),
            ..Default::default()
        },
        6,
    )
    .await;
    let engine = HarnessBuilder::new()
        .with_server(server)
        .max_active_downloads(3)
        .build();

    let ids: Vec<String> = fixtures
        .into_iter()
        .map(|fixture| submit_named(&engine, "Race", fixture.xml))
        .collect();
    for id in &ids {
        assert!(
            engine
                .wait_for_status(id, Duration::from_secs(20), &[JobStatus::Completed])
                .await,
            "job {id} did not complete"
        );
    }

    let mut dirs: Vec<_> = ids
        .iter()
        .map(|id| {
            engine
                .queue_manager
                .history_get(id)
                .unwrap()
                .expect("history row")
                .output_dir
        })
        .collect();
    dirs.sort();
    assert_eq!(
        dirs,
        vec![
            engine.complete_dir.join("Race"),
            engine.complete_dir.join("Race.1"),
            engine.complete_dir.join("Race.2"),
        ]
    );
    for dir in &dirs {
        let entries = std::fs::read_dir(dir).unwrap().count();
        assert_eq!(
            entries,
            1,
            "{} must hold exactly one job's files",
            dir.display()
        );
    }
}
