//! Functional tests for NNTP connection lifecycle using the in-process mock server.

use std::collections::HashMap;

use nzb_nntp::capabilities::NntpCapabilities;
use nzb_nntp::connection::NntpConnection;
use nzb_nntp::error::NntpError;
use nzb_nntp::testutil::{MockConfig, MockNntpServer, test_config, test_config_with_auth};

// ---------------------------------------------------------------------------
// Connection & disconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_connect_and_quit() {
    let server = MockNntpServer::start(MockConfig::default()).await;
    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();
    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_connect_with_auth() {
    let server = MockNntpServer::start(MockConfig {
        auth_required: true,
        valid_credentials: Some(("user".into(), "pass".into())),
        ..Default::default()
    })
    .await;

    let config = test_config_with_auth(server.port(), "user", "pass");
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();
    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_connect_auth_failure() {
    let server = MockNntpServer::start(MockConfig {
        auth_required: true,
        fail_auth: true,
        ..Default::default()
    })
    .await;

    let config = test_config_with_auth(server.port(), "user", "wrong");
    let mut conn = NntpConnection::new("test".into());
    let err = conn.connect(&config).await.unwrap_err();
    assert!(
        matches!(err, NntpError::Auth(_)),
        "expected Auth error, got: {err}"
    );
}

#[tokio::test]
async fn test_connect_service_unavailable() {
    let server = MockNntpServer::start(MockConfig {
        service_unavailable: true,
        ..Default::default()
    })
    .await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    let err = conn.connect(&config).await.unwrap_err();
    assert!(
        matches!(err, NntpError::ServiceUnavailable(_)),
        "expected ServiceUnavailable error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Article fetch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fetch_article_body() {
    let body = b"This is the article content.\r\nLine 2.".to_vec();
    let mut articles = HashMap::new();
    articles.insert("test-article-1".to_string(), body.clone());

    let server = MockNntpServer::start(MockConfig {
        articles,
        ..Default::default()
    })
    .await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let resp = conn.fetch_article("test-article-1").await.unwrap();
    assert_eq!(resp.code, 220);
    let data = resp.data.expect("article should have body");
    let data_str = String::from_utf8_lossy(&data);
    assert!(
        data_str.contains("This is the article content."),
        "body should contain expected text, got: {data_str}"
    );

    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_fetch_missing_article() {
    let server = MockNntpServer::start(MockConfig::default()).await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let err = conn.fetch_article("nonexistent-article").await.unwrap_err();
    assert!(
        matches!(err, NntpError::ArticleNotFound(_)),
        "expected ArticleNotFound, got: {err}"
    );

    conn.quit().await.unwrap();
}

// ---------------------------------------------------------------------------
// STAT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stat_article_exists() {
    let mut articles = HashMap::new();
    articles.insert("exists-1".to_string(), b"data".to_vec());

    let server = MockNntpServer::start(MockConfig {
        articles,
        ..Default::default()
    })
    .await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let resp = conn.stat_article("exists-1").await.unwrap();
    assert_eq!(resp.code, 223);

    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_stat_article_fatal_codes_mark_connection_error() {
    use nzb_nntp::connection::ConnectionState;

    for (code, check) in [
        (
            502u16,
            (|e: &NntpError| matches!(e, NntpError::ServiceUnavailable(_)))
                as fn(&NntpError) -> bool,
        ),
        (403, |e| matches!(e, NntpError::PermissionDenied(_))),
        (400, |e| {
            matches!(e, NntpError::Protocol(_) | NntpError::Connection(_))
        }),
    ] {
        let mut overrides = HashMap::new();
        overrides.insert("doomed".to_string(), code);
        let server = MockNntpServer::start(MockConfig {
            article_response_overrides: overrides,
            ..Default::default()
        })
        .await;
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&test_config(server.port())).await.unwrap();

        let err = conn.stat_article("doomed").await.unwrap_err();
        assert!(check(&err), "{code}: unexpected error {err:?}");
        assert_eq!(
            conn.state,
            ConnectionState::Error,
            "{code} must not leave the connection reusable"
        );
    }
}

#[tokio::test]
async fn test_stat_article_missing() {
    let server = MockNntpServer::start(MockConfig::default()).await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let err = conn.stat_article("no-such-article").await.unwrap_err();
    assert!(
        matches!(err, NntpError::ArticleNotFound(_)),
        "expected ArticleNotFound, got: {err}"
    );

    conn.quit().await.unwrap();
}

// ---------------------------------------------------------------------------
// GROUP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_group_select() {
    let mut groups = HashMap::new();
    groups.insert("alt.test".to_string(), (1000, 1, 1000));

    let server = MockNntpServer::start(MockConfig {
        groups,
        ..Default::default()
    })
    .await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let group = conn.group("alt.test").await.unwrap();
    assert_eq!(group.count, 1000);
    assert_eq!(group.first, 1);
    assert_eq!(group.last, 1000);

    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_group_not_found() {
    let server = MockNntpServer::start(MockConfig::default()).await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let err = conn.group("nonexistent.group").await.unwrap_err();
    assert!(
        matches!(err, NntpError::NoSuchGroup(_)),
        "expected NoSuchGroup, got: {err}"
    );

    conn.quit().await.unwrap();
}

// ---------------------------------------------------------------------------
// XOVER
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_xover_range() {
    let mut groups = HashMap::new();
    groups.insert("alt.test".to_string(), (3, 1, 3));

    let server = MockNntpServer::start(MockConfig {
        groups,
        xover_entries: vec![
            "1\tSubject1\tuser@test\t01 Jan 2024\t<mid1>\t\t100\t5".to_string(),
            "2\tSubject2\tuser@test\t02 Jan 2024\t<mid2>\t\t200\t10".to_string(),
            "3\tSubject3\tuser@test\t03 Jan 2024\t<mid3>\t\t300\t15".to_string(),
        ],
        ..Default::default()
    })
    .await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    conn.group("alt.test").await.unwrap();
    let entries = conn.xover(1, 3).await.unwrap();
    assert_eq!(entries.len(), 3, "should return 3 XOVER entries");

    conn.quit().await.unwrap();
}

// ---------------------------------------------------------------------------
// Dot-stuffing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_article_with_dot_stuffing() {
    let mut articles = HashMap::new();
    // Body contains a line starting with "." which the mock server will dot-stuff.
    articles.insert(
        "dot-test".to_string(),
        b"Line 1\r\n.Line starting with dot\r\nLine 3".to_vec(),
    );

    let server = MockNntpServer::start(MockConfig {
        articles,
        ..Default::default()
    })
    .await;

    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let resp = conn.fetch_article("dot-test").await.unwrap();
    let data = resp.data.expect("should have body");
    let text = String::from_utf8_lossy(&data);
    assert!(
        text.contains(".Line starting with dot"),
        "dot-stuffed line should be unescaped: {text}"
    );

    conn.quit().await.unwrap();
}

// ---------------------------------------------------------------------------
// CAPABILITIES (RFC 3977 §5.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_capabilities_probed_on_connect() {
    let server = MockNntpServer::start(MockConfig::default()).await;
    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let caps = conn.capabilities();
    assert!(caps.probed, "CAPABILITIES should have been probed");
    assert!(caps.reader, "mock server advertises READER");
    assert!(caps.have_body);
    assert!(caps.have_stat);
    assert!(caps.have_article);
    assert!(caps.have_head);
    assert!(caps.hdr, "mock advertises HDR");
    assert!(caps.over, "mock advertises OVER");
    assert!(caps.over_msgid, "mock advertises OVER MSGID");
    assert_eq!(caps.version.as_deref(), Some("2"));

    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_capabilities_unsupported_falls_back_to_defaults() {
    // Server returns 500 to CAPABILITIES — pre-RFC-3977 behaviour. Client
    // should fall back to permissive defaults so BODY/STAT remain usable.
    let server = MockNntpServer::start(MockConfig {
        capabilities_unsupported: true,
        ..Default::default()
    })
    .await;
    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let caps = conn.capabilities();
    assert!(!caps.probed, "server rejected CAPABILITIES");
    assert!(caps.have_body, "defaults must keep BODY available");
    assert!(caps.have_stat, "defaults must keep STAT available");
    assert!(caps.have_article);
    assert!(caps.have_head);

    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_mode_reader_transition_on_transit_server() {
    // Server advertises MODE-READER (transit mode); client must issue
    // MODE READER and the derived flags must reflect reader mode being active.
    let server = MockNntpServer::start(MockConfig {
        capabilities_mode_reader: true,
        ..Default::default()
    })
    .await;
    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let caps = conn.capabilities();
    assert!(caps.probed);
    assert!(
        caps.mode_reader_required,
        "server advertised MODE-READER capability"
    );
    assert!(
        caps.reader,
        "MODE READER transition should mark reader mode active"
    );
    assert!(caps.have_body);
    assert!(caps.have_stat);
    assert!(caps.have_article);
    assert!(caps.have_head);

    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_fetch_body_falls_back_to_article_and_strips_headers() {
    // Real CAPABILITIES responses can't produce have_body=false /
    // have_article=true, so we drive the fallback by overriding caps directly.
    // The mock has the article available via ARTICLE; we assert the fallback
    // returns body-only bytes (headers stripped) under BODY's status code.
    let mut articles = HashMap::new();
    let raw_article =
        b"Subject: hi\r\nFrom: a@b\r\nMessage-ID: <fb1@test>\r\n\r\nBODY-LINE-1\r\nBODY-LINE-2";
    articles.insert("fb1@test".into(), raw_article.to_vec());

    let server = MockNntpServer::start(MockConfig {
        articles,
        ..Default::default()
    })
    .await;
    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    // Force the capability state we want to exercise: BODY unavailable,
    // ARTICLE available.
    let mut caps = NntpCapabilities::default_assumed();
    caps.have_body = false;
    caps.have_article = true;
    conn.set_capabilities_for_test(caps);

    let resp = conn.fetch_body("fb1@test").await.unwrap();
    assert_eq!(resp.code, 222, "fallback must report BODY's status code");
    let body = resp.data.expect("body data");
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        !text.contains("Subject:"),
        "headers must be stripped, got: {text:?}"
    );
    assert!(text.starts_with("BODY-LINE-1"), "got: {text:?}");
    assert!(text.contains("BODY-LINE-2"), "got: {text:?}");

    conn.quit().await.unwrap();
}

#[tokio::test]
async fn test_stat_unavailable_returns_article_not_found() {
    // When the server's caps don't include STAT, callers should see
    // ArticleNotFound (cheap fail-over) rather than a Protocol error
    // (which would mark the connection broken).
    let server = MockNntpServer::start(MockConfig::default()).await;
    let config = test_config(server.port());
    let mut conn = NntpConnection::new("test".into());
    conn.connect(&config).await.unwrap();

    let mut caps = NntpCapabilities::default_assumed();
    caps.have_stat = false;
    conn.set_capabilities_for_test(caps);

    match conn.stat_article("xyz@test").await {
        Err(NntpError::ArticleNotFound(mid)) => assert_eq!(mid, "<xyz@test>"),
        other => panic!("expected ArticleNotFound, got {other:?}"),
    }

    conn.quit().await.unwrap();
}
