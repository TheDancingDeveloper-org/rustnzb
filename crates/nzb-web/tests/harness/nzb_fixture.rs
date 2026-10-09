//! Tiny NZB XML fixture builder for tests. Produces NZB documents the
//! production `nzb_parser` accepts and that line up 1:1 with `MockConfig`'s
//! `articles` map (so you can submit the NZB and the mock serves the bodies).
//!
//! ```ignore
//! use harness::nzb_fixture::NzbFixture;
//! let fix = NzbFixture::new("test-job")
//!     .add_file("hello.txt", &[
//!         ("msg-1", b"hello world"),
//!     ])
//!     .build();
//! // fix.xml — the NZB bytes to submit
//! // fix.articles — the (msg_id, raw_body, filename) triples to feed
//! //                into harness::yenc_articles for the mock config
//! ```

use std::collections::HashMap;
use std::fmt::Write;

use nzb_nntp::testutil::MockConfig;

#[derive(Default)]
pub struct NzbFixture<'a> {
    name: String,
    files: Vec<NzbFixtureFile<'a>>,
}

struct NzbFixtureFile<'a> {
    filename: String,
    segments: Vec<(&'a str, &'a [u8])>, // (message_id, raw body)
}

/// Output of `NzbFixture::build()`.
pub struct BuiltFixture<'a> {
    /// NZB XML bytes ready for `submit_nzb_xml`.
    pub xml: Vec<u8>,
    /// `(message_id, raw_body, filename)` tuples ready for
    /// `harness::yenc_articles` to encode and feed into the mock.
    pub articles: Vec<(&'a str, &'a [u8], String)>,
}

/// Owned, reusable fixture cases for tests that need to hand the same input
/// to several providers or restart a queue manager. All bytes are generated
/// from literals, so the catalog never reads the network or wall clock.
#[derive(Clone, Debug)]
pub struct FixtureCase {
    pub name: String,
    pub xml: Vec<u8>,
    pub articles: HashMap<String, Vec<u8>>,
}

impl FixtureCase {
    pub fn mock_config(&self) -> MockConfig {
        MockConfig {
            articles: self.articles.clone(),
            ..MockConfig::default()
        }
    }
}

/// Stable fixture catalog shared by harness profiles and contract tests.
pub struct FixtureCatalog;

impl FixtureCatalog {
    fn from_fixture(fixture: BuiltFixture<'_>, name: &str) -> FixtureCase {
        let triples: Vec<(&str, &[u8], &str)> = fixture
            .articles
            .iter()
            .map(|(id, body, filename)| (*id, *body, filename.as_str()))
            .collect();
        let articles = super::yenc_articles(&triples);
        FixtureCase {
            name: name.into(),
            xml: fixture.xml,
            articles,
        }
    }

    pub fn single() -> FixtureCase {
        let fixture = NzbFixture::new("catalog-single")
            .add_file("catalog.txt", &[("catalog-single-1@test", b"catalog body")])
            .build();
        Self::from_fixture(fixture, "catalog-single")
    }

    pub fn multi_segment() -> FixtureCase {
        let fixture = NzbFixture::new("catalog-multi")
            .add_file(
                "catalog.bin",
                &[
                    ("catalog-multi-1@test", b"first"),
                    ("catalog-multi-2@test", b"second"),
                    ("catalog-multi-3@test", b"third"),
                ],
            )
            .build();
        Self::from_fixture(fixture, "catalog-multi")
    }
}

impl<'a> NzbFixture<'a> {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            files: Vec::new(),
        }
    }

    /// Add a file with one or more segments. The `segments` slice is
    /// `(message_id, body_bytes)` pairs.
    pub fn add_file(mut self, filename: &str, segments: &[(&'a str, &'a [u8])]) -> Self {
        assert!(!segments.is_empty(), "file must have at least one segment");
        self.files.push(NzbFixtureFile {
            filename: filename.to_string(),
            segments: segments.to_vec(),
        });
        self
    }

    pub fn build(self) -> BuiltFixture<'a> {
        let mut xml = String::new();
        writeln!(xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#).unwrap();
        writeln!(xml, r#"<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">"#).unwrap();
        for file in &self.files {
            // Subject embeds the quoted filename so the parser's
            // `extract_filename` finds it. Total parts = number of segments.
            let total_parts = file.segments.len();
            let _ = writeln!(
                xml,
                r#"  <file poster="test@test" date="0" subject='"{fname}" yEnc (1/{total})'>"#,
                fname = file.filename,
                total = total_parts
            );
            xml.push_str("    <groups>\n      <group>alt.binaries.test</group>\n    </groups>\n");
            xml.push_str("    <segments>\n");
            for (i, (msg_id, body)) in file.segments.iter().enumerate() {
                let _ = writeln!(
                    xml,
                    r#"      <segment number="{n}" bytes="{bytes}">{mid}</segment>"#,
                    n = i + 1,
                    bytes = body.len(),
                    mid = msg_id
                );
            }
            xml.push_str("    </segments>\n");
            xml.push_str("  </file>\n");
        }
        xml.push_str("</nzb>\n");

        let mut articles = Vec::new();
        for file in &self.files {
            for (msg_id, body) in &file.segments {
                articles.push((*msg_id, *body, file.filename.clone()));
            }
        }

        let _ = self.name; // currently only used for naming the job at submit time
        BuiltFixture {
            xml: xml.into_bytes(),
            articles,
        }
    }
}
