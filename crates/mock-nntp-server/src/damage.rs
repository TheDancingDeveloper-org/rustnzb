//! Deterministic PAR2 damage fixtures served through the mock NNTP server.
//!
//! A [`DamageRelease`] is a small release generated from a seed: two data
//! files split into yEnc articles, a real PAR2 index, and recovery volumes
//! with known exponents. Nothing binary is committed; every byte comes from
//! the seed.
//!
//! A [`DamageScenario`] applies one provider pathology to that release and
//! yields a [`ScenarioFixture`]. It holds two servers to start (primary `A`,
//! then `B`), the NZB to submit, what a downloader would leave on disk, and an
//! [`Expectation`] describing the end state the client should reach.
//!
//! Layout of the standard release (slice size 4096, articles of 10 000 bytes):
//!
//! | file              | size    | articles | slices |
//! |-------------------|---------|----------|--------|
//! | `<name>.mkv`      | 120 000 | 12       | 30     |
//! | `<name>.nfo`      | 2 500   | 1        | 1      |
//! | `<name>.par2`     | index   |          |        |
//! | `<name>.volNN+MM.par2` | 1, 2, 4, 8 recovery slices (15 total) | | |
//!
//! Articles deliberately straddle slice boundaries, so one missing article
//! damages two or three slices.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use nzb_nntp::testutil::{ArticleResponseSequences, MockConfig, MockNntpServer};

use crate::par2::Par2Fixture;
use crate::{FixtureSegment, SAMPLE_GROUP};

/// PAR2 slice size of the standard release.
pub const DAMAGE_SLICE_SIZE: u64 = 4096;
/// Payload bytes per article. Not a multiple of the slice size on purpose.
pub const DAMAGE_ARTICLE_SIZE: usize = 10_000;
/// Recovery slices per volume, in volume order. Exponents run 0..15.
pub const RECOVERY_VOLUME_BLOCKS: [u32; 4] = [1, 2, 4, 8];
/// How many ARTICLE attempts the busy provider answers 430 before serving.
/// The fixture's two fast retries both fall inside this, and the next attempt
/// is served. Built on the published `article_response_sequences`, which only
/// applies to ARTICLE — the command a downloader issues.
pub const BUSY_PROVIDER_REFUSALS: usize = 2;

const MKV_SIZE: usize = 120_000;
const NFO_SIZE: usize = 2_500;

/// One file of the release: its payload and the articles that carry it.
#[derive(Clone, Debug)]
pub struct ReleaseFile {
    pub filename: String,
    pub payload: Vec<u8>,
    pub segments: Vec<FixtureSegment>,
}

/// A recovery volume and the exponents of the slices it carries.
#[derive(Clone, Debug)]
pub struct RecoveryVolume {
    pub file: ReleaseFile,
    pub exponents: Vec<u32>,
}

/// A release with a real PAR2 set, generated from a seed.
#[derive(Clone, Debug)]
pub struct DamageRelease {
    pub name: String,
    pub data_files: Vec<ReleaseFile>,
    pub par2_index: ReleaseFile,
    pub volumes: Vec<RecoveryVolume>,
    pub par2: Par2Fixture,
}

impl DamageRelease {
    /// Generate the standard release. The same `name` and `seed` always give
    /// the same bytes.
    pub fn generate(name: &str, seed: u64) -> Self {
        Self::generate_with_volumes(name, seed, &RECOVERY_VOLUME_BLOCKS)
    }

    /// Generate the standard data files and index with recovery volumes of
    /// the given block counts, in order. An empty list gives an index-only
    /// release with no recovery slices.
    pub fn generate_with_volumes(name: &str, seed: u64, volume_blocks: &[u32]) -> Self {
        let mut rng = SplitMix64(seed);
        let mkv_name = format!("{name}.mkv");
        let nfo_name = format!("{name}.nfo");
        let mkv = rng.bytes(MKV_SIZE);
        let nfo = rng.bytes(NFO_SIZE);

        let par2 = Par2Fixture::new()
            .with_slice_size(DAMAGE_SLICE_SIZE)
            .add_file(&mkv_name, &mkv)
            .add_file(&nfo_name, &nfo);

        let mut volumes = Vec::new();
        let mut next_exponent = 0u32;
        for &blocks in volume_blocks {
            let exponents: Vec<u32> = (next_exponent..next_exponent + blocks).collect();
            let filename = format!("{name}.vol{next_exponent:02}+{blocks:02}.par2");
            let payload = par2.volume_bytes(&exponents);
            volumes.push(RecoveryVolume {
                file: ReleaseFile::split(filename, payload),
                exponents,
            });
            next_exponent += blocks;
        }

        Self {
            name: name.to_string(),
            data_files: vec![
                ReleaseFile::split(mkv_name, mkv),
                ReleaseFile::split(nfo_name, nfo),
            ],
            par2_index: ReleaseFile::split(format!("{name}.par2"), par2.index_bytes()),
            volumes,
            par2,
        }
    }

    /// Recovery slices across every volume.
    pub fn recovery_blocks(&self) -> u32 {
        self.volumes.iter().map(|v| v.exponents.len() as u32).sum()
    }

    /// Every file of the release: data files, the index, then the volumes.
    pub fn all_files(&self) -> impl Iterator<Item = &ReleaseFile> {
        self.data_files
            .iter()
            .chain(std::iter::once(&self.par2_index))
            .chain(self.volumes.iter().map(|v| &v.file))
    }

    pub fn data_file(&self, filename: &str) -> Option<&ReleaseFile> {
        self.data_files.iter().find(|f| f.filename == filename)
    }

    /// The article with `message_id`, in any file.
    pub fn segment(&self, message_id: &str) -> Option<&FixtureSegment> {
        self.all_files()
            .flat_map(|f| f.segments.iter())
            .find(|s| s.message_id == message_id)
    }

    /// Message-id of data file `file` (index into `data_files`), 1-based
    /// segment `number`.
    pub fn data_message_id(&self, file: usize, number: u32) -> String {
        self.data_files[file].segments[number as usize - 1]
            .message_id
            .clone()
    }

    /// Global PAR2 slice indices an article's bytes fall in. Empty for
    /// articles of PAR2 files, which are not protected by the set.
    pub fn slices_touched(&self, segment: &FixtureSegment) -> BTreeSet<u32> {
        let Some(base) = self.par2.first_global_slice(&segment.filename) else {
            return BTreeSet::new();
        };
        if segment.body.is_empty() {
            return BTreeSet::new();
        }
        let first = segment.offset / DAMAGE_SLICE_SIZE;
        let last = (segment.offset + segment.body.len() as u64 - 1) / DAMAGE_SLICE_SIZE;
        (first..=last).map(|s| base + s as u32).collect()
    }

    /// Every slice of data file `filename`.
    pub fn file_slices(&self, filename: &str) -> BTreeSet<u32> {
        let Some(base) = self.par2.first_global_slice(filename) else {
            return BTreeSet::new();
        };
        let size = self
            .data_file(filename)
            .map_or(0, |f| f.payload.len() as u64);
        let count = size.div_ceil(DAMAGE_SLICE_SIZE) as u32;
        (base..base + count).collect()
    }

    /// yEnc-encoded article bytes keyed by message-id, for `MockConfig`.
    pub fn encoded_articles(&self) -> HashMap<String, Vec<u8>> {
        self.all_files()
            .flat_map(|f| f.segments.iter())
            .map(|s| (s.message_id.clone(), encode(s, &s.body)))
            .collect()
    }

    /// An NZB for the release, leaving out the files named in `omit`.
    pub fn nzb_xml(&self, omit: &[String]) -> Vec<u8> {
        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        xml.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");
        for file in self.all_files().filter(|f| !omit.contains(&f.filename)) {
            let _ = writeln!(
                xml,
                "  <file poster=\"fixture@rustnzb.test\" date=\"0\" subject='{name} \"{fname}\" yEnc (1/{total})'>",
                name = self.name,
                fname = file.filename,
                total = file.segments.len()
            );
            let _ = writeln!(
                xml,
                "    <groups>\n      <group>{SAMPLE_GROUP}</group>\n    </groups>"
            );
            xml.push_str("    <segments>\n");
            for segment in &file.segments {
                let _ = writeln!(
                    xml,
                    "      <segment number=\"{n}\" bytes=\"{bytes}\">{mid}</segment>",
                    n = segment.segment_number,
                    bytes = segment.body.len(),
                    mid = segment.message_id
                );
            }
            xml.push_str("    </segments>\n  </file>\n");
        }
        xml.push_str("</nzb>\n");
        xml.into_bytes()
    }

    /// Write the PAR2 index and every recovery volume into `dir`.
    pub fn write_par2(&self, dir: &Path) {
        std::fs::write(
            dir.join(&self.par2_index.filename),
            &self.par2_index.payload,
        )
        .unwrap();
        for volume in &self.volumes {
            std::fs::write(dir.join(&volume.file.filename), &volume.file.payload).unwrap();
        }
    }
}

impl ReleaseFile {
    fn split(filename: String, payload: Vec<u8>) -> Self {
        let total = payload.len().div_ceil(DAMAGE_ARTICLE_SIZE).max(1) as u32;
        let segments = payload
            .chunks(DAMAGE_ARTICLE_SIZE)
            .enumerate()
            .map(|(idx, body)| FixtureSegment {
                message_id: format!("{filename}.{:03}@damage.rustnzb.test", idx + 1),
                filename: filename.clone(),
                body: body.to_vec(),
                segment_number: idx as u32 + 1,
                total_segments: total,
                total_file_size: payload.len() as u64,
                offset: (idx * DAMAGE_ARTICLE_SIZE) as u64,
            })
            .collect();
        Self {
            filename,
            payload,
            segments,
        }
    }
}

/// One provider pathology applied to the standard release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DamageScenario {
    /// Two articles answer 430 on every server. Repairable.
    MissingEverywhere,
    /// Two articles answer 430 on server A and are served by server B.
    MissingOnPrimaryOnly,
    /// One article answers 430 on every server for the first
    /// [`BUSY_PROVIDER_REFUSALS`] ARTICLE attempts, then is served. The two
    /// fast retries both fail and a later attempt succeeds. Must not be
    /// counted as damage.
    BusyProvider,
    /// One article answers 430 on server A, and server B, the last server,
    /// stops answering ARTICLE altogether. The outcome is unestablished, not
    /// confirmed missing.
    TransportTimeoutOnLastServer,
    /// One article is served with a wrong body that still decodes cleanly
    /// (the yEnc CRC is computed over the wrong bytes). Only PAR2 catches it.
    CorruptBody,
    /// Seven articles answer 430 everywhere, damaging more slices than the
    /// release carries recovery slices for. Hopeless.
    DamageExceedsRecovery,
    /// The PAR2 set names the `.nfo` file but the NZB does not carry it.
    PartialPost,
}

impl DamageScenario {
    pub const ALL: [DamageScenario; 7] = [
        DamageScenario::MissingEverywhere,
        DamageScenario::MissingOnPrimaryOnly,
        DamageScenario::BusyProvider,
        DamageScenario::TransportTimeoutOnLastServer,
        DamageScenario::CorruptBody,
        DamageScenario::DamageExceedsRecovery,
        DamageScenario::PartialPost,
    ];

    /// A stable, filesystem-safe name.
    pub fn slug(self) -> &'static str {
        match self {
            DamageScenario::MissingEverywhere => "missing-everywhere",
            DamageScenario::MissingOnPrimaryOnly => "missing-on-primary-only",
            DamageScenario::BusyProvider => "busy-provider",
            DamageScenario::TransportTimeoutOnLastServer => "transport-timeout-last-server",
            DamageScenario::CorruptBody => "corrupt-body",
            DamageScenario::DamageExceedsRecovery => "damage-exceeds-recovery",
            DamageScenario::PartialPost => "partial-post",
        }
    }

    /// Build the fixture. Each scenario has its own release name and seed, so
    /// message-ids never collide between scenarios.
    pub fn fixture(self) -> ScenarioFixture {
        let seed = 0x5EED_0000 + Self::ALL.iter().position(|s| *s == self).unwrap() as u64;
        let release = DamageRelease::generate(&format!("damage-{}", self.slug()), seed);
        let mkv = |n: u32| release.data_message_id(0, n);
        let nfo_name = release.data_files[1].filename.clone();

        let mut a = ServerPlan::default();
        let mut b = ServerPlan::default();
        let mut expect = Expectation {
            recovery_blocks: release.recovery_blocks(),
            ..Expectation::default()
        };

        match self {
            DamageScenario::MissingEverywhere => {
                for mid in [mkv(3), mkv(8)] {
                    a.removed.insert(mid.clone());
                    b.removed.insert(mid.clone());
                    expect.missing_articles.insert(mid);
                }
            }
            DamageScenario::MissingOnPrimaryOnly => {
                for mid in [mkv(2), mkv(9)] {
                    a.removed.insert(mid.clone());
                    expect.transient_articles.insert(mid);
                }
            }
            DamageScenario::BusyProvider => {
                let mid = mkv(6);
                a.refuse_first_n.insert(mid.clone(), BUSY_PROVIDER_REFUSALS);
                b.refuse_first_n.insert(mid.clone(), BUSY_PROVIDER_REFUSALS);
                expect.transient_articles.insert(mid);
            }
            DamageScenario::TransportTimeoutOnLastServer => {
                let mid = mkv(4);
                a.removed.insert(mid.clone());
                b.hang_on_article = true;
                expect.unestablished_articles.insert(mid);
            }
            DamageScenario::CorruptBody => {
                let mid = mkv(5);
                let segment = release.segment(&mid).unwrap();
                let mut body = segment.body.clone();
                body[CORRUPT_BYTE_IN_ARTICLE] ^= 0xFF;
                let encoded = encode(segment, &body);
                a.replaced.insert(mid.clone(), encoded.clone());
                b.replaced.insert(mid.clone(), encoded);
                expect.corrupt_articles.insert(mid);
            }
            DamageScenario::DamageExceedsRecovery => {
                for mid in (1..=7).map(mkv) {
                    a.removed.insert(mid.clone());
                    b.removed.insert(mid.clone());
                    expect.missing_articles.insert(mid);
                }
            }
            DamageScenario::PartialPost => {
                for segment in &release.data_files[1].segments {
                    a.removed.insert(segment.message_id.clone());
                    b.removed.insert(segment.message_id.clone());
                }
                expect.files_absent_from_nzb.push(nfo_name.clone());
            }
        }

        for mid in &expect.missing_articles {
            expect
                .damaged_slices
                .extend(release.slices_touched(release.segment(mid).unwrap()));
        }
        for mid in &expect.corrupt_articles {
            let segment = release.segment(mid).unwrap();
            let byte = segment.offset + CORRUPT_BYTE_IN_ARTICLE as u64;
            let base = release.par2.first_global_slice(&segment.filename).unwrap();
            expect
                .damaged_slices
                .insert(base + (byte / DAMAGE_SLICE_SIZE) as u32);
        }
        for filename in &expect.files_absent_from_nzb {
            expect.damaged_slices.extend(release.file_slices(filename));
        }
        for mid in &expect.unestablished_articles {
            expect
                .unknown_slices
                .extend(release.slices_touched(release.segment(mid).unwrap()));
        }
        let at_risk = expect.damaged_slices.len() + expect.unknown_slices.len();
        expect.repairable = at_risk as u32 <= expect.recovery_blocks;

        ScenarioFixture {
            scenario: self,
            release,
            servers: vec![a, b],
            expect,
        }
    }
}

/// Byte flipped inside the corrupt article's body.
const CORRUPT_BYTE_IN_ARTICLE: usize = 1234;

/// What the client should conclude once the job ends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expectation {
    /// Articles no server ever serves: confirmed missing.
    pub missing_articles: BTreeSet<String>,
    /// Articles served with a wrong body that decodes cleanly.
    pub corrupt_articles: BTreeSet<String>,
    /// Articles refused at first and served by a later server or attempt.
    /// Not damage.
    pub transient_articles: BTreeSet<String>,
    /// Articles the last server never answered (transport failure). Their
    /// state is unestablished, not confirmed missing.
    pub unestablished_articles: BTreeSet<String>,
    /// Files the PAR2 set names that the NZB does not carry.
    pub files_absent_from_nzb: Vec<String>,
    /// Global slice indices confirmed damaged at job end: touched by a
    /// missing or corrupt article, or part of a file absent from the NZB.
    pub damaged_slices: BTreeSet<u32>,
    /// Global slice indices whose state cannot be established, because an
    /// article covering them is unestablished.
    pub unknown_slices: BTreeSet<u32>,
    /// Recovery slices the release carries across every volume.
    pub recovery_blocks: u32,
    /// Whether the slices at risk (damaged plus unknown) fit within the
    /// recovery slices, so a repair from the full set succeeds.
    pub repairable: bool,
}

impl Expectation {
    /// The diagnosis a hopeless job records, or `None` when the damage is
    /// repairable.
    pub fn hopeless_diagnosis(&self) -> Option<String> {
        (!self.repairable).then(|| {
            format!(
                "{} slice(s) missing but the release only carries {} recovery slice(s)",
                self.damaged_slices.len(),
                self.recovery_blocks
            )
        })
    }
}

/// Per-server fault plan, kept as data so fresh `MockConfig`s (with fresh
/// shared state) can be built for every server start.
#[derive(Clone, Debug, Default)]
struct ServerPlan {
    removed: BTreeSet<String>,
    replaced: BTreeMap<String, Vec<u8>>,
    /// ARTICLE attempts to answer 430 before the article is served normally.
    refuse_first_n: HashMap<String, usize>,
    hang_on_article: bool,
}

/// A scenario applied to a release: servers, NZB, disk state, expectation.
#[derive(Clone, Debug)]
pub struct ScenarioFixture {
    pub scenario: DamageScenario,
    pub release: DamageRelease,
    servers: Vec<ServerPlan>,
    pub expect: Expectation,
}

impl ScenarioFixture {
    /// One `MockConfig` per server, primary first.
    pub fn server_configs(&self) -> Vec<MockConfig> {
        let articles = self.release.encoded_articles();
        self.servers
            .iter()
            .map(|plan| {
                let mut articles = articles.clone();
                for mid in &plan.removed {
                    articles.remove(mid);
                }
                for (mid, encoded) in &plan.replaced {
                    articles.insert(mid.clone(), encoded.clone());
                }
                let count = articles.len() as u64;
                let article_response_sequences = (!plan.refuse_first_n.is_empty()).then(|| {
                    let sequences = ArticleResponseSequences::default();
                    for (mid, n) in &plan.refuse_first_n {
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
                    article_response_sequences,
                    hang_after_command: plan.hang_on_article.then(|| "ARTICLE".to_string()),
                    ..MockConfig::default()
                }
            })
            .collect()
    }

    /// Start every server, primary first.
    pub async fn start_servers(&self) -> Vec<MockNntpServer> {
        let mut servers = Vec::new();
        for config in self.server_configs() {
            servers.push(MockNntpServer::start(config).await);
        }
        servers
    }

    /// The NZB to submit. Files absent from the post are left out.
    pub fn nzb_xml(&self) -> Vec<u8> {
        self.release.nzb_xml(&self.expect.files_absent_from_nzb)
    }

    /// Write what a downloader leaves in `dir` at job end: data files at full
    /// size with zeroes where no article landed (missing or unestablished),
    /// wrong bytes where the corrupt article landed, files absent from the
    /// NZB not written, plus the PAR2 index and every recovery volume.
    pub fn write_received(&self, dir: &Path) {
        for file in &self.release.data_files {
            if self.expect.files_absent_from_nzb.contains(&file.filename) {
                continue;
            }
            let mut bytes = file.payload.clone();
            for segment in &file.segments {
                let range = segment.offset as usize..segment.offset as usize + segment.body.len();
                let mid = &segment.message_id;
                if self.expect.missing_articles.contains(mid)
                    || self.expect.unestablished_articles.contains(mid)
                {
                    bytes[range].fill(0);
                } else if self.expect.corrupt_articles.contains(mid) {
                    bytes[segment.offset as usize + CORRUPT_BYTE_IN_ARTICLE] ^= 0xFF;
                }
            }
            std::fs::write(dir.join(&file.filename), bytes).unwrap();
        }
        self.release.write_par2(dir);
    }
}

fn encode(segment: &FixtureSegment, body: &[u8]) -> Vec<u8> {
    yenc_simd::encode_article(
        body,
        &segment.filename,
        segment.segment_number,
        segment.total_segments,
        segment.offset,
        segment.total_file_size,
    )
    .0
}

/// Small seeded generator so payloads need no RNG dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}
