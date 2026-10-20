pub mod damage;
#[cfg(unix)]
pub mod fake_unrar;
pub mod par2;

use std::collections::HashMap;

use nzb_nntp::config::ServerConfig;
use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config};

pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:19119";
pub const SAMPLE_GROUP: &str = "alt.binaries.test";
pub const SAMPLE_SINGLE_FILENAME: &str = "sample.bin";
pub const SAMPLE_SINGLE_MESSAGE_ID: &str = "sample-article-001@rustnzb.test";
pub const SAMPLE_MULTI_FILENAME: &str = "sample-multi.bin";
pub const SAMPLE_MULTI_MESSAGE_IDS: [&str; 3] = [
    "sample-multi-001@rustnzb.test",
    "sample-multi-002@rustnzb.test",
    "sample-multi-003@rustnzb.test",
];

#[derive(Clone, Debug)]
pub struct FixtureSegment {
    pub message_id: String,
    pub filename: String,
    pub body: Vec<u8>,
    pub segment_number: u32,
    pub total_segments: u32,
    pub total_file_size: u64,
    pub offset: u64,
}

#[derive(Clone, Debug)]
pub struct FixtureFile {
    pub filename: String,
    pub segments: Vec<FixtureSegment>,
}

pub fn fixture_files() -> Vec<FixtureFile> {
    let single_body = deterministic_body(102_400, 0x11);
    let single_file = FixtureFile {
        filename: SAMPLE_SINGLE_FILENAME.to_string(),
        segments: vec![FixtureSegment {
            message_id: SAMPLE_SINGLE_MESSAGE_ID.to_string(),
            filename: SAMPLE_SINGLE_FILENAME.to_string(),
            body: single_body.clone(),
            segment_number: 1,
            total_segments: 1,
            total_file_size: single_body.len() as u64,
            offset: 0,
        }],
    };

    let multi_bodies = [
        deterministic_body(4096, 0x21),
        deterministic_body(4096, 0x43),
        deterministic_body(4096, 0x65),
    ];
    let multi_total = multi_bodies
        .iter()
        .map(|body| body.len() as u64)
        .sum::<u64>();
    let mut offset = 0u64;
    let mut multi_segments = Vec::new();
    for (idx, (message_id, body)) in SAMPLE_MULTI_MESSAGE_IDS
        .iter()
        .zip(multi_bodies)
        .enumerate()
    {
        let len = body.len() as u64;
        multi_segments.push(FixtureSegment {
            message_id: (*message_id).to_string(),
            filename: SAMPLE_MULTI_FILENAME.to_string(),
            body,
            segment_number: (idx + 1) as u32,
            total_segments: SAMPLE_MULTI_MESSAGE_IDS.len() as u32,
            total_file_size: multi_total,
            offset,
        });
        offset += len;
    }

    vec![
        single_file,
        FixtureFile {
            filename: SAMPLE_MULTI_FILENAME.to_string(),
            segments: multi_segments,
        },
    ]
}

pub fn fixture_segments() -> Vec<FixtureSegment> {
    fixture_files()
        .into_iter()
        .flat_map(|file| file.segments)
        .collect()
}

pub fn sample_mock_config() -> MockConfig {
    let segments = fixture_segments();
    let mut articles = HashMap::new();
    for segment in &segments {
        let (encoded, _crc) = yenc_simd::encode_article(
            &segment.body,
            &segment.filename,
            segment.segment_number,
            segment.total_segments,
            segment.offset,
            segment.total_file_size,
        );
        articles.insert(segment.message_id.clone(), encoded);
    }

    MockConfig {
        articles,
        groups: HashMap::from([(
            SAMPLE_GROUP.to_string(),
            (segments.len() as u64, 1, segments.len() as u64),
        )]),
        ..Default::default()
    }
}

pub async fn start_sample_server() -> MockNntpServer {
    MockNntpServer::start(sample_mock_config()).await
}

pub async fn start_sample_server_on(bind_addr: &str) -> MockNntpServer {
    MockNntpServer::start_on(bind_addr, sample_mock_config()).await
}

pub fn server_config(port: u16) -> ServerConfig {
    test_config(port)
}

fn deterministic_body(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|idx| seed.wrapping_add((idx % 251) as u8))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nzb_nntp::NntpConnection;

    #[test]
    fn fixture_catalog_has_stable_single_and_multi_part_files() {
        let files = fixture_files();

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].filename, SAMPLE_SINGLE_FILENAME);
        assert_eq!(files[0].segments.len(), 1);
        assert_eq!(files[1].filename, SAMPLE_MULTI_FILENAME);
        assert_eq!(files[1].segments.len(), SAMPLE_MULTI_MESSAGE_IDS.len());
    }

    #[test]
    fn multipart_offsets_are_contiguous_and_cover_the_file() {
        let file = fixture_files().remove(1);
        let mut expected_offset = 0;

        for (index, segment) in file.segments.iter().enumerate() {
            assert_eq!(segment.segment_number, (index + 1) as u32);
            assert_eq!(segment.total_segments, file.segments.len() as u32);
            assert_eq!(segment.offset, expected_offset);
            assert_eq!(segment.filename, file.filename);
            expected_offset += segment.body.len() as u64;
        }

        assert_eq!(expected_offset, file.segments[0].total_file_size);
    }

    #[test]
    fn fixtures_are_reproducible_and_have_unique_message_ids() {
        let first = fixture_segments();
        let second = fixture_segments();
        let ids = first
            .iter()
            .map(|segment| segment.message_id.as_str())
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(first.len(), second.len());
        assert_eq!(ids.len(), first.len());
        for (left, right) in first.iter().zip(second.iter()) {
            assert_eq!(left.message_id, right.message_id);
            assert_eq!(left.body, right.body);
        }
    }

    #[test]
    fn sample_config_contains_decodable_yenc_for_every_segment() {
        let segments = fixture_segments();
        let config = sample_mock_config();

        assert_eq!(config.articles.len(), segments.len());
        assert_eq!(
            config.groups.get(SAMPLE_GROUP),
            Some(&(segments.len() as u64, 1, segments.len() as u64))
        );

        for segment in segments {
            let encoded = config
                .articles
                .get(&segment.message_id)
                .expect("fixture article should be present");
            let decoded = yenc_simd::decode_yenc(encoded).expect("fixture yEnc should decode");
            assert_eq!(decoded.data, segment.body);
            assert_eq!(decoded.filename.as_deref(), Some(segment.filename.as_str()));
        }
    }

    #[tokio::test]
    async fn sample_server_round_trips_every_fixture_article() {
        let server = start_sample_server().await;
        let config = server_config(server.port());
        let mut connection = NntpConnection::new(config.id.clone());
        connection.connect(&config).await.unwrap();

        for segment in fixture_segments() {
            let response = connection.fetch_article(&segment.message_id).await.unwrap();
            let data = response
                .data
                .expect("article response should include a body");
            let decoded = yenc_simd::decode_yenc(&data).unwrap();
            assert_eq!(decoded.data, segment.body);
        }

        connection.quit().await.unwrap();
    }
}
