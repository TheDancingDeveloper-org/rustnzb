use crate::config::{ARTICLE_SIZE, MSG_ID_DOMAIN};
use crate::yenc;
use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;

/// Statistics tracked by the synthetic NNTP server.
struct ServerStats {
    articles_served: AtomicU64,
    bytes_served: AtomicU64,
    connections_total: AtomicU64,
    connections_active: AtomicU64,
}

impl ServerStats {
    fn new() -> Self {
        Self {
            articles_served: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
            connections_total: AtomicU64::new(0),
            connections_active: AtomicU64::new(0),
        }
    }
}

/// Parsed message-ID components for synthetic article generation.
/// Format: `stress-{total_size}-f{file_idx}-p{part}@benchnzb`
struct ParsedMsgId {
    total_size: u64,
    file_idx: u32,
    part: u32,
    filename: String,
}

fn parse_message_id(msg_id: &str) -> Option<ParsedMsgId> {
    let stripped = msg_id.trim_matches(|c| c == '<' || c == '>');
    let without_domain = stripped.strip_suffix(&format!("@{MSG_ID_DOMAIN}"))?;

    // Format: stress-{total_size}-f{file_idx}-p{part}
    let rest = without_domain.strip_prefix("stress-")?;
    let (size_and_file, part_str) = rest.rsplit_once("-p")?;
    let part: u32 = part_str.parse().ok()?;

    let (size_str, file_str) = size_and_file.rsplit_once("-f")?;
    let total_size: u64 = size_str.parse().ok()?;
    let file_idx: u32 = file_str.parse().ok()?;

    let filename = format!("stress_f{file_idx:06}.bin");

    Some(ParsedMsgId {
        total_size,
        file_idx,
        part,
        filename,
    })
}

/// Generate deterministic article data from message-ID components.
/// Uses a simple but fast deterministic fill — we don't need cryptographic
/// randomness, just unique-enough bytes that yEnc encodes realistically.
fn generate_article_data(parsed: &ParsedMsgId) -> Vec<u8> {
    let offset = (parsed.part as u64 - 1) * ARTICLE_SIZE;
    let length = std::cmp::min(ARTICLE_SIZE, parsed.total_size - offset) as usize;

    let mut data = vec![0u8; length];

    // Deterministic fill seeded from file_idx + part.
    // Uses a simple xorshift-like pattern for speed.
    let mut state: u64 = (parsed.file_idx as u64)
        .wrapping_mul(0x517cc1b727220a95)
        .wrapping_add(parsed.part as u64)
        .wrapping_mul(0x6c62272e07bb0142);
    if state == 0 {
        state = 1;
    }

    for chunk in data.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        let len = chunk.len().min(8);
        chunk[..len].copy_from_slice(&bytes[..len]);
    }

    data
}

async fn handle_connection(stream: tokio::net::TcpStream, stats: Arc<ServerStats>) {
    stats.connections_total.fetch_add(1, Ordering::Relaxed);
    stats.connections_active.fetch_add(1, Ordering::Relaxed);

    let peer = stream.peer_addr().ok();
    if let Err(e) = handle_connection_inner(stream, &stats).await {
        tracing::debug!("Connection from {:?} ended: {e}", peer);
    }

    stats.connections_active.fetch_sub(1, Ordering::Relaxed);
}

async fn handle_connection_inner(stream: tokio::net::TcpStream, stats: &ServerStats) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    writer
        .write_all(b"200 synth-nntp benchnzb-v2 ready\r\n")
        .await?;
    writer.flush().await?;

    let mut line = String::new();
    loop {
        line.clear();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            reader.read_line(&mut line),
        )
        .await??;
        if n == 0 {
            break;
        }

        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }

        let upper = cmd.to_uppercase();

        if upper.starts_with("AUTHINFO USER") {
            writer.write_all(b"381 PASS required\r\n").await?;
        } else if upper.starts_with("AUTHINFO PASS") {
            writer.write_all(b"281 Authentication accepted\r\n").await?;
        } else if upper.starts_with("GROUP") {
            writer
                .write_all(b"211 1000000 1 1000000 alt.binaries.test\r\n")
                .await?;
        } else if upper.starts_with("BODY") || upper.starts_with("ARTICLE") {
            let is_article = upper.starts_with("ARTICLE");
            let msg_id = extract_message_id(cmd);

            if let Some(parsed) = parse_message_id(&msg_id) {
                // Validate part is within range
                let total_parts = parsed.total_size.div_ceil(ARTICLE_SIZE) as u32;
                if parsed.part < 1 || parsed.part > total_parts {
                    writer.write_all(b"430 No Such Article\r\n").await?;
                } else {
                    let data = generate_article_data(&parsed);
                    let offset = (parsed.part as u64 - 1) * ARTICLE_SIZE;

                    let code = if is_article { "220" } else { "222" };
                    writer
                        .write_all(format!("{code} 0 <{msg_id}>\r\n").as_bytes())
                        .await?;

                    if is_article {
                        writer
                            .write_all(
                                format!(
                                    "From: bench@benchnzb\r\n\
                                     Subject: {} ({}/{})\r\n\
                                     Message-ID: <{msg_id}>\r\n\
                                     Newsgroups: alt.binaries.test\r\n\
                                     \r\n",
                                    parsed.filename, parsed.part, total_parts
                                )
                                .as_bytes(),
                            )
                            .await?;
                    }

                    let (encoded, _crc) = yenc::encode_article(
                        &data,
                        &parsed.filename,
                        parsed.part,
                        total_parts,
                        offset,
                        parsed.total_size,
                    );

                    stats.articles_served.fetch_add(1, Ordering::Relaxed);
                    stats
                        .bytes_served
                        .fetch_add(data.len() as u64, Ordering::Relaxed);

                    writer.write_all(&encoded).await?;
                    writer.write_all(b".\r\n").await?;
                }
            } else {
                writer.write_all(b"430 No Such Article\r\n").await?;
            }
        } else if upper.starts_with("STAT") {
            let msg_id = extract_message_id(cmd);
            if parse_message_id(&msg_id).is_some() {
                writer
                    .write_all(format!("223 0 <{msg_id}>\r\n").as_bytes())
                    .await?;
            } else {
                writer.write_all(b"430 No Such Article\r\n").await?;
            }
        } else if upper.starts_with("CAPABILITIES") {
            writer
                .write_all(b"101 Capability list:\r\nVERSION 2\r\nAUTHINFO USER\r\nREADER\r\n.\r\n")
                .await?;
        } else if upper.starts_with("MODE READER") {
            writer
                .write_all(b"200 Reader mode acknowledged\r\n")
                .await?;
        } else if upper.starts_with("DATE") {
            let now = chrono::Utc::now().format("%Y%m%d%H%M%S");
            writer
                .write_all(format!("111 {now}\r\n").as_bytes())
                .await?;
        } else if upper.starts_with("QUIT") {
            writer.write_all(b"205 Goodbye\r\n").await?;
            writer.flush().await?;
            break;
        } else {
            writer
                .write_all(
                    format!(
                        "500 Unknown command: {}\r\n",
                        cmd.split_whitespace().next().unwrap_or("")
                    )
                    .as_bytes(),
                )
                .await?;
        }

        writer.flush().await?;
    }

    Ok(())
}

fn extract_message_id(cmd: &str) -> String {
    let arg = cmd.split_whitespace().nth(1).unwrap_or("");
    arg.trim_matches(|c| c == '<' || c == '>').to_string()
}

pub async fn run(port: u16, health_port: u16) -> Result<()> {
    let stats = Arc::new(ServerStats::new());

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("Synthetic NNTP server listening on 0.0.0.0:{port}");
    tracing::info!("  Article size: {} bytes", ARTICLE_SIZE);
    tracing::info!("  Accepts any message-ID: stress-{{size}}-f{{idx}}-p{{part}}@{MSG_ID_DOMAIN}");

    // Health/stats server
    let stats_clone = stats.clone();
    tokio::spawn(async move {
        run_control_server(health_port, stats_clone).await;
    });

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let stats = stats.clone();
                tokio::spawn(handle_connection(stream, stats));
            }
            Err(e) => {
                tracing::warn!("NNTP accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn run_control_server(port: u16, stats: Arc<ServerStats>) {
    use tokio::io::AsyncReadExt;

    let listener = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    tracing::info!("Control server on port {port} (/health, /stats)");

    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let stats = stats.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");

            let response = match path {
                "/health" => {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nOK"
                        .to_string()
                }
                "/stats" => {
                    let body = format!(
                        "{{\"articles_served\":{},\"bytes_served\":{},\"connections_total\":{},\"connections_active\":{}}}",
                        stats.articles_served.load(Ordering::Relaxed),
                        stats.bytes_served.load(Ordering::Relaxed),
                        stats.connections_total.load(Ordering::Relaxed),
                        stats.connections_active.load(Ordering::Relaxed),
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
            };
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}
