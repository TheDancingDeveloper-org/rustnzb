//! Outbound notifications for job outcomes and disk pressure.
//!
//! Three targets are supported, each configured under `[notifications]`:
//!
//! - a generic JSON webhook, POSTed one document per event;
//! - an Apprise API endpoint, which fans out to whatever services Apprise
//!   is configured for;
//! - an ntfy topic, published through the server's JSON publish API.
//!
//! Delivery runs on a background task and never blocks the queue. A failed
//! delivery is logged once, without the target URL (which may carry a
//! secret), and is not retried.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::nzb_core::config::NotificationConfig;

/// Per-request timeout for a notification delivery.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The outcome a notification reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    /// A job finished and reached history as completed.
    Complete,
    /// A job finished and reached history as failed.
    Failed,
    /// Free space on a storage volume fell below the minimum, and the
    /// engine stopped work because of it.
    DiskFull,
}

impl NotificationKind {
    /// Stable identifier used in the webhook payload.
    pub fn key(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::DiskFull => "disk_full",
        }
    }

    /// Apprise message type for this outcome.
    fn apprise_type(self) -> &'static str {
        match self {
            Self::Complete => "success",
            Self::Failed => "failure",
            Self::DiskFull => "warning",
        }
    }

    /// ntfy priority (1 to 5) for this outcome.
    fn ntfy_priority(self) -> u8 {
        match self {
            Self::Complete => 3,
            Self::Failed => 4,
            Self::DiskFull => 5,
        }
    }

    /// ntfy emoji tag for this outcome.
    fn ntfy_tag(self) -> &'static str {
        match self {
            Self::Complete => "white_check_mark",
            Self::Failed => "x",
            Self::DiskFull => "warning",
        }
    }
}

/// One notification, independent of the target it is sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationEvent {
    pub kind: NotificationKind,
    pub title: String,
    pub message: String,
    pub job_name: Option<String>,
    pub category: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

impl NotificationEvent {
    /// An event not tied to a job, such as a disk-space alarm.
    pub fn new(
        kind: NotificationKind,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            message: message.into(),
            job_name: None,
            category: None,
            occurred_at: Utc::now(),
        }
    }

    /// Attach the job the event is about.
    pub fn for_job(mut self, name: &str, category: &str) -> Self {
        self.job_name = Some(name.to_string());
        self.category = (!category.is_empty()).then(|| category.to_string());
        self
    }

    /// Body of the generic webhook: stable keys, RFC 3339 timestamp.
    pub fn webhook_payload(&self) -> Value {
        json!({
            "event": self.kind.key(),
            "title": self.title,
            "message": self.message,
            "job": self.job_name,
            "category": self.category,
            "timestamp": self.occurred_at.to_rfc3339(),
            "source": "rustnzb",
        })
    }

    /// Body for the Apprise API stateless or keyed notify endpoint.
    pub fn apprise_payload(&self, apprise_urls: Option<&str>) -> Value {
        let mut body = json!({
            "title": self.title,
            "body": self.message,
            "type": self.kind.apprise_type(),
        });
        if let Some(urls) = apprise_urls {
            body["urls"] = Value::String(urls.to_string());
        }
        body
    }

    /// Body for an ntfy JSON publish to the server root.
    pub fn ntfy_payload(&self, topic: &str) -> Value {
        json!({
            "topic": topic,
            "title": self.title,
            "message": self.message,
            "priority": self.kind.ntfy_priority(),
            "tags": [self.kind.ntfy_tag()],
        })
    }
}

/// A parsed ntfy target.
#[derive(Debug, Clone)]
struct NtfyTarget {
    server: reqwest::Url,
    topic: String,
    token: Option<String>,
}

/// Sends events to every configured target.
#[derive(Debug)]
pub struct Notifier {
    client: reqwest::Client,
    webhook: Option<reqwest::Url>,
    apprise: Option<reqwest::Url>,
    apprise_urls: Option<String>,
    ntfy: Option<NtfyTarget>,
}

impl Notifier {
    /// Build the notifier from configuration. A target with an invalid URL
    /// is logged and left disabled, so the others still work.
    pub fn from_config(config: &NotificationConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DELIVERY_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client,
            webhook: parse_target_url("webhook_url", &config.webhook_url),
            apprise: parse_target_url("apprise_api_url", &config.apprise_api_url),
            apprise_urls: non_empty(&config.apprise_urls),
            ntfy: parse_ntfy(config),
        }
    }

    /// A notifier with no targets; every call is a no-op.
    pub fn disabled() -> Self {
        Self::from_config(&NotificationConfig::default())
    }

    /// Whether at least one target is configured.
    pub fn is_enabled(&self) -> bool {
        self.webhook.is_some() || self.apprise.is_some() || self.ntfy.is_some()
    }

    /// Queue an event for delivery to every target. Returns immediately.
    /// Must be called from within a Tokio runtime when a target is enabled.
    pub fn notify(self: &Arc<Self>, event: NotificationEvent) {
        if !self.is_enabled() {
            return;
        }
        let notifier = Arc::clone(self);
        tokio::spawn(async move {
            for (target, result) in notifier.deliver(&event).await {
                match result {
                    Ok(()) => debug!(target, event = event.kind.key(), "Notification delivered"),
                    Err(e) => warn!(
                        target,
                        event = event.kind.key(),
                        error = %e.without_url(),
                        "Notification delivery failed"
                    ),
                }
            }
        });
    }

    /// Send an event to every configured target and report each outcome.
    pub async fn deliver(
        &self,
        event: &NotificationEvent,
    ) -> Vec<(&'static str, Result<(), reqwest::Error>)> {
        let mut outcomes = Vec::with_capacity(3);
        if let Some(url) = &self.webhook {
            let result = self.post_json(url, None, &event.webhook_payload()).await;
            outcomes.push(("webhook", result));
        }
        if let Some(url) = &self.apprise {
            let payload = event.apprise_payload(self.apprise_urls.as_deref());
            outcomes.push(("apprise", self.post_json(url, None, &payload).await));
        }
        if let Some(ntfy) = &self.ntfy {
            let payload = event.ntfy_payload(&ntfy.topic);
            let result = self
                .post_json(&ntfy.server, ntfy.token.as_deref(), &payload)
                .await;
            outcomes.push(("ntfy", result));
        }
        outcomes
    }

    async fn post_json(
        &self,
        url: &reqwest::Url,
        bearer: Option<&str>,
        body: &Value,
    ) -> Result<(), reqwest::Error> {
        let mut request = self.client.post(url.clone()).json(body);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        request.send().await?.error_for_status()?;
        Ok(())
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Parse an http or https target URL. Empty means disabled.
fn parse_target_url(name: &str, raw: &str) -> Option<reqwest::Url> {
    let raw = non_empty(raw)?;
    match reqwest::Url::parse(&raw) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Some(url),
        _ => {
            // Log the setting name only: the value may hold a secret.
            warn!(
                setting = name,
                "Notification target disabled: not a valid http(s) URL"
            );
            None
        }
    }
}

fn parse_ntfy(config: &NotificationConfig) -> Option<NtfyTarget> {
    let topic = non_empty(&config.ntfy_topic)?;
    let server = parse_target_url("ntfy_server", &config.ntfy_server)?;
    Some(NtfyTarget {
        server,
        topic,
        token: non_empty(&config.ntfy_token),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sample(kind: NotificationKind) -> NotificationEvent {
        NotificationEvent::new(kind, "Download complete", "Ubuntu.iso finished")
            .for_job("Ubuntu.iso", "tv")
    }

    #[test]
    fn webhook_payload_has_stable_keys() {
        let payload = sample(NotificationKind::Complete).webhook_payload();
        assert_eq!(payload["event"], "complete");
        assert_eq!(payload["title"], "Download complete");
        assert_eq!(payload["message"], "Ubuntu.iso finished");
        assert_eq!(payload["job"], "Ubuntu.iso");
        assert_eq!(payload["category"], "tv");
        assert_eq!(payload["source"], "rustnzb");
        assert!(payload["timestamp"].as_str().is_some());
    }

    #[test]
    fn disk_event_has_no_job_fields() {
        let event = NotificationEvent::new(NotificationKind::DiskFull, "Disk", "Low space");
        let payload = event.webhook_payload();
        assert_eq!(payload["event"], "disk_full");
        assert!(payload["job"].is_null());
        assert!(payload["category"].is_null());
    }

    #[test]
    fn empty_category_is_omitted() {
        let event =
            NotificationEvent::new(NotificationKind::Failed, "Failed", "bad").for_job("x", "");
        assert_eq!(event.category, None);
    }

    #[test]
    fn apprise_payload_maps_outcome_to_type() {
        let payload = sample(NotificationKind::Failed).apprise_payload(Some("discord://a/b"));
        assert_eq!(payload["type"], "failure");
        assert_eq!(payload["body"], "Ubuntu.iso finished");
        assert_eq!(payload["urls"], "discord://a/b");

        let keyed = sample(NotificationKind::DiskFull).apprise_payload(None);
        assert_eq!(keyed["type"], "warning");
        assert!(keyed.get("urls").is_none());
    }

    #[test]
    fn ntfy_payload_sets_priority_and_tag() {
        let payload = sample(NotificationKind::DiskFull).ntfy_payload("rustnzb-alerts");
        assert_eq!(payload["topic"], "rustnzb-alerts");
        assert_eq!(payload["priority"], 5);
        assert_eq!(payload["tags"][0], "warning");
    }

    #[test]
    fn disabled_config_builds_no_targets() {
        let notifier = Notifier::from_config(&NotificationConfig::default());
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn invalid_urls_are_disabled_individually() {
        let config = NotificationConfig {
            webhook_url: "ftp://example.invalid/hook".into(),
            apprise_api_url: "not a url".into(),
            ntfy_topic: "alerts".into(),
            ..NotificationConfig::default()
        };
        let notifier = Notifier::from_config(&config);
        assert!(notifier.webhook.is_none());
        assert!(notifier.apprise.is_none());
        assert!(notifier.ntfy.is_some());
    }

    #[test]
    fn ntfy_needs_a_topic() {
        let config = NotificationConfig {
            ntfy_topic: String::new(),
            ..NotificationConfig::default()
        };
        assert!(Notifier::from_config(&config).ntfy.is_none());
    }

    /// Serve one HTTP request on loopback and return the request head and body.
    async fn one_shot_server() -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while buf.len() < end + 4 + length {
                        let n = socket.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                    return String::from_utf8_lossy(&buf).to_string();
                }
            }
        });
        (url, handle)
    }

    #[tokio::test]
    async fn webhook_posts_json_to_configured_url() {
        let (url, server) = one_shot_server().await;
        let notifier = Notifier::from_config(&NotificationConfig {
            webhook_url: url,
            ..NotificationConfig::default()
        });

        let outcomes = notifier.deliver(&sample(NotificationKind::Complete)).await;

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].0, "webhook");
        assert!(outcomes[0].1.is_ok());
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /hook "));
        assert!(request.contains("\"event\":\"complete\""));
    }

    #[tokio::test]
    async fn non_success_status_is_reported_as_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let notifier = Notifier::from_config(&NotificationConfig {
            webhook_url: url,
            ..NotificationConfig::default()
        });

        let outcomes = notifier.deliver(&sample(NotificationKind::Failed)).await;

        assert!(outcomes[0].1.is_err());
    }
}
