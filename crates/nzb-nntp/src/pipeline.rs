//! Request pipelining for NNTP connections.
//!
//! NNTP responses are strictly ordered, so we can send multiple ARTICLE
//! commands before reading responses. This dramatically improves throughput
//! on high-latency links.

use std::collections::VecDeque;

use tracing::trace;

use crate::connection::{ConnectionState, NntpConnection, NntpResponse};
use crate::error::{NntpError, NntpResult};

// ---------------------------------------------------------------------------
// Pipeline request
// ---------------------------------------------------------------------------

/// A request that has been sent and is awaiting a response.
#[derive(Debug, Clone)]
pub struct PipelineRequest {
    /// The message-id that was requested.
    pub message_id: String,
    /// An opaque tag the caller can use to correlate requests.
    pub tag: u64,
}

/// The result for one pipelined article fetch.
#[derive(Debug)]
pub struct PipelineResult {
    /// The original request.
    pub request: PipelineRequest,
    /// The fetch outcome.
    pub result: NntpResult<NntpResponse>,
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Pipelined NNTP command sender/receiver.
///
/// Usage:
/// 1. Call `submit()` to queue article requests.
/// 2. Internally, up to `depth` ARTICLE commands are sent before any
///    responses are read.
/// 3. Call `receive_one()` to read the next response.
/// 4. Call `drain()` to read all outstanding responses.
pub struct Pipeline {
    /// Maximum number of in-flight requests.
    depth: usize,
    /// Requests that have been sent but whose responses have not been read.
    in_flight: VecDeque<PipelineRequest>,
    /// Requests queued locally but not yet sent to the server.
    pending: VecDeque<PipelineRequest>,
}

impl Pipeline {
    /// Create a new pipeline with the given depth (from `ServerConfig::pipelining`).
    /// A depth of 0 or 1 means no pipelining (send one, read one).
    pub fn new(depth: u8) -> Self {
        let depth = (depth as usize).max(1);
        Self {
            depth,
            in_flight: VecDeque::with_capacity(depth),
            pending: VecDeque::new(),
        }
    }

    /// Queue an article fetch request.
    pub fn submit(&mut self, message_id: String, tag: u64) {
        self.pending.push_back(PipelineRequest { message_id, tag });
    }

    /// Number of requests that have been sent but not yet received.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Number of requests waiting to be sent.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// True if there are no pending or in-flight requests.
    pub fn is_empty(&self) -> bool {
        self.in_flight.is_empty() && self.pending.is_empty()
    }

    /// Send as many pending requests as the pipeline depth allows.
    ///
    /// This only sends the ARTICLE commands; it does NOT read any responses.
    /// All commands are buffered first, then flushed once — this is critical
    /// for pipelining performance (avoids per-command TCP flushes).
    pub async fn flush_sends(&mut self, conn: &mut NntpConnection) -> NntpResult<()> {
        let mut sent = 0usize;
        while self.in_flight.len() < self.depth {
            let Some(req) = self.pending.pop_front() else {
                break;
            };

            let mid = if req.message_id.starts_with('<') {
                req.message_id.clone()
            } else {
                format!("<{}>", req.message_id)
            };

            conn.send_command_no_flush(&format!("ARTICLE {mid}"))
                .await?;
            trace!(mid = %mid, tag = req.tag, "Pipeline sent ARTICLE");
            self.in_flight.push_back(req);
            sent += 1;
        }
        if sent > 0 {
            conn.flush().await?;
        }
        Ok(())
    }

    /// Read one response from the server, matching it to the oldest in-flight
    /// request. Returns `None` if there are no in-flight requests.
    pub async fn receive_one(
        &mut self,
        conn: &mut NntpConnection,
    ) -> NntpResult<Option<PipelineResult>> {
        let Some(request) = self.in_flight.pop_front() else {
            return Ok(None);
        };

        let status = conn.read_response_line().await?;

        let result = match status.code {
            220 => {
                // Article follows — read multi-line body
                match conn.read_multiline_body().await {
                    Ok(data) => Ok(NntpResponse {
                        code: status.code,
                        message: status.message,
                        data: Some(data),
                    }),
                    Err(e) => Err(e),
                }
            }
            430 => Err(NntpError::ArticleNotFound(request.message_id.clone())),
            411 => Err(NntpError::NoSuchGroup(status.message)),
            412 | 420 => Err(NntpError::NoArticleSelected(status.message)),
            403 => {
                conn.state = ConnectionState::Error;
                Err(NntpError::PermissionDenied(status.message))
            }
            480 => {
                conn.state = ConnectionState::Error;
                Err(NntpError::AuthRequired(status.message))
            }
            481 | 482 => {
                conn.state = ConnectionState::Error;
                Err(NntpError::Auth(format!(
                    "ARTICLE rejected ({}): {}",
                    status.code, status.message
                )))
            }
            502 => {
                conn.state = ConnectionState::Error;
                Err(NntpError::ServiceUnavailable(status.message))
            }
            _ => {
                conn.state = ConnectionState::Error;
                Err(crate::error::unexpected_article_response(
                    status.code,
                    status.message,
                ))
            }
        };

        Ok(Some(PipelineResult { request, result }))
    }

    /// Convenience: submit requests, flush sends, and read all responses.
    ///
    /// This interleaves sending and receiving to keep the pipeline full.
    pub async fn process_all(
        &mut self,
        conn: &mut NntpConnection,
    ) -> NntpResult<Vec<PipelineResult>> {
        let mut results = Vec::with_capacity(self.pending.len() + self.in_flight.len());

        loop {
            // Fill the pipeline
            self.flush_sends(conn).await?;

            if self.in_flight.is_empty() {
                break;
            }

            // Read one response
            if let Some(result) = self.receive_one(conn).await? {
                // If the connection entered an error state, bail out
                let is_fatal = matches!(
                    &result.result,
                    Err(NntpError::Auth(_))
                        | Err(NntpError::AuthRequired(_))
                        | Err(NntpError::PermissionDenied(_))
                        | Err(NntpError::ServiceUnavailable(_))
                        | Err(NntpError::Connection(_))
                        | Err(NntpError::Io(_))
                );
                results.push(result);
                if is_fatal {
                    // Drain remaining in-flight as errors
                    while let Some(req) = self.in_flight.pop_front() {
                        results.push(PipelineResult {
                            request: req,
                            result: Err(NntpError::Connection(
                                "Pipeline aborted due to fatal error".into(),
                            )),
                        });
                    }
                    // Move pending back as errors too
                    while let Some(req) = self.pending.pop_front() {
                        results.push(PipelineResult {
                            request: req,
                            result: Err(NntpError::Connection(
                                "Pipeline aborted due to fatal error".into(),
                            )),
                        });
                    }
                    break;
                }
            }
        }

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// StatPipeline — pipelined STAT commands for availability checking
// ---------------------------------------------------------------------------

/// Result of a single STAT check.
#[derive(Debug)]
pub struct StatResult {
    /// The message-id that was checked.
    pub message_id: String,
    /// Whether the article exists on the server.
    pub exists: bool,
}

/// Batch STAT checker. Sends all STAT commands in bulk, reads responses in
/// order. Much faster than individual stat_article() calls on high-latency links.
pub struct StatPipeline {
    pending: Vec<String>,
}

/// Maximum number of STAT commands to pipeline in a single batch.
/// Prevents overwhelming servers that may disconnect on very deep pipelines.
const STAT_BATCH_SIZE: usize = 100;

impl StatPipeline {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Queue a message-id for STAT checking.
    pub fn add(&mut self, message_id: String) {
        self.pending.push(message_id);
    }

    /// Number of queued message-ids.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True if no message-ids are queued.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Execute all queued STAT commands and return results.
    ///
    /// Sends commands in batches of up to [`STAT_BATCH_SIZE`], then reads
    /// all responses. STAT responses are single-line (no body), so this is
    /// simpler than the ARTICLE pipeline.
    pub async fn execute(&mut self, conn: &mut NntpConnection) -> NntpResult<Vec<StatResult>> {
        let ids = std::mem::take(&mut self.pending);
        let mut results = Vec::with_capacity(ids.len());

        for batch in ids.chunks(STAT_BATCH_SIZE) {
            // Set connection to Busy for the duration of this batch
            conn.state = ConnectionState::Busy;

            // Send all STAT commands in this batch (buffered, single flush)
            for mid in batch {
                let normalized = if mid.starts_with('<') && mid.ends_with('>') {
                    mid.clone()
                } else {
                    format!("<{mid}>")
                };
                conn.send_command_no_flush(&format!("STAT {normalized}"))
                    .await?;
                trace!(mid = %normalized, "StatPipeline sent STAT");
            }
            conn.flush().await?;

            // Read responses in order
            for mid in batch {
                let resp = conn.read_response_line().await?;
                match resp.code {
                    223 => {
                        results.push(StatResult {
                            message_id: mid.clone(),
                            exists: true,
                        });
                    }
                    430 => {
                        results.push(StatResult {
                            message_id: mid.clone(),
                            exists: false,
                        });
                    }
                    480 => {
                        conn.state = ConnectionState::Error;
                        return Err(NntpError::AuthRequired(resp.message));
                    }
                    481 | 482 => {
                        conn.state = ConnectionState::Error;
                        return Err(NntpError::Auth(format!(
                            "STAT rejected ({}): {}",
                            resp.code, resp.message
                        )));
                    }
                    502 => {
                        conn.state = ConnectionState::Error;
                        return Err(NntpError::ServiceUnavailable(resp.message));
                    }
                    _ => {
                        // Unknown response — treat as missing but don't abort
                        trace!(code = resp.code, mid = %mid, "Unexpected STAT response");
                        results.push(StatResult {
                            message_id: mid.clone(),
                            exists: false,
                        });
                    }
                }
            }

            conn.state = ConnectionState::Ready;
        }

        Ok(results)
    }
}

impl Default for StatPipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockConfig, MockNntpServer, test_config};
    use std::collections::HashMap;

    #[test]
    fn test_pipeline_submit_and_counts() {
        let mut pipe = Pipeline::new(5);
        assert!(pipe.is_empty());

        pipe.submit("abc@example.com".into(), 1);
        pipe.submit("def@example.com".into(), 2);

        assert_eq!(pipe.pending_count(), 2);
        assert_eq!(pipe.in_flight_count(), 0);
        assert!(!pipe.is_empty());
    }

    #[test]
    fn test_pipeline_depth_minimum() {
        let pipe = Pipeline::new(0);
        assert_eq!(pipe.depth, 1);
    }

    #[test]
    fn test_pipeline_depth_values() {
        assert_eq!(Pipeline::new(1).depth, 1);
        assert_eq!(Pipeline::new(5).depth, 5);
        assert_eq!(Pipeline::new(255).depth, 255);
    }

    #[test]
    fn test_pipeline_empty_after_creation() {
        let pipe = Pipeline::new(10);
        assert!(pipe.is_empty());
        assert_eq!(pipe.in_flight_count(), 0);
        assert_eq!(pipe.pending_count(), 0);
    }

    // -----------------------------------------------------------------------
    // Pipeline integration tests with mock server
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pipeline_process_all_single_article() {
        let mut articles = HashMap::new();
        articles.insert("pipe1@test".into(), b"Pipeline article 1".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut pipeline = Pipeline::new(1);
        pipeline.submit("pipe1@test".into(), 1);

        let results = pipeline.process_all(&mut conn).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].result.is_ok());
        assert_eq!(results[0].request.tag, 1);
        assert_eq!(results[0].request.message_id, "pipe1@test");

        let resp = results[0].result.as_ref().unwrap();
        assert_eq!(resp.code, 220);
        assert!(resp.data.is_some());
    }

    #[tokio::test]
    async fn test_pipeline_process_all_multiple_articles() {
        let mut articles = HashMap::new();
        articles.insert("p1@test".into(), b"Body 1".to_vec());
        articles.insert("p2@test".into(), b"Body 2".to_vec());
        articles.insert("p3@test".into(), b"Body 3".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut pipeline = Pipeline::new(3);
        pipeline.submit("p1@test".into(), 10);
        pipeline.submit("p2@test".into(), 20);
        pipeline.submit("p3@test".into(), 30);

        let results = pipeline.process_all(&mut conn).await.unwrap();
        assert_eq!(results.len(), 3);

        // All should succeed
        for result in &results {
            assert!(result.result.is_ok());
            assert_eq!(result.result.as_ref().unwrap().code, 220);
        }

        // Tags should be in order
        assert_eq!(results[0].request.tag, 10);
        assert_eq!(results[1].request.tag, 20);
        assert_eq!(results[2].request.tag, 30);
    }

    #[tokio::test]
    async fn test_pipeline_with_missing_articles() {
        let mut articles = HashMap::new();
        articles.insert("exists@test".into(), b"Found it".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut pipeline = Pipeline::new(3);
        pipeline.submit("exists@test".into(), 1);
        pipeline.submit("missing@test".into(), 2);
        pipeline.submit("exists@test".into(), 3);

        let results = pipeline.process_all(&mut conn).await.unwrap();
        assert_eq!(results.len(), 3);

        // First: success
        assert!(results[0].result.is_ok());
        // Second: article not found
        assert!(matches!(
            &results[1].result,
            Err(NntpError::ArticleNotFound(_))
        ));
        // Third: success (connection should recover from 430)
        assert!(results[2].result.is_ok());
    }

    #[tokio::test]
    async fn test_pipeline_depth_one_sequential() {
        let mut articles = HashMap::new();
        articles.insert("seq1@test".into(), b"Sequential 1".to_vec());
        articles.insert("seq2@test".into(), b"Sequential 2".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        // Depth 1 = no pipelining, send-receive-send-receive
        let mut pipeline = Pipeline::new(1);
        pipeline.submit("seq1@test".into(), 1);
        pipeline.submit("seq2@test".into(), 2);

        let results = pipeline.process_all(&mut conn).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].result.is_ok());
        assert!(results[1].result.is_ok());
    }

    #[tokio::test]
    async fn test_pipeline_flush_and_receive_manually() {
        let mut articles = HashMap::new();
        articles.insert("manual@test".into(), b"Manual pipeline".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut pipeline = Pipeline::new(5);
        pipeline.submit("manual@test".into(), 1);

        // Manually flush sends
        pipeline.flush_sends(&mut conn).await.unwrap();
        assert_eq!(pipeline.in_flight_count(), 1);
        assert_eq!(pipeline.pending_count(), 0);

        // Manually receive
        let result = pipeline.receive_one(&mut conn).await.unwrap();
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.result.is_ok());
        assert_eq!(result.request.message_id, "manual@test");
    }

    #[tokio::test]
    async fn test_pipeline_receive_with_nothing_in_flight() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut pipeline = Pipeline::new(5);
        // Nothing submitted — receive_one returns None
        let result = pipeline.receive_one(&mut conn).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_pipeline_empty_process_all() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut pipeline = Pipeline::new(5);
        let results = pipeline.process_all(&mut conn).await.unwrap();
        assert!(results.is_empty());
    }

    // -----------------------------------------------------------------------
    // StatPipeline integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_stat_pipeline_all_exist() {
        let mut articles = HashMap::new();
        articles.insert("s1@test".into(), b"data".to_vec());
        articles.insert("s2@test".into(), b"data".to_vec());
        articles.insert("s3@test".into(), b"data".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut stat = StatPipeline::new();
        stat.add("s1@test".into());
        stat.add("s2@test".into());
        stat.add("s3@test".into());
        assert_eq!(stat.len(), 3);

        let results = stat.execute(&mut conn).await.unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.exists));
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[tokio::test]
    async fn test_stat_pipeline_mixed_results() {
        let mut articles = HashMap::new();
        articles.insert("found@test".into(), b"data".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut stat = StatPipeline::new();
        stat.add("found@test".into());
        stat.add("missing@test".into());
        stat.add("found@test".into());

        let results = stat.execute(&mut conn).await.unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].exists);
        assert!(!results[1].exists);
        assert!(results[2].exists);
    }

    #[tokio::test]
    async fn test_stat_pipeline_none_exist() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut stat = StatPipeline::new();
        stat.add("no1@test".into());
        stat.add("no2@test".into());

        let results = stat.execute(&mut conn).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(!results[0].exists);
        assert!(!results[1].exists);
    }

    #[tokio::test]
    async fn test_stat_pipeline_empty() {
        let server = MockNntpServer::start(MockConfig::default()).await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut stat = StatPipeline::new();
        assert!(stat.is_empty());

        let results = stat.execute(&mut conn).await.unwrap();
        assert!(results.is_empty());
        assert_eq!(conn.state, ConnectionState::Ready);
    }

    #[test]
    fn test_stat_pipeline_default() {
        let stat = StatPipeline::default();
        assert!(stat.is_empty());
        assert_eq!(stat.len(), 0);
    }

    #[tokio::test]
    async fn test_stat_pipeline_with_bracketed_ids() {
        let mut articles = HashMap::new();
        articles.insert("bracketed@test".into(), b"data".to_vec());

        let server = MockNntpServer::start(MockConfig {
            articles,
            ..MockConfig::default()
        })
        .await;
        let config = test_config(server.port());
        let mut conn = NntpConnection::new("test".into());
        conn.connect(&config).await.unwrap();

        let mut stat = StatPipeline::new();
        stat.add("<bracketed@test>".into()); // already has brackets
        stat.add("bracketed@test".into()); // without brackets

        let results = stat.execute(&mut conn).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].exists);
        assert!(results[1].exists);
    }
}
