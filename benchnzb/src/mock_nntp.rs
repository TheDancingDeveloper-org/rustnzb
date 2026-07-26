use crate::config::{ARTICLE_SIZE, MSG_ID_DOMAIN};
use crate::yenc;
use anyhow::Result;
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleIndex {
    pub article_size: u64,
    pub entries: Vec<FileEntry>,
    #[serde(default)]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub msg_prefix: String,
    pub data_file: String,
    pub filename: String,
    pub total_size: u64,
}

/// A memory-mapped data file. The mmap is kept alive by the Arc.
struct MappedFile {
    mmap: Option<Mmap>,
    filename: String,
    total_size: u64,
}

struct ServerState {
    index: ArticleIndex,
    prefix_map: HashMap<String, usize>,
    /// mmap'd data files, indexed same as index.entries
    mapped_files: Vec<Arc<MappedFile>>,
    missing: HashSet<String>,
    data_dir: PathBuf,
    stats: Arc<ServerStats>,
}

#[derive(Default)]
struct ServerStats {
    /// BODY / ARTICLE requests, including deterministic 430 responses. This
    /// makes duplicate fetches and retries visible without client-side speed
    /// inference.
    article_requests: AtomicU64,
    articles_served: AtomicU64,
    payload_bytes_served: AtomicU64,
    /// Bytes in generated yEnc bodies. This excludes NNTP response headers,
    /// so it is reproducible while still exposing encoding overhead.
    wire_bytes_served: AtomicU64,
    article_not_found: AtomicU64,
}

impl ServerStats {
    fn reset(&self) {
        self.article_requests.store(0, Ordering::Relaxed);
        self.articles_served.store(0, Ordering::Relaxed);
        self.payload_bytes_served.store(0, Ordering::Relaxed);
        self.wire_bytes_served.store(0, Ordering::Relaxed);
        self.article_not_found.store(0, Ordering::Relaxed);
    }
}

impl ServerState {
    fn load(data_dir: &Path) -> Result<Self> {
        let idx_path = data_dir.join("articles.json");
        let index: ArticleIndex = if idx_path.exists() {
            let data = std::fs::read_to_string(&idx_path)?;
            serde_json::from_str(&data)?
        } else {
            ArticleIndex {
                article_size: ARTICLE_SIZE,
                entries: vec![],
                missing: vec![],
            }
        };

        let mut prefix_map = HashMap::new();
        let mut mapped_files = Vec::new();
        let mut total_mapped: u64 = 0;

        for (i, entry) in index.entries.iter().enumerate() {
            prefix_map.insert(entry.msg_prefix.clone(), i);

            let file_path = Path::new(&entry.data_file);
            if file_path.exists() {
                let file = std::fs::File::open(file_path)?;
                let mmap = unsafe { Mmap::map(&file)? };
                mmap.advise(memmap2::Advice::Sequential)?;
                total_mapped += mmap.len() as u64;
                mapped_files.push(Arc::new(MappedFile {
                    mmap: Some(mmap),
                    filename: entry.filename.clone(),
                    total_size: entry.total_size,
                }));
            } else {
                mapped_files.push(Arc::new(MappedFile {
                    mmap: None,
                    filename: entry.filename.clone(),
                    total_size: 0,
                }));
            }
        }

        let missing: HashSet<String> = index.missing.iter().cloned().collect();

        tracing::info!(
            "Loaded {} file entries, {} missing articles, {:.1} GB mmap'd",
            index.entries.len(),
            missing.len(),
            total_mapped as f64 / (1024.0 * 1024.0 * 1024.0),
        );

        Ok(Self {
            index,
            prefix_map,
            mapped_files,
            missing,
            data_dir: data_dir.to_path_buf(),
            stats: Arc::new(ServerStats::default()),
        })
    }

    fn reload(&mut self) -> Result<()> {
        let new = Self::load(&self.data_dir)?;
        *self = new;
        Ok(())
    }

    /// Lookup article by message-id.
    /// Returns (mapped_file, offset, length, part, total_parts) — all from mmap, no I/O.
    fn lookup(&self, message_id: &str) -> Option<(Arc<MappedFile>, u64, u64, u32, u32)> {
        let stripped = message_id.trim_matches(|c| c == '<' || c == '>');
        let without_domain = stripped.strip_suffix(&format!("@{MSG_ID_DOMAIN}"))?;
        let (prefix, part_str) = without_domain.rsplit_once("-p")?;
        let part: u32 = part_str.parse().ok()?;

        let entry_idx = self.prefix_map.get(prefix)?;
        let mf = &self.mapped_files[*entry_idx];

        mf.mmap.as_ref()?;

        let article_size = self.index.article_size;
        let offset = (part as u64 - 1) * article_size;
        if offset >= mf.total_size {
            return None;
        }
        let length = std::cmp::min(article_size, mf.total_size - offset);
        let total_parts = mf.total_size.div_ceil(article_size) as u32;

        Some((mf.clone(), offset, length, part, total_parts))
    }

    fn is_missing(&self, message_id: &str) -> bool {
        let stripped = message_id.trim_matches(|c| c == '<' || c == '>');
        self.missing.contains(stripped)
    }
}

async fn handle_connection(stream: tokio::net::TcpStream, state: Arc<RwLock<ServerState>>) {
    let peer = stream.peer_addr().ok();
    if let Err(e) = handle_connection_inner(stream, state).await {
        tracing::debug!("Connection from {:?} ended: {e}", peer);
    }
}

async fn handle_connection_inner(
    stream: tokio::net::TcpStream,
    state: Arc<RwLock<ServerState>>,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    writer
        .write_all(b"200 mock-nntp benchnzb ready\r\n")
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

            let st = state.read().await;
            let stats = st.stats.clone();
            stats.article_requests.fetch_add(1, Ordering::Relaxed);
            if st.is_missing(&msg_id) {
                stats.article_not_found.fetch_add(1, Ordering::Relaxed);
                drop(st);
                writer.write_all(b"430 No Such Article\r\n").await?;
            } else if let Some((mf, offset, length, part, total_parts)) = st.lookup(&msg_id) {
                stats.articles_served.fetch_add(1, Ordering::Relaxed);
                stats
                    .payload_bytes_served
                    .fetch_add(length, Ordering::Relaxed);
                drop(st);

                // Slice directly from mmap — no file open, seek, or read syscalls
                let mmap = mf.mmap.as_ref().unwrap();
                let data = &mmap[offset as usize..(offset + length) as usize];

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
                                mf.filename, part, total_parts
                            )
                            .as_bytes(),
                        )
                        .await?;
                }

                // yEnc encode from mmap slice and write
                let (encoded, _crc) = yenc::encode_article(
                    data,
                    &mf.filename,
                    part,
                    total_parts,
                    offset,
                    mf.total_size,
                );
                stats
                    .wire_bytes_served
                    .fetch_add(encoded.len() as u64, Ordering::Relaxed);
                writer.write_all(&encoded).await?;
                writer.write_all(b".\r\n").await?;
            } else {
                drop(st);
                writer.write_all(b"430 No Such Article\r\n").await?;
            }
        } else if upper.starts_with("STAT") {
            let msg_id = extract_message_id(cmd);
            let st = state.read().await;
            if st.is_missing(&msg_id) {
                st.stats.article_not_found.fetch_add(1, Ordering::Relaxed);
                writer.write_all(b"430 No Such Article\r\n").await?;
            } else if st.lookup(&msg_id).is_some() {
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

pub async fn run(port: u16, data_dir: PathBuf, health_port: u16) -> Result<()> {
    let state = Arc::new(RwLock::new(ServerState::load(&data_dir)?));

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("Mock NNTP server listening on 0.0.0.0:{port}");

    // Health/control server
    let state_clone = state.clone();
    let data_dir_clone = data_dir.clone();
    tokio::spawn(async move {
        run_control_server(health_port, state_clone, data_dir_clone).await;
    });

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                tokio::spawn(handle_connection(stream, state));
            }
            Err(e) => {
                tracing::warn!("NNTP accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn run_control_server(port: u16, state: Arc<RwLock<ServerState>>, _data_dir: PathBuf) {
    let listener = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    tracing::info!("Control server on port {port} (/health, /status, /reload)");

    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();

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
                "/status" => {
                    let st = state.read().await;
                    let body = format!(
                        "{{\"entries\":{},\"missing\":{},\"article_requests\":{},\"articles_served\":{},\"payload_bytes_served\":{},\"wire_bytes_served\":{},\"article_not_found\":{}}}",
                        st.index.entries.len(),
                        st.missing.len(),
                        st.stats.article_requests.load(Ordering::Relaxed),
                        st.stats.articles_served.load(Ordering::Relaxed),
                        st.stats.payload_bytes_served.load(Ordering::Relaxed),
                        st.stats.wire_bytes_served.load(Ordering::Relaxed),
                        st.stats.article_not_found.load(Ordering::Relaxed),
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                }
                "/reload" => {
                    let mut st = state.write().await;
                    match st.reload() {
                        Ok(()) => {
                            let body = format!(
                                "{{\"entries\":{},\"missing\":{},\"article_requests\":{},\"articles_served\":{},\"payload_bytes_served\":{},\"wire_bytes_served\":{},\"article_not_found\":{}}}",
                                st.index.entries.len(),
                                st.missing.len(),
                                st.stats.article_requests.load(Ordering::Relaxed),
                                st.stats.articles_served.load(Ordering::Relaxed),
                                st.stats.payload_bytes_served.load(Ordering::Relaxed),
                                st.stats.wire_bytes_served.load(Ordering::Relaxed),
                                st.stats.article_not_found.load(Ordering::Relaxed),
                            );
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            )
                        }
                        Err(e) => {
                            let body = format!("{{\"error\":\"{e}\"}}");
                            format!(
                                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            )
                        }
                    }
                }
                "/reset-stats" => {
                    let st = state.read().await;
                    st.stats.reset();
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_string()
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
            };
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}
