use nzb_web::nzb_core::sabnzbd_import::*;

fn sab_benchmark_ini() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("benchnzb/configs/sabnzbd.ini"),
    )
    .expect("benchnzb/configs/sabnzbd.ini should exist")
}

// ---------------------------------------------------------------------------
// INI parsing — using benchnzb/configs/sabnzbd.ini
// ---------------------------------------------------------------------------

#[test]
fn parse_benchmark_ini() {
    let content = sab_benchmark_ini();
    let preview = parse_sabnzbd_ini(&content);

    // Should have exactly 1 server
    assert_eq!(preview.servers.len(), 1);
    let server = &preview.servers[0];
    assert_eq!(server.host, "mock-nntp");
    assert_eq!(server.port, 119);
    assert_eq!(server.connections, 20);
    assert!(!server.ssl);
    assert!(!server.ssl_verify);
    assert!(server.enabled);
    assert!(!server.optional);
    assert_eq!(server.priority, 0);
    assert_eq!(server.retention, 0);
    assert_eq!(server.username.as_deref(), Some("bench"));
    assert_eq!(server.password.as_deref(), Some("bench"));
    assert!(!server.password_masked);

    // The displayname field should be used as name
    assert!(
        server.name == "Mock NNTP" || server.name == "mock-nntp",
        "server name should be 'Mock NNTP' or 'mock-nntp', got '{}'",
        server.name
    );
}

#[test]
fn parse_benchmark_ini_categories() {
    let content = sab_benchmark_ini();
    let preview = parse_sabnzbd_ini(&content);

    assert_eq!(preview.categories.len(), 1);
    let cat = &preview.categories[0];
    assert_eq!(cat.name, "Default"); // * → Default
    assert_eq!(cat.post_processing, 3);
    assert!(cat.output_dir.is_none());
}

#[test]
fn parse_benchmark_ini_general() {
    let content = sab_benchmark_ini();
    let preview = parse_sabnzbd_ini(&content);

    assert_eq!(
        preview.general.api_key.as_deref(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert_eq!(
        preview.general.complete_dir.as_deref(),
        Some("/downloads/complete")
    );
    assert_eq!(
        preview.general.incomplete_dir.as_deref(),
        Some("/downloads/incomplete")
    );
    assert_eq!(preview.general.speed_limit_bps, 0); // bandwidth_limit = ""
}

// ---------------------------------------------------------------------------
// Bandwidth limit parsing
// ---------------------------------------------------------------------------

#[test]
fn bandwidth_parsing() {
    assert_eq!(parse_bandwidth_limit("50M"), 50 * 1024 * 1024);
    assert_eq!(parse_bandwidth_limit("1G"), 1024 * 1024 * 1024);
    assert_eq!(parse_bandwidth_limit("500K"), 500 * 1024);
    assert_eq!(parse_bandwidth_limit("0"), 0);
    assert_eq!(parse_bandwidth_limit(""), 0);
    assert_eq!(parse_bandwidth_limit("\"\""), 0);
    // Plain number = KB/s (SABnzbd convention)
    assert_eq!(parse_bandwidth_limit("100"), 100 * 1024);
}

// ---------------------------------------------------------------------------
// Bool parsing
// ---------------------------------------------------------------------------

#[test]
fn bool_parsing() {
    assert!(!parse_ini_bool("0"));
    assert!(parse_ini_bool("1"));
    assert!(!parse_ini_bool("no"));
    assert!(parse_ini_bool("yes"));
    assert!(parse_ini_bool("true"));
    assert!(!parse_ini_bool("false"));
}

// ---------------------------------------------------------------------------
// Multi-server INI fixture
// ---------------------------------------------------------------------------

#[test]
fn multi_server_ini() {
    let content = r#"
[misc]
api_key = testkey123

[servers]
[[primary]]
name = primary
host = news.example.com
port = 563
ssl = 1
ssl_verify = 1
username = user1
password = pass1
connections = 30
enable = 1
optional = 0
priority = 0
retention = 3000
displayname = Primary Server

[[backup]]
name = backup
host = backup.example.com
port = 119
ssl = 0
ssl_verify = 0
username = user2
password = pass2
connections = 10
enable = 1
optional = 1
priority = 1
retention = 500
displayname = Backup Server
"#;

    let preview = parse_sabnzbd_ini(content);
    assert_eq!(preview.servers.len(), 2);

    let primary = preview
        .servers
        .iter()
        .find(|s| s.host == "news.example.com")
        .unwrap();
    assert_eq!(primary.name, "Primary Server");
    assert_eq!(primary.port, 563);
    assert!(primary.ssl);
    assert!(primary.ssl_verify);
    assert_eq!(primary.connections, 30);
    assert_eq!(primary.priority, 0);
    assert_eq!(primary.retention, 3000);
    assert!(!primary.optional);

    let backup = preview
        .servers
        .iter()
        .find(|s| s.host == "backup.example.com")
        .unwrap();
    assert_eq!(backup.name, "Backup Server");
    assert_eq!(backup.port, 119);
    assert!(!backup.ssl);
    assert_eq!(backup.connections, 10);
    assert_eq!(backup.priority, 1);
    assert_eq!(backup.retention, 500);
    assert!(backup.optional);
}

// ---------------------------------------------------------------------------
// API response parsing
// ---------------------------------------------------------------------------

#[test]
fn api_response_parsing() {
    let json: serde_json::Value = serde_json::from_str(
        r#"{
        "config": {
            "misc": {
                "api_key": "abc123",
                "complete_dir": "/data/complete",
                "download_dir": "/data/incomplete",
                "bandwidth_limit": "25M"
            },
            "servers": [
                {
                    "name": "srv1",
                    "displayname": "My Server",
                    "host": "news.provider.com",
                    "port": 563,
                    "ssl": 1,
                    "ssl_verify": 1,
                    "username": "myuser",
                    "password": "**********",
                    "connections": 20,
                    "enable": 1,
                    "optional": 0,
                    "priority": 0,
                    "retention": 4000
                }
            ],
            "categories": [
                { "name": "*", "pp": 3, "dir": "", "script": "Default" },
                { "name": "movies", "pp": 2, "dir": "Movies", "script": "Default" }
            ]
        }
    }"#,
    )
    .unwrap();

    let preview = parse_sabnzbd_api_response(&json);

    // Server
    assert_eq!(preview.servers.len(), 1);
    let s = &preview.servers[0];
    assert_eq!(s.name, "My Server");
    assert_eq!(s.host, "news.provider.com");
    assert_eq!(s.port, 563);
    assert!(s.ssl);
    assert!(s.password_masked);
    assert_eq!(s.connections, 20);

    // Categories
    assert_eq!(preview.categories.len(), 2);
    assert_eq!(preview.categories[0].name, "Default"); // * → Default
    assert_eq!(preview.categories[1].name, "movies");
    assert_eq!(preview.categories[1].post_processing, 2);

    // General
    assert_eq!(preview.general.api_key.as_deref(), Some("abc123"));
    assert_eq!(preview.general.speed_limit_bps, 25 * 1024 * 1024);

    // Should have a warning about masked password
    assert!(preview.warnings.iter().any(|w| w.contains("masked")));
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn empty_ini() {
    let preview = parse_sabnzbd_ini("");
    assert!(preview.servers.is_empty());
    assert!(preview.categories.is_empty());
    assert!(preview.warnings.is_empty());
}

#[test]
fn ini_no_servers() {
    let content = "[misc]\napi_key = test\n";
    let preview = parse_sabnzbd_ini(content);
    assert!(preview.servers.is_empty());
    assert_eq!(preview.general.api_key.as_deref(), Some("test"));
}

#[test]
fn ini_malformed_lines() {
    let content = r#"
[misc]
api_key = testkey
this is not a valid line
= also invalid
[servers]
[[s1]]
host = good.host.com
port = not_a_number
connections = 8
"#;
    let preview = parse_sabnzbd_ini(content);
    assert_eq!(preview.servers.len(), 1);
    assert_eq!(preview.servers[0].host, "good.host.com");
    // port should fall back to default 563 since "not_a_number" won't parse
    assert_eq!(preview.servers[0].port, 563);
    assert_eq!(preview.servers[0].connections, 8);
}

// ---------------------------------------------------------------------------
// ServerConfig conversion
// ---------------------------------------------------------------------------

#[test]
fn imported_server_to_config() {
    let imported = ImportedServer {
        name: "Test".into(),
        host: "news.test.com".into(),
        port: 563,
        ssl: true,
        ssl_verify: true,
        username: Some("user".into()),
        password: Some("pass".into()),
        password_masked: false,
        connections: 16,
        priority: 0,
        enabled: true,
        retention: 2000,
        optional: false,
    };

    let sc = imported.to_server_config();
    assert_eq!(sc.host, "news.test.com");
    assert_eq!(sc.port, 563);
    assert!(sc.ssl);
    assert_eq!(sc.connections, 16);
    assert_eq!(sc.pipelining, 4);
    assert!(!sc.id.is_empty()); // UUID was generated
}
