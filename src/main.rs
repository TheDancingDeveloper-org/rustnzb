use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use nzb_web::nzb_core::config::AppConfig;
use nzb_web::{LogBuffer, LogBufferLayer, StartupConfig};

use rustnzb::handlers;

#[derive(Parser, Debug)]
#[command(name = "rustnzb", version, about = "Usenet NZB download client")]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml", env = "RUSTNZB_CONFIG")]
    config: PathBuf,

    /// Override listen address
    #[arg(long, env = "RUSTNZB_LISTEN_ADDR")]
    listen_addr: Option<String>,

    /// Override listen port
    #[arg(short, long, env = "RUSTNZB_PORT")]
    port: Option<u16>,

    /// Override data directory
    #[arg(long, env = "RUSTNZB_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", env = "RUSTNZB_LOG_LEVEL")]
    log_level: String,

    /// Log file path
    #[arg(long, env = "RUSTNZB_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// Run smoke tests to verify external tools (7z, optional unrar) work, then exit
    #[arg(long)]
    smoke_test: bool,
}

fn init_otel_logging(
    endpoint: &str,
    service_name: &str,
) -> Option<opentelemetry_sdk::logs::SdkLoggerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::LogExporter;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::logs::SdkLoggerProvider;

    let exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .ok()?;

    let provider = SdkLoggerProvider::builder()
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", service_name.to_string())])
                .build(),
        )
        .with_batch_exporter(exporter)
        .build();

    Some(provider)
}

fn init_otel_metrics(
    endpoint: &str,
    service_name: &str,
) -> Option<opentelemetry_sdk::metrics::SdkMeterProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::MetricExporter;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::metrics::PeriodicReader;
    use opentelemetry_sdk::metrics::SdkMeterProvider;

    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .ok()?;

    let reader = PeriodicReader::builder(exporter)
        .with_interval(std::time::Duration::from_secs(15))
        .build();

    let provider = SdkMeterProvider::builder()
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", service_name.to_string())])
                .build(),
        )
        .with_reader(reader)
        .build();

    Some(provider)
}

/// Verify that all external tools work in the current environment.
/// Returns 0 on success, 1 on failure.
fn run_smoke_tests() -> i32 {
    use std::process::Command;

    let mut passed = 0u32;
    let mut failed = 0u32;

    // --- rust-par2 (native library) ---
    print!("rust-par2       ... ");
    println!("OK (native library, no external binary needed)");
    passed += 1;

    // --- unrar (optional — 7z handles RAR extraction as fallback) ---
    print!("unrar           ... ");
    match Command::new("unrar").output() {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            if text.to_lowercase().contains("unrar") {
                println!("OK");
                passed += 1;
            } else {
                println!("SKIP (not found, 7z will handle RAR files)");
            }
        }
        Err(_) => {
            println!("SKIP (not found, 7z will handle RAR files)");
        }
    }

    // --- 7z ---
    print!("7z              ... ");
    match Command::new("7z").output() {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            if text.contains("7-Zip") {
                println!("OK");
                passed += 1;
            } else {
                println!("FAIL - ran but output unexpected");
                failed += 1;
            }
        }
        Err(e) => {
            println!("FAIL - {e}");
            failed += 1;
        }
    }

    println!("\n{passed} passed, {failed} failed");
    if failed > 0 { 1 } else { 0 }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the rustls crypto provider before any TLS operations.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    let args = Args::parse();

    if args.smoke_test {
        std::process::exit(run_smoke_tests());
    }

    // Load config early to check OTEL settings before initializing tracing
    let mut config = AppConfig::load(&args.config)?;
    // Strip incidental whitespace from user-supplied server fields. Guards
    // against pasted hostnames carrying a trailing newline/space, which
    // surfaces as a misleading "Name does not resolve" from getaddrinfo.
    for srv in config.servers.iter_mut() {
        handlers::sanitize_server_config(srv);
    }

    // Initialize logging (must happen before startup::initialize)
    let log_buffer = LogBuffer::new();
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);
    let log_layer = LogBufferLayer::new(log_buffer.clone());

    let _otel_log_provider;
    let _otel_meter_provider;

    if config.otel.enabled {
        eprintln!(
            "OpenTelemetry enabled: endpoint={}, service={}",
            config.otel.endpoint, config.otel.service_name
        );

        _otel_log_provider = init_otel_logging(&config.otel.endpoint, &config.otel.service_name);
        _otel_meter_provider = init_otel_metrics(&config.otel.endpoint, &config.otel.service_name);

        if let Some(ref mp) = _otel_meter_provider {
            opentelemetry::global::set_meter_provider(mp.clone());
        }

        if let Some(ref lp) = _otel_log_provider {
            let otel_log_layer =
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(lp);
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(log_layer)
                .with(otel_log_layer)
                .init();
        } else {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(log_layer)
                .init();
        }
    } else {
        _otel_log_provider = None;
        _otel_meter_provider = None;

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .with(log_layer)
            .init();
    }

    info!("rustnzb v{}", env!("CARGO_PKG_VERSION"));

    // Initialize the engine (config, DB, queue manager, background services)
    let result = nzb_web::startup::initialize(
        StartupConfig {
            config_path: args.config,
            listen_addr: args.listen_addr,
            port: args.port,
            data_dir: args.data_dir,
            log_level: Some(args.log_level),
        },
        Some(log_buffer),
    )
    .await?;

    // Spawn OTEL metrics reporter if enabled
    if config.otel.enabled && _otel_meter_provider.is_some() {
        let qm = Arc::clone(&result.queue_manager);
        tokio::spawn(async move {
            let meter = opentelemetry::global::meter("rustnzb");
            let speed_gauge = meter.f64_gauge("download.speed_bps").build();
            let queue_gauge = meter.u64_gauge("queue.depth").build();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                speed_gauge.record(qm.get_speed() as f64, &[]);
                queue_gauge.record(qm.queue_size() as u64, &[]);
            }
        });
        info!("OpenTelemetry metrics reporter started");
    }

    // Start HTTP server
    info!("Starting HTTP API server");

    #[cfg(feature = "webdav")]
    {
        use axum::Extension;
        use std::sync::Arc;

        let servers = result.queue_manager.get_servers();
        let data_dir = result.state.config().general.data_dir.clone();
        let dav_handle: Option<Arc<rustnzb::dav::DavHandle>> =
            rustnzb::dav::DavHandle::init(&data_dir, servers)
                .await
                .inspect_err(|e| tracing::warn!("WebDAV init failed, running without it: {e}"))
                .ok()
                .map(Arc::new);

        // Spawn background task: auto-send completed downloads to DAV pipeline
        // when DavConfig.auto_send_all or category_rules matches.
        if let Some(ref dav) = dav_handle {
            let dav_clone = Arc::clone(dav);
            let state_clone = result.state.clone();
            let mut completions = result.queue_manager.subscribe_completions();
            tokio::spawn(async move {
                loop {
                    match completions.recv().await {
                        Ok(event) => {
                            if event.status != nzb_web::nzb_core::models::JobStatus::Completed {
                                continue;
                            }
                            let dav_cfg = state_clone.config().dav.clone();
                            let should_send = dav_cfg.auto_send_all
                                || dav_cfg.category_rules.contains(&event.category);
                            if !should_send {
                                continue;
                            }
                            // Look up NZB data from history.
                            let nzb_data = match state_clone
                                .queue_manager
                                .history_get_nzb_data(&event.id)
                            {
                                Ok(Some(d)) => d,
                                Ok(None) => {
                                    tracing::warn!(
                                        job = %event.name,
                                        "DAV auto-send: no NZB data retained"
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    tracing::warn!(job = %event.name, error = %e, "DAV auto-send: DB error");
                                    continue;
                                }
                            };
                            let file_name = format!("{}.nzb", event.name);
                            if let Err(e) = dav_clone
                                .enqueue_nzb(&file_name, &event.name, &nzb_data)
                                .await
                            {
                                tracing::warn!(job = %event.name, error = %e, "DAV auto-send: enqueue failed");
                            } else {
                                tracing::info!(job = %event.name, "DAV auto-send: queued");
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("DAV auto-send: missed {n} completion events (lagged)");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let router = {
            let mut r = rustnzb::server::build_router(result.state.clone());
            if let Some(ref dav) = dav_handle {
                r = r.nest("/dav", nzbdav_dav::dav_router(Arc::clone(&dav.store)));
                info!("WebDAV media library mounted at /dav");
            }
            // Always layer Option<Arc<DavHandle>> so h_status and h_dav_add can extract it.
            r.layer(Extension(dav_handle))
        };

        rustnzb::server::serve(result.state, router).await?;
    }

    #[cfg(not(feature = "webdav"))]
    {
        rustnzb::server::run(result.state).await?;
    }

    Ok(())
}
