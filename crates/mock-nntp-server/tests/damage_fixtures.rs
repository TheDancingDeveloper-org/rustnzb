//! WI-151: deterministic PAR2 damage fixtures.
//!
//! The recovery volumes repair byte-exact through `rust_par2`, each scenario's
//! disk state verifies to exactly the damage its expectation names, and the
//! mock servers show each provider pathology on the wire.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use mock_nntp_server::damage::{
    BUSY_PROVIDER_DELAY, DamageRelease, DamageScenario, ScenarioFixture,
};
use mock_nntp_server::par2::Par2Fixture;
use mock_nntp_server::server_config;
use nzb_nntp::NntpConnection;
use nzb_nntp::error::NntpError;
use nzb_nntp::testutil::MockNntpServer;

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 251) as u8))
        .collect()
}

/// Global indices of every slice `rust_par2::verify` reports damaged or
/// missing, mapped through the fixture's file order.
fn verified_damage(par2: &Par2Fixture, result: &rust_par2::VerifyResult) -> BTreeSet<u32> {
    let mut damage = BTreeSet::new();
    for file in &result.damaged {
        let base = par2.first_global_slice(&file.filename).unwrap();
        damage.extend(file.damaged_block_indices.iter().map(|i| base + i));
    }
    for file in &result.missing {
        let base = par2.first_global_slice(&file.filename).unwrap();
        damage.extend(base..base + file.block_count);
    }
    damage
}

#[test]
fn recovery_volume_repairs_damaged_slices_byte_exact() {
    let dir = tempfile::tempdir().unwrap();
    let a = payload(20_000, 0x10); // 5 slices, last partial
    let b = payload(9_000, 0x70); // 3 slices, last partial
    let par2 = Par2Fixture::new()
        .add_file("a.bin", &a)
        .add_file("b.bin", &b);
    par2.write_index(&dir.path().join("set.par2"));
    par2.write_volume(&dir.path().join("set.vol00+03.par2"), &[0, 1, 2]);

    // Three damaged slices across both files, including a partial last slice.
    let mut bad_a = a.clone();
    bad_a[100] ^= 0xFF; // slice 0
    bad_a[19_000] ^= 0xFF; // slice 4 (partial)
    let mut bad_b = b.clone();
    bad_b[5_000] ^= 0xFF; // slice 1
    fs::write(dir.path().join("a.bin"), &bad_a).unwrap();
    fs::write(dir.path().join("b.bin"), &bad_b).unwrap();

    let set = rust_par2::parse(&dir.path().join("set.par2")).unwrap();
    let result = rust_par2::repair(&set, dir.path()).unwrap();
    assert!(result.success, "repair must succeed: {result}");
    assert_eq!(result.blocks_repaired, 3);
    assert_eq!(fs::read(dir.path().join("a.bin")).unwrap(), a);
    assert_eq!(fs::read(dir.path().join("b.bin")).unwrap(), b);
}

#[test]
fn damage_beyond_the_recovery_slices_cannot_be_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let a = payload(20_000, 0x33);
    let par2 = Par2Fixture::new().add_file("a.bin", &a);
    par2.write_index(&dir.path().join("set.par2"));
    par2.write_volume(&dir.path().join("set.vol00+01.par2"), &[0]);

    let mut bad = a.clone();
    bad[0] ^= 0xFF;
    bad[8_192] ^= 0xFF;
    fs::write(dir.path().join("a.bin"), &bad).unwrap();

    let set = rust_par2::parse(&dir.path().join("set.par2")).unwrap();
    let repaired = rust_par2::repair(&set, dir.path());
    assert!(
        repaired.map(|r| !r.success).unwrap_or(true),
        "two damaged slices must not repair from one recovery slice"
    );
}

#[test]
fn release_is_reproducible_and_volumes_carry_their_exponents() {
    let first = DamageRelease::generate("repro", 7);
    let second = DamageRelease::generate("repro", 7);
    assert_eq!(first.encoded_articles(), second.encoded_articles());
    assert_ne!(
        first.data_files[0].payload,
        DamageRelease::generate("repro", 8).data_files[0].payload,
        "a different seed must give different bytes"
    );

    assert_eq!(first.par2.total_slices(), 31);
    assert_eq!(first.recovery_blocks(), 15);
    let exponents: Vec<u32> = first
        .volumes
        .iter()
        .flat_map(|v| v.exponents.iter().copied())
        .collect();
    assert_eq!(exponents, (0..15).collect::<Vec<_>>());
    assert_eq!(first.volumes[1].file.filename, "repro.vol01+02.par2");

    // Articles straddle slices: the second mkv article covers bytes
    // 10_000..20_000, which is slices 2..=4 of that file.
    let mkv = &first.data_files[0];
    let base = first.par2.first_global_slice(&mkv.filename).unwrap();
    assert_eq!(
        first.slices_touched(&mkv.segments[1]),
        (base + 2..=base + 4).collect()
    );
    assert!(
        first
            .slices_touched(&first.par2_index.segments[0])
            .is_empty()
    );
}

#[test]
fn every_scenario_verifies_to_its_expected_damage_and_repairs_accordingly() {
    for scenario in DamageScenario::ALL {
        let fixture = scenario.fixture();
        let dir = tempfile::tempdir().unwrap();
        fixture.write_received(dir.path());
        let release = &fixture.release;
        let expect = &fixture.expect;

        let index = dir.path().join(&release.par2_index.filename);
        let set = rust_par2::parse(&index).unwrap();
        let verified = rust_par2::verify(&set, dir.path());

        let at_risk: BTreeSet<u32> = expect
            .damaged_slices
            .union(&expect.unknown_slices)
            .copied()
            .collect();
        assert_eq!(
            verified_damage(&release.par2, &verified),
            at_risk,
            "{scenario:?}: verify must see exactly the damaged and unknown slices"
        );
        let missing: Vec<String> = verified
            .missing
            .iter()
            .map(|m| m.filename.clone())
            .collect();
        assert_eq!(missing, expect.files_absent_from_nzb, "{scenario:?}");

        if at_risk.is_empty() {
            assert!(verified.all_correct(), "{scenario:?}: {verified}");
            assert!(expect.repairable);
            continue;
        }

        let repaired = rust_par2::repair(&set, dir.path());
        if expect.repairable {
            let repaired = repaired.unwrap_or_else(|e| panic!("{scenario:?}: {e}"));
            assert!(repaired.success, "{scenario:?}: {repaired}");
            assert_data_files_equal_payload(&fixture, dir.path());
            assert_eq!(expect.hopeless_diagnosis(), None);
        } else {
            assert!(
                repaired.map(|r| !r.success).unwrap_or(true),
                "{scenario:?}: must not repair"
            );
        }
    }
}

fn assert_data_files_equal_payload(fixture: &ScenarioFixture, dir: &Path) {
    for file in &fixture.release.data_files {
        assert_eq!(
            fs::read(dir.join(&file.filename)).unwrap(),
            file.payload,
            "{:?}: {} must equal the seeded payload",
            fixture.scenario,
            file.filename
        );
    }
}

#[test]
fn scenario_expectations_name_the_documented_damage() {
    let missing = DamageScenario::MissingEverywhere.fixture().expect;
    assert_eq!(missing.missing_articles.len(), 2);
    assert!(missing.repairable);

    let corrupt = DamageScenario::CorruptBody.fixture().expect;
    assert_eq!(
        corrupt.damaged_slices.len(),
        1,
        "one flipped byte, one slice"
    );

    let hopeless = DamageScenario::DamageExceedsRecovery.fixture().expect;
    assert!(!hopeless.repairable);
    assert!(hopeless.damaged_slices.len() as u32 > hopeless.recovery_blocks);
    assert_eq!(
        hopeless.hopeless_diagnosis().unwrap(),
        format!(
            "{} slice(s) missing but the release only carries 15 recovery slice(s)",
            hopeless.damaged_slices.len()
        )
    );

    let timeout = DamageScenario::TransportTimeoutOnLastServer
        .fixture()
        .expect;
    assert!(
        timeout.damaged_slices.is_empty(),
        "unestablished is not damage"
    );
    assert!(!timeout.unknown_slices.is_empty());

    for transient in [
        DamageScenario::MissingOnPrimaryOnly,
        DamageScenario::BusyProvider,
    ] {
        let expect = transient.fixture().expect;
        assert!(!expect.transient_articles.is_empty(), "{transient:?}");
        assert!(expect.damaged_slices.is_empty(), "{transient:?}");
    }

    let partial = DamageScenario::PartialPost.fixture();
    let nzb = String::from_utf8(partial.nzb_xml()).unwrap();
    let nfo = &partial.release.data_files[1];
    assert!(
        !nzb.contains(&nfo.filename),
        "the NZB must not carry the nfo"
    );
    assert!(nzb.contains(&partial.release.data_files[0].filename));
    assert!(nzb.contains(&partial.release.par2_index.filename));
    assert_eq!(
        partial.expect.damaged_slices,
        partial.release.file_slices(&nfo.filename)
    );
}

// ---------------------------------------------------------------------------
// On the wire
// ---------------------------------------------------------------------------

async fn connect(server: &MockNntpServer) -> NntpConnection {
    let config = server_config(server.port());
    let mut connection = NntpConnection::new(config.id.clone());
    connection.connect(&config).await.unwrap();
    connection
}

/// Fetch and decode an article; `None` on 430.
async fn fetch(connection: &mut NntpConnection, mid: &str) -> Option<Vec<u8>> {
    match connection.fetch_article(mid).await {
        Ok(response) => {
            let data = response.data.expect("article body");
            Some(yenc_simd::decode_yenc(&data).unwrap().data)
        }
        Err(NntpError::ArticleNotFound(_)) => None,
        Err(e) => panic!("unexpected error fetching {mid}: {e}"),
    }
}

/// Every article not named by the expectation is served intact by the
/// primary, and missing articles are refused by every server.
#[tokio::test]
async fn every_scenario_serves_the_intact_articles_and_refuses_the_missing() {
    for scenario in DamageScenario::ALL {
        let fixture = scenario.fixture();
        let servers = fixture.start_servers().await;
        let mut primary = connect(&servers[0]).await;
        let expect = &fixture.expect;

        for file in fixture.release.all_files() {
            for segment in &file.segments {
                let mid = &segment.message_id;
                let named = expect.missing_articles.contains(mid)
                    || expect.corrupt_articles.contains(mid)
                    || expect.transient_articles.contains(mid)
                    || expect.unestablished_articles.contains(mid)
                    || expect.files_absent_from_nzb.contains(&segment.filename);
                if named {
                    continue;
                }
                assert_eq!(
                    fetch(&mut primary, mid).await.as_deref(),
                    Some(segment.body.as_slice()),
                    "{scenario:?}: {mid}"
                );
            }
        }

        for mid in &expect.missing_articles {
            for server in &servers {
                let mut connection = connect(server).await;
                assert_eq!(
                    fetch(&mut connection, mid).await,
                    None,
                    "{scenario:?}: {mid}"
                );
            }
        }
    }
}

#[tokio::test]
async fn primary_refusal_is_served_by_the_second_server() {
    let fixture = DamageScenario::MissingOnPrimaryOnly.fixture();
    let servers = fixture.start_servers().await;
    let mut a = connect(&servers[0]).await;
    let mut b = connect(&servers[1]).await;
    for mid in &fixture.expect.transient_articles {
        let segment = fixture.release.segment(mid).unwrap();
        assert_eq!(fetch(&mut a, mid).await, None);
        assert_eq!(
            fetch(&mut b, mid).await.as_deref(),
            Some(segment.body.as_slice())
        );
    }
}

#[tokio::test]
async fn busy_provider_refuses_fast_retries_then_serves() {
    let fixture = DamageScenario::BusyProvider.fixture();
    let started = Instant::now();
    let servers = fixture.start_servers().await;
    let mid = fixture.expect.transient_articles.iter().next().unwrap();
    let segment = fixture.release.segment(mid).unwrap();

    for server in &servers {
        let mut connection = connect(server).await;
        assert_eq!(fetch(&mut connection, mid).await, None);
        assert_eq!(fetch(&mut connection, mid).await, None);
    }
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "the refused retries must land inside the 350 ms window"
    );

    tokio::time::sleep(BUSY_PROVIDER_DELAY.saturating_sub(started.elapsed())).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut connection = connect(&servers[0]).await;
    assert_eq!(
        fetch(&mut connection, mid).await.as_deref(),
        Some(segment.body.as_slice())
    );
}

#[tokio::test]
async fn last_server_never_answers_the_unestablished_article() {
    let fixture = DamageScenario::TransportTimeoutOnLastServer.fixture();
    let servers = fixture.start_servers().await;
    let mid = fixture.expect.unestablished_articles.iter().next().unwrap();

    let mut a = connect(&servers[0]).await;
    assert_eq!(fetch(&mut a, mid).await, None);

    let mut b = connect(&servers[1]).await;
    let answer = tokio::time::timeout(Duration::from_millis(300), b.fetch_article(mid)).await;
    assert!(answer.is_err(), "the last server must time out, not answer");
}

#[tokio::test]
async fn corrupt_article_decodes_cleanly_but_differs_in_one_slice() {
    let fixture = DamageScenario::CorruptBody.fixture();
    let servers = fixture.start_servers().await;
    let mid = fixture.expect.corrupt_articles.iter().next().unwrap();
    let segment = fixture.release.segment(mid).unwrap();

    for server in &servers {
        let mut connection = connect(server).await;
        let body = fetch(&mut connection, mid).await.expect("served");
        assert_eq!(body.len(), segment.body.len());
        let diffs: Vec<usize> = (0..body.len())
            .filter(|&i| body[i] != segment.body[i])
            .collect();
        assert_eq!(diffs.len(), 1, "exactly one wrong byte");
        let byte = segment.offset + diffs[0] as u64;
        let base = fixture
            .release
            .par2
            .first_global_slice(&segment.filename)
            .unwrap();
        let slice = base + (byte / fixture.release.par2.slice_size()) as u32;
        assert_eq!(fixture.expect.damaged_slices, BTreeSet::from([slice]));
    }
}
