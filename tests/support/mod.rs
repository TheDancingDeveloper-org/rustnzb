#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crc32fast::Hasher;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::{AppConfig, ServerConfig};
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, QueueManager};
use rustnzb::server::build_router;
use tempfile::TempDir;
use tokio::task::JoinHandle;

const SAMPLE_NZB_PATH: &str = "e2e/fixtures/sample.nzb";
const YENC_LINE_WIDTH: usize = 128;

pub struct TestApp {
    pub base_url: String,
    pub complete_dir: PathBuf,
    _tmp_dir: TempDir,
    handle: JoinHandle<()>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub async fn start_test_server(server_configs: Vec<ServerConfig>) -> TestApp {
    let base = AppConfig::default();
    let config = AppConfig {
        general: nzb_web::nzb_core::config::GeneralConfig {
            max_active_downloads: 1,
            article_timeout_secs: 10,
            ..base.general
        },
        servers: server_configs,
        categories: base.categories,
        otel: base.otel,
        rss_feeds: base.rss_feeds,
        dav: base.dav,
    };

    let db = Database::open_memory().expect("Failed to create in-memory database");
    let tmp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let incomplete_dir = tmp_dir.path().join("incomplete");
    let complete_dir = tmp_dir.path().join("complete");
    std::fs::create_dir_all(&incomplete_dir).expect("Failed to create incomplete dir");
    std::fs::create_dir_all(&complete_dir).expect("Failed to create complete dir");

    let log_buffer = nzb_web::LogBuffer::new();
    let qm = QueueManager::new(
        config.servers.clone(),
        db,
        incomplete_dir,
        complete_dir.clone(),
        log_buffer.clone(),
        config.general.max_active_downloads,
        config.categories.clone(),
        config.general.min_free_space_bytes,
        config.general.speed_limit_bps,
        false,
        config.general.abort_hopeless,
        config.general.early_failure_check,
        config.general.required_completion_pct,
        config.general.article_timeout_secs,
    );
    let token_store = Arc::new(TokenStore::new());
    let credential_store = Arc::new(CredentialStore::new(tmp_dir.path().to_path_buf()));
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        PathBuf::from("config.toml"),
        qm,
        log_buffer,
        token_store,
        credential_store,
    ));

    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind random port");
    let addr = listener.local_addr().expect("Failed to get local addr");
    let base_url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    TestApp {
        base_url,
        complete_dir,
        _tmp_dir: tmp_dir,
        handle,
    }
}

pub fn sample_nzb_bytes() -> Vec<u8> {
    std::fs::read(SAMPLE_NZB_PATH).expect("Failed to read sample NZB fixture")
}

pub fn sample_nzb_variant_bytes() -> Vec<u8> {
    let xml = String::from_utf8(sample_nzb_bytes()).expect("sample fixture must be UTF-8 XML");
    xml.replace("Sample Test File", "Sample Alt File")
        .replace("sample.bin", "sample-alt.bin")
        .replace(
            "sample-article-001@rustnzb.test",
            "sample-article-002@rustnzb.test",
        )
        .into_bytes()
}

#[derive(Default)]
pub struct NzbFixture<'a> {
    files: Vec<NzbFixtureFile<'a>>,
}

struct NzbFixtureFile<'a> {
    filename: String,
    segments: Vec<(&'a str, &'a [u8])>,
}

pub struct BuiltFixture<'a> {
    pub xml: Vec<u8>,
    pub articles: Vec<(&'a str, &'a [u8], String)>,
}

impl<'a> NzbFixture<'a> {
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

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
            let total_parts = file.segments.len();
            let _ = writeln!(
                xml,
                r#"  <file poster="test@test" date="0" subject='"{fname}" yEnc (1/{total})'>"#,
                fname = file.filename,
                total = total_parts
            );
            xml.push_str("    <groups>\n      <group>alt.binaries.test</group>\n    </groups>\n");
            xml.push_str("    <segments>\n");
            for (index, (message_id, body)) in file.segments.iter().enumerate() {
                let _ = writeln!(
                    xml,
                    r#"      <segment number="{number}" bytes="{bytes}">{message_id}</segment>"#,
                    number = index + 1,
                    bytes = body.len(),
                    message_id = message_id
                );
            }
            xml.push_str("    </segments>\n");
            xml.push_str("  </file>\n");
        }

        xml.push_str("</nzb>\n");

        let mut articles = Vec::new();
        for file in &self.files {
            for (message_id, body) in &file.segments {
                articles.push((*message_id, *body, file.filename.clone()));
            }
        }

        BuiltFixture {
            xml: xml.into_bytes(),
            articles,
        }
    }
}

impl BuiltFixture<'_> {
    pub fn encoded_articles(&self) -> HashMap<String, Vec<u8>> {
        self.articles
            .iter()
            .map(|(message_id, body, filename)| {
                (
                    (*message_id).to_string(),
                    encode_yenc_article(body, filename, 1, 1, 0, body.len() as u64),
                )
            })
            .collect()
    }
}

fn encode_yenc_article(
    raw: &[u8],
    filename: &str,
    part: u32,
    total_parts: u32,
    file_offset: u64,
    total_file_size: u64,
) -> Vec<u8> {
    let mut hasher = Hasher::new();
    hasher.update(raw);
    let crc = hasher.finalize();

    let mut out = Vec::with_capacity(raw.len() * 11 / 10 + 256);

    if total_parts > 1 {
        out.extend_from_slice(
            format!(
                "=ybegin part={part} line={YENC_LINE_WIDTH} size={total_file_size} name={filename}\r\n"
            )
            .as_bytes(),
        );
        let begin = file_offset + 1;
        let end = file_offset + raw.len() as u64;
        out.extend_from_slice(format!("=ypart begin={begin} end={end}\r\n").as_bytes());
    } else {
        out.extend_from_slice(
            format!("=ybegin line={YENC_LINE_WIDTH} size={total_file_size} name={filename}\r\n")
                .as_bytes(),
        );
    }

    let mut line_pos = 0usize;
    for &byte in raw {
        let encoded = byte.wrapping_add(42);
        let escape = matches!(encoded, 0x00 | 0x0A | 0x0D | 0x3D)
            || (line_pos == 0 && matches!(encoded, 0x09 | 0x20 | 0x2E));

        if escape {
            out.push(b'=');
            out.push(encoded.wrapping_add(64));
            line_pos += 2;
        } else {
            out.push(encoded);
            line_pos += 1;
        }

        if line_pos >= YENC_LINE_WIDTH {
            out.extend_from_slice(b"\r\n");
            line_pos = 0;
        }
    }

    if line_pos > 0 {
        out.extend_from_slice(b"\r\n");
    }

    if total_parts > 1 {
        out.extend_from_slice(format!("=yend size={} pcrc32={crc:08X}\r\n", raw.len()).as_bytes());
    } else {
        out.extend_from_slice(format!("=yend size={} crc32={crc:08X}\r\n", raw.len()).as_bytes());
    }

    out
}
