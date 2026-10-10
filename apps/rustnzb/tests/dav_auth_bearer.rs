//! `/dav` auth: the web UI's Bearer session token is accepted alongside Basic
//! and X-Api-Key, and a rejected Bearer token never triggers a Basic prompt.
#![cfg(feature = "webdav")]

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router;
use axum::routing::any;
use nzb_web::auth::{CredentialStore, TokenStore};
use nzb_web::nzb_core::config::AppConfig;
use nzb_web::nzb_core::db::Database;
use nzb_web::{AppState, LogBuffer, QueueManager};

struct Harness {
    base: String,
    state: Arc<AppState>,
    _tempdir: tempfile::TempDir,
}

async fn spawn_dav(configure: impl FnOnce(&mut AppConfig)) -> Harness {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let log_buffer = LogBuffer::default();
    let manager = QueueManager::new(
        Vec::new(),
        Database::open_memory().expect("database"),
        tempdir.path().join("incomplete"),
        tempdir.path().join("complete"),
        log_buffer.clone(),
        1,
        Vec::new(),
        0,
        0,
        false,
        5,
        false,
        false,
        100.0,
        30,
    );
    let mut config = AppConfig::default();
    config.dav.enabled = true;
    configure(&mut config);
    let state = Arc::new(AppState::new(
        Arc::new(ArcSwap::from_pointee(config)),
        tempdir.path().join("config.toml"),
        manager,
        log_buffer,
        Arc::new(TokenStore::new()),
        Arc::new(CredentialStore::new(tempdir.path().to_path_buf())),
    ));

    let auth_state = state.clone();
    let dav = Router::new()
        .route("/{*path}", any(|| async { "ok" }))
        .layer(axum::middleware::from_fn(
            move |headers: axum::http::HeaderMap,
                  req: axum::extract::Request,
                  next: axum::middleware::Next| {
                let st = auth_state.clone();
                async move { rustnzb::dav::auth::dav_auth(st, headers, req, next).await }
            },
        ));
    let app = Router::new().nest("/dav", dav);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://{addr}/dav"),
        state,
        _tempdir: tempdir,
    }
}

fn with_basic_creds(cfg: &mut AppConfig) {
    cfg.dav.username = Some("plex".into());
    cfg.dav.password = Some("secret".into());
}

#[tokio::test]
async fn valid_session_bearer_token_is_accepted_on_dav() {
    let h = spawn_dav(with_basic_creds).await;
    let token = h.state.token_store.create_tokens().access_token;

    let resp = reqwest::Client::new()
        .get(format!("{}/content", h.base))
        .bearer_auth(&token)
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn invalid_bearer_token_is_rejected_without_basic_challenge() {
    let h = spawn_dav(with_basic_creds).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/content", h.base))
        .bearer_auth("not-a-session-token")
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 401);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        !challenge.starts_with("Basic"),
        "a rejected Bearer token must not trigger a browser Basic prompt, got {challenge:?}"
    );
}

#[tokio::test]
async fn basic_and_api_key_still_work_and_anonymous_gets_basic_challenge() {
    let h = spawn_dav(|cfg| {
        with_basic_creds(cfg);
        cfg.dav.api_key = Some("dav-key".into());
    })
    .await;
    let client = reqwest::Client::new();
    let url = format!("{}/content", h.base);

    let basic = client
        .get(&url)
        .basic_auth("plex", Some("secret"))
        .send()
        .await
        .expect("request");
    assert_eq!(basic.status(), 200);

    let key = client
        .get(&url)
        .header("X-Api-Key", "dav-key")
        .send()
        .await
        .expect("request");
    assert_eq!(key.status(), 200);

    let anon = client.get(&url).send().await.expect("request");
    assert_eq!(anon.status(), 401);
    assert!(
        anon.headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Basic")),
        "WebDAV clients still need the Basic challenge"
    );
}
