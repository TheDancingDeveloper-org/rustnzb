use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use nzb_web::nzb_core::config::ServerConfig;
use nzbdav_core::database::DavDatabase;
use nzbdav_core::models::{DownloadStatus, HistoryItem, QueueItem};
use nzbdav_core::sqlite_db::SqliteDavDatabase;
use nzbdav_dav::store::DatabaseStore;
use nzbdav_pipeline::queue_item_processor::{PipelineConfig, QueueItemProcessor};
use nzbdav_stream::UsenetArticleProvider;
use nzbdav_stream::nzb_nntp::ConnectionPool;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

pub struct DavHandle {
    pub store: Arc<DatabaseStore>,
    /// Persistent connection reused by enqueue_nzb / pipeline_status; the queue
    /// loop uses its own connections to avoid blocking the main runtime.
    db: Arc<SqliteDavDatabase>,
    db_path: String,
    cancel: CancellationToken,
    _thread: std::thread::JoinHandle<()>,
}

impl DavHandle {
    pub async fn init(data_dir: &Path, servers: Vec<ServerConfig>) -> anyhow::Result<Self> {
        let db_path = data_dir.join("nzbdav.db").to_string_lossy().to_string();

        // Open the DAV SQLite DB and seed root directories.
        let conn = nzbdav_core::db::open(&db_path)
            .with_context(|| format!("opening nzbdav DB at {db_path}"))?;
        let dav_db: Arc<SqliteDavDatabase> =
            Arc::new(SqliteDavDatabase::new(Arc::new(Mutex::new(conn))));
        nzbdav_core::seed::seed_root_items(&*dav_db).await?;

        // Retain a reference for enqueue_nzb / pipeline_status before passing
        // ownership into DatabaseStore.
        let db_for_handle = Arc::clone(&dav_db);

        // Build NNTP pools from rustnzbd's server configs (same nzb-nntp types).
        let pools: Vec<Arc<ConnectionPool>> = servers
            .into_iter()
            .map(|s| Arc::new(ConnectionPool::new(Arc::new(s))))
            .collect();
        let provider = Arc::new(UsenetArticleProvider::new(pools));

        let store = Arc::new(DatabaseStore::new(
            dav_db,
            Arc::clone(&provider),
            3, // lookahead segments
        ));

        let cancel = CancellationToken::new();
        let thread = {
            let db_path = db_path.clone();
            let cancel = cancel.clone();
            let processor = Arc::new(QueueItemProcessor::new(
                Arc::clone(&provider),
                PipelineConfig::default(),
            ));
            std::thread::Builder::new()
                .name("dav-queue".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("dav-queue runtime");
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&rt, async move {
                        run_queue_loop(db_path, processor, cancel).await;
                    });
                })
                .context("spawning dav-queue thread")?
        };

        info!("WebDAV media library initialised (db: {db_path})");
        Ok(Self {
            store,
            db: db_for_handle,
            db_path,
            cancel,
            _thread: thread,
        })
    }

    /// Queue an NZB for DAV pipeline processing.
    /// Returns the new queue item UUID which can be used to track status.
    pub async fn enqueue_nzb(
        &self,
        file_name: &str,
        job_name: &str,
        nzb_data: &[u8],
    ) -> anyhow::Result<Uuid> {
        let db = &*self.db;
        let item_id = Uuid::new_v4();
        let item = QueueItem {
            id: item_id,
            created_at: Utc::now().naive_utc(),
            file_name: file_name.to_string(),
            job_name: job_name.to_string(),
            nzb_file_size: nzb_data.len() as i64,
            total_segment_bytes: 0,
            category: String::new(),
            priority: 0,
            post_processing: 0,
            pause_until: None,
        };
        db.put_nzb_blob(item_id, nzb_data).await?;
        db.insert_queue_item(&item).await?;

        info!(job_name, "queued for DAV pipeline");
        Ok(item_id)
    }

    /// Return the current state of the DAV pipeline: queued items + history.
    pub async fn pipeline_status(&self) -> anyhow::Result<DavPipelineStatus> {
        let db = &*self.db;
        let queue = db.list_queue_items().await?;
        let history = db.list_history_items(0, 200).await?;
        Ok(DavPipelineStatus { queue, history })
    }
}

pub struct DavPipelineStatus {
    pub queue: Vec<QueueItem>,
    pub history: Vec<HistoryItem>,
}

impl Drop for DavHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn open_db(db_path: &str) -> anyhow::Result<SqliteDavDatabase> {
    let conn = nzbdav_core::db::open(db_path)
        .with_context(|| format!("opening nzbdav DB at {db_path}"))?;
    Ok(SqliteDavDatabase::new(Arc::new(Mutex::new(conn))))
}

async fn run_queue_loop(
    db_path: String,
    processor: Arc<QueueItemProcessor>,
    cancel: CancellationToken,
) {
    const MAX_CONCURRENT: usize = 2;
    const POLL_INTERVAL: Duration = Duration::from_secs(3);

    info!("DAV queue manager started");
    let mut active: tokio::task::JoinSet<Uuid> = tokio::task::JoinSet::new();
    let mut active_ids: HashSet<Uuid> = HashSet::new();

    loop {
        // Reap completed tasks.
        while let Some(result) = active.try_join_next() {
            match result {
                Ok(id) => {
                    active_ids.remove(&id);
                }
                Err(e) => {
                    error!(error = %e, "DAV pipeline task panicked");
                }
            }
        }

        if active.len() < MAX_CONCURRENT
            && let Ok(db) = open_db(&db_path)
        {
            let exclude: Vec<Uuid> = active_ids.iter().copied().collect();
            match db.get_next_queue_item(&exclude).await {
                Ok(Some(item)) => {
                    let item_id = item.id;
                    active_ids.insert(item_id);
                    info!(job_name = %item.job_name, "DAV pipeline starting");
                    let db_path = db_path.clone();
                    let processor = Arc::clone(&processor);
                    active.spawn_local(async move {
                        process_item(&db_path, &processor, item).await;
                        item_id
                    });
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    error!(error = %e, "failed to poll DAV queue");
                }
            }
        }

        tokio::select! {
            () = cancel.cancelled() => break,
            Some(result) = active.join_next() => {
                match result {
                    Ok(id) => { active_ids.remove(&id); }
                    Err(e) => { error!(error = %e, "DAV pipeline task panicked"); }
                }
            }
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }

    // Drain active tasks on shutdown.
    while let Some(result) = active.join_next().await {
        if let Err(e) = result {
            error!(error = %e, "DAV pipeline task panicked during shutdown");
        }
    }
    info!("DAV queue manager stopped");
}

async fn process_item(db_path: &str, processor: &QueueItemProcessor, item: QueueItem) {
    let db = match open_db(db_path) {
        Ok(db) => db,
        Err(e) => {
            error!(error = %e, "DAV: failed to open DB for item processing");
            return;
        }
    };

    let job_name = item.job_name.clone();
    let item_id = item.id;
    let start = Instant::now();

    let nzb_data = match db.get_nzb_blob(item_id).await {
        Ok(data) => data,
        Err(e) => {
            error!(job_name = %job_name, error = %e, "DAV: NZB blob missing");
            finish_item(
                &db,
                &item,
                DownloadStatus::Failed,
                Some(e.to_string()),
                None,
            )
            .await;
            return;
        }
    };

    match processor.process(&db, &item, &nzb_data).await {
        Ok(result) => {
            info!(
                job_name = %job_name,
                items_created = result.items_created,
                elapsed_secs = start.elapsed().as_secs_f32(),
                "DAV pipeline completed"
            );
            finish_item(
                &db,
                &item,
                DownloadStatus::Completed,
                None,
                Some(result.job_dir_id),
            )
            .await;
        }
        Err(e) if e.is_retryable() => {
            warn!(job_name = %job_name, error = %e, "DAV: retryable error — pausing 60s");
            let pause_until = Utc::now().naive_utc() + chrono::Duration::seconds(60);
            if let Err(ue) = db
                .update_queue_pause_until(item_id, Some(pause_until))
                .await
            {
                error!(error = %ue, "DAV: failed to set pause_until");
            }
        }
        Err(e) => {
            error!(job_name = %job_name, error = %e, "DAV: pipeline failed");
            finish_item(
                &db,
                &item,
                DownloadStatus::Failed,
                Some(e.to_string()),
                None,
            )
            .await;
        }
    }
}

async fn finish_item(
    db: &dyn DavDatabase,
    item: &QueueItem,
    status: DownloadStatus,
    fail_message: Option<String>,
    download_dir_id: Option<Uuid>,
) {
    let history = HistoryItem {
        id: Uuid::new_v4(),
        created_at: Utc::now().naive_utc(),
        file_name: item.file_name.clone(),
        job_name: item.job_name.clone(),
        category: item.category.clone(),
        download_status: status,
        total_segment_bytes: item.total_segment_bytes,
        download_time_seconds: 0,
        fail_message,
        download_dir_id,
        nzb_blob_id: Some(item.id),
    };
    if let Err(e) = db.insert_history_item(&history).await {
        error!(error = %e, "DAV: failed to insert history");
    }
    if let Err(e) = db.delete_queue_item(item.id).await {
        error!(error = %e, "DAV: failed to delete queue item");
    }
}
