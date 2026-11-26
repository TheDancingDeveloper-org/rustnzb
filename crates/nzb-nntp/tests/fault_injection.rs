//! Tests for the fault-injection primitives in `MockNntpServer`.
//!
//! Each test drives the mock with a raw TCP client and asserts the wire-level
//! behaviour, so the tests are independent of any higher-level client logic.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

use nzb_nntp::testutil::{AuthRateLimit, MockConfig, MockNntpServer};

async fn connect(server: &MockNntpServer) -> BufReader<TcpStream> {
    let stream = TcpStream::connect(server.addr).await.unwrap();
    BufReader::new(stream)
}

async fn read_line(reader: &mut BufReader<TcpStream>) -> String {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let _ = reader.read_line(&mut line).await;
    line
}

async fn auth_anon(reader: &mut BufReader<TcpStream>) {
    reader
        .get_mut()
        .write_all(b"AUTHINFO USER anyuser\r\n")
        .await
        .unwrap();
    let _ = read_line(reader).await; // 381
    reader
        .get_mut()
        .write_all(b"AUTHINFO PASS anypass\r\n")
        .await
        .unwrap();
    let _ = read_line(reader).await; // 281
}

// ---------------------------------------------------------------------------
// silent_close_after_bytes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn silent_close_truncates_writes_and_drops_socket() {
    // Welcome banner is "200 Mock NNTP Ready\r\n" = 21 bytes. Limit at 10
    // bytes — the welcome should be truncated and the socket should close
    // before the client can send anything.
    let server = MockNntpServer::start(MockConfig {
        silent_close_after_bytes: Some(10),
        ..Default::default()
    })
    .await;

    let mut reader = connect(&server).await;

    // Read everything until EOF.
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;

    assert!(
        buf.len() <= 10,
        "expected at most 10 bytes before silent close, got {}",
        buf.len()
    );
    // The truncated banner should still start with "200" since the welcome
    // is the first thing written.
    assert!(
        buf.starts_with(b"200"),
        "expected truncated banner to start with 200, got {:?}",
        String::from_utf8_lossy(&buf)
    );
}

#[tokio::test]
async fn silent_close_mid_session() {
    // Allow welcome (21 bytes) plus a partial response. The client should
    // get EOF mid-session, not a clean QUIT.
    let server = MockNntpServer::start(MockConfig {
        silent_close_after_bytes: Some(30),
        ..Default::default()
    })
    .await;

    let mut reader = connect(&server).await;
    let banner = read_line(&mut reader).await;
    assert!(banner.starts_with("200"), "unexpected banner: {banner:?}");

    // Send a command that would normally produce a long response.
    reader
        .get_mut()
        .write_all(b"AUTHINFO USER test\r\n")
        .await
        .unwrap();

    // Read until EOF.
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest).await;

    // Total bytes received (banner + rest) should not exceed the limit.
    let total = banner.len() + rest.len();
    assert!(
        total <= 30,
        "expected ≤30 bytes total, got {total}: banner={banner:?} rest={:?}",
        String::from_utf8_lossy(&rest)
    );
}

// ---------------------------------------------------------------------------
// hang_after_command
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hang_after_command_stops_responding() {
    let server = MockNntpServer::start(MockConfig {
        hang_after_command: Some("ARTICLE".into()),
        articles: {
            let mut m = HashMap::new();
            m.insert("msg-1".into(), b"hi".to_vec());
            m
        },
        ..Default::default()
    })
    .await;

    let mut reader = connect(&server).await;
    let _ = read_line(&mut reader).await; // banner

    // Auth — should still respond (hang only triggers on ARTICLE).
    auth_anon(&mut reader).await;

    // Now send ARTICLE. Server enters hang mode and emits nothing.
    reader
        .get_mut()
        .write_all(b"ARTICLE <msg-1>\r\n")
        .await
        .unwrap();

    // Reading should time out because the server never responds.
    let result = timeout(Duration::from_millis(500), read_line(&mut reader)).await;
    assert!(
        result.is_err() || result.as_ref().unwrap().is_empty(),
        "expected hang (timeout or empty), got: {result:?}"
    );
}

#[tokio::test]
async fn hang_does_not_affect_pre_hang_commands() {
    let server = MockNntpServer::start(MockConfig {
        hang_after_command: Some("ARTICLE".into()),
        ..Default::default()
    })
    .await;

    let mut reader = connect(&server).await;
    let banner = read_line(&mut reader).await;
    assert!(banner.starts_with("200"));

    // AUTHINFO is not the hang verb, should respond normally.
    reader
        .get_mut()
        .write_all(b"AUTHINFO USER x\r\n")
        .await
        .unwrap();
    let resp = read_line(&mut reader).await;
    assert!(resp.starts_with("381"), "expected 381, got {resp:?}");
}

// ---------------------------------------------------------------------------
// close_after_n_commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn close_after_n_commands_drops_socket() {
    let server = MockNntpServer::start(MockConfig {
        close_after_n_commands: Some(2),
        ..Default::default()
    })
    .await;

    let mut reader = connect(&server).await;
    let _ = read_line(&mut reader).await; // banner

    // Command 1: should succeed.
    reader
        .get_mut()
        .write_all(b"AUTHINFO USER x\r\n")
        .await
        .unwrap();
    let r1 = read_line(&mut reader).await;
    assert!(r1.starts_with("381"), "cmd 1: {r1:?}");

    // Command 2: should succeed, then server closes.
    reader
        .get_mut()
        .write_all(b"AUTHINFO PASS y\r\n")
        .await
        .unwrap();
    let r2 = read_line(&mut reader).await;
    assert!(r2.starts_with("281"), "cmd 2: {r2:?}");

    // Command 3: socket should be closed → read returns EOF.
    let _ = reader.get_mut().write_all(b"AUTHINFO USER z\r\n").await; // may or may not error depending on TCP state
    let r3 = read_line(&mut reader).await;
    assert!(r3.is_empty(), "expected EOF after 2 commands, got: {r3:?}");
}

// ---------------------------------------------------------------------------
// response_delay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn response_delay_introduces_observable_latency() {
    let delay = Duration::from_millis(150);
    let server = MockNntpServer::start(MockConfig {
        response_delay: Some(delay),
        ..Default::default()
    })
    .await;

    let mut reader = connect(&server).await;
    // Banner is the first delayed write.
    let start = Instant::now();
    let _ = read_line(&mut reader).await;
    let elapsed = start.elapsed();

    assert!(
        elapsed >= delay,
        "expected ≥{}ms latency, got {}ms",
        delay.as_millis(),
        elapsed.as_millis()
    );
}

// ---------------------------------------------------------------------------
// article_response_overrides
// ---------------------------------------------------------------------------

#[tokio::test]
async fn article_response_override_returns_custom_code() {
    let mut overrides = HashMap::new();
    overrides.insert("dead-1".into(), 430u16);
    overrides.insert("dead-2".into(), 502u16);
    overrides.insert("dead-3".into(), 403u16);

    // Even with the article present, the override wins.
    let mut articles = HashMap::new();
    articles.insert("dead-1".into(), b"actual body".to_vec());

    let server = MockNntpServer::start(MockConfig {
        articles,
        article_response_overrides: overrides,
        ..Default::default()
    })
    .await;

    let mut reader = connect(&server).await;
    let _ = read_line(&mut reader).await; // banner
    auth_anon(&mut reader).await;

    for (mid, expected_code) in &[("dead-1", "430"), ("dead-2", "502"), ("dead-3", "403")] {
        let cmd = format!("ARTICLE <{mid}>\r\n");
        reader.get_mut().write_all(cmd.as_bytes()).await.unwrap();
        let resp = read_line(&mut reader).await;
        assert!(
            resp.starts_with(expected_code),
            "for {mid}: expected {expected_code}, got {resp:?}"
        );
    }
}

#[tokio::test]
async fn article_without_override_falls_through_to_404() {
    let server = MockNntpServer::start(MockConfig::default()).await;
    let mut reader = connect(&server).await;
    let _ = read_line(&mut reader).await; // banner
    auth_anon(&mut reader).await;

    reader
        .get_mut()
        .write_all(b"ARTICLE <not-here>\r\n")
        .await
        .unwrap();
    let resp = read_line(&mut reader).await;
    assert!(resp.starts_with("430"), "expected 430, got {resp:?}");
}

// ---------------------------------------------------------------------------
// auth_rate_limit (cross-connection state)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_rate_limit_trips_after_threshold() {
    // Allow 2 attempts per long window. The 3rd PASS should be rejected.
    let limiter = AuthRateLimit::new(2, Duration::from_mins(1));

    let server = MockNntpServer::start(MockConfig {
        auth_rate_limit: Some(limiter),
        ..Default::default()
    })
    .await;

    // Three independent connections. The first 2 PASS attempts should
    // succeed (281), the 3rd should be rate-limited (481).
    for i in 0..3 {
        let mut reader = connect(&server).await;
        let _ = read_line(&mut reader).await; // banner

        reader
            .get_mut()
            .write_all(b"AUTHINFO USER x\r\n")
            .await
            .unwrap();
        let _ = read_line(&mut reader).await; // 381

        reader
            .get_mut()
            .write_all(b"AUTHINFO PASS y\r\n")
            .await
            .unwrap();
        let resp = read_line(&mut reader).await;

        if i < 2 {
            assert!(
                resp.starts_with("281"),
                "attempt {i}: expected 281, got {resp:?}"
            );
        } else {
            assert!(
                resp.starts_with("481"),
                "attempt {i}: expected 481 (rate-limited), got {resp:?}"
            );
        }
    }
}

#[tokio::test]
async fn auth_rate_limit_clones_share_state() {
    // Cloning the limiter (e.g. when MockConfig is cloned) must NOT reset
    // the state — both clones must observe the same window.
    let limiter = AuthRateLimit::new(1, Duration::from_mins(1));
    let server1 = MockNntpServer::start(MockConfig {
        auth_rate_limit: Some(limiter.clone()),
        ..Default::default()
    })
    .await;
    let server2 = MockNntpServer::start(MockConfig {
        auth_rate_limit: Some(limiter),
        ..Default::default()
    })
    .await;

    // First attempt against server1 should succeed.
    let mut r = connect(&server1).await;
    let _ = read_line(&mut r).await;
    r.get_mut().write_all(b"AUTHINFO USER x\r\n").await.unwrap();
    let _ = read_line(&mut r).await;
    r.get_mut().write_all(b"AUTHINFO PASS y\r\n").await.unwrap();
    let r1 = read_line(&mut r).await;
    assert!(r1.starts_with("281"), "server1: {r1:?}");

    // Second attempt against server2 should already be rate-limited
    // because state is shared.
    let mut r = connect(&server2).await;
    let _ = read_line(&mut r).await;
    r.get_mut().write_all(b"AUTHINFO USER x\r\n").await.unwrap();
    let _ = read_line(&mut r).await;
    r.get_mut().write_all(b"AUTHINFO PASS y\r\n").await.unwrap();
    let r2 = read_line(&mut r).await;
    assert!(
        r2.starts_with("481"),
        "server2 should be rate-limited via shared state, got {r2:?}"
    );
}
