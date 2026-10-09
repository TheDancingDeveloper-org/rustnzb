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

    /// Log level (trace, debug, info, warn, error). No clap default: when
    /// neither this flag nor RUSTNZB_LOG_LEVEL is given, config.toml's
    /// `general.log_level` applies (falling back to "info").
    #[arg(long, env = "RUSTNZB_LOG_LEVEL")]
    log_level: Option<String>,

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

const LOG_LEVEL_NAMES: [&str; 6] = ["trace", "debug", "info", "warn", "error", "off"];

/// Whether `spec` is a usable log filter: either a bare level name or a full
/// `EnvFilter` directive list (e.g. `info,nzb_nntp=debug`). A bare word that
/// is not a level is rejected, because `EnvFilter` would otherwise read it as
/// a target name and silently enable every level for that target only.
fn is_valid_log_filter(spec: &str) -> bool {
    if EnvFilter::try_new(spec).is_err() {
        return false;
    }
    spec.split(',').map(str::trim).all(|d| {
        d.is_empty()
            || d.contains('=')
            || d.contains('[')
            || LOG_LEVEL_NAMES.iter().any(|l| l.eq_ignore_ascii_case(d))
    })
}

/// Resolve the tracing filter. Precedence, highest first:
///
/// 1. `RUST_LOG` (full filter override)
/// 2. `--log-level`, then `RUSTNZB_LOG_LEVEL` (clap lets the flag win)
/// 3. config.toml `general.log_level`
/// 4. `"info"`
///
/// Empty values count as unset. An invalid value falls back to `"info"`, and
/// an invalid `RUST_LOG` falls through to the next source; either way a
/// warning is returned for the caller to log once tracing is up.
fn resolve_log_filter(
    rust_log: Option<&str>,
    cli_or_env: Option<&str>,
    toml_level: &str,
) -> (String, Option<String>) {
    let mut warning = None;
    if let Some(rust_log) = rust_log.map(str::trim).filter(|s| !s.is_empty()) {
        if EnvFilter::try_new(rust_log).is_ok() {
            return (rust_log.to_string(), None);
        }
        warning = Some(format!("ignoring invalid RUST_LOG filter {rust_log:?}"));
    }
    let chosen = [cli_or_env, Some(toml_level)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty());
    match chosen {
        Some(spec) if is_valid_log_filter(spec) => (spec.to_string(), warning),
        Some(spec) => (
            "info".to_string(),
            Some(format!(
                "invalid log level {spec:?} (expected one of {}); falling back to \"info\"",
                LOG_LEVEL_NAMES.join(", ")
            )),
        ),
        None => ("info".to_string(), warning),
    }
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

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn apply_runtime_config_overrides(config: &mut AppConfig) -> bool {
    let dav_enabled = env_flag_enabled("RUSTNZB_DAV_ENABLED") || env_flag_enabled("ENABLE_DAV");
    if dav_enabled && !config.dav.enabled {
        config.dav.enabled = true;
        return true;
    }
    false
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
    if apply_runtime_config_overrides(&mut config) {
        config.save(&args.config)?;
    }

    // Initialize logging (must happen before startup::initialize)
    let log_buffer = LogBuffer::new();
    let rust_log = std::env::var(EnvFilter::DEFAULT_ENV).ok();
    let (filter_spec, log_filter_warning) = resolve_log_filter(
        rust_log.as_deref(),
        args.log_level.as_deref(),
        &config.general.log_level,
    );
    let filter = EnvFilter::new(&filter_spec);
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);
    let log_layer = LogBufferLayer::new(log_buffer.clone());

    let _otel_log_provider;
    let _otel_meter_provider;

    let otel_logs_enabled = config.otel.logs_enabled();
    let otel_metrics_enabled = config.otel.metrics_enabled();

    if otel_logs_enabled || otel_metrics_enabled {
        eprintln!(
            "OpenTelemetry config: logs_enabled={}, logs_endpoint={}, metrics_enabled={}, metrics_endpoint={}, service={}",
            otel_logs_enabled,
            config.otel.logs_endpoint(),
            otel_metrics_enabled,
            config.otel.metrics_endpoint(),
            config.otel.service_name
        );

        _otel_log_provider = if otel_logs_enabled {
            init_otel_logging(config.otel.logs_endpoint(), &config.otel.service_name)
        } else {
            None
        };
        _otel_meter_provider = if otel_metrics_enabled {
            init_otel_metrics(config.otel.metrics_endpoint(), &config.otel.service_name)
        } else {
            None
        };

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

    info!("rustnzb v{}", env!("RUSTNZB_BUILD_VERSION"));
    if let Some(warning) = log_filter_warning {
        tracing::warn!("{warning}");
    }

    // Initialize the engine (config, DB, queue manager, background services)
    let result = nzb_web::startup::initialize(
        StartupConfig {
            config_path: args.config,
            listen_addr: args.listen_addr,
            port: args.port,
            data_dir: args.data_dir,
            log_level: Some(filter_spec),
        },
        Some(log_buffer),
    )
    .await?;

    // Spawn OTEL metrics reporter if enabled
    if otel_metrics_enabled && _otel_meter_provider.is_some() {
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
        use std::sync::Arc;

        let servers = result.queue_manager.get_servers();
        let data_dir = result.state.config().general.data_dir.clone();
        let dav_enabled = result.state.config().dav.enabled;
        let dav_handle: Option<Arc<rustnzb::dav::DavHandle>> = if dav_enabled {
            rustnzb::dav::DavHandle::init(&data_dir, servers)
                .await
                .inspect_err(|e| tracing::warn!("WebDAV init failed, running without it: {e}"))
                .ok()
                .map(Arc::new)
        } else {
            info!("WebDAV media library disabled");
            None
        };

        // Spawn background task: auto-send jobs to DAV streaming pipeline immediately
        // on add (before download begins) when DavConfig.auto_send_all or
        // category_rules matches. This enables on-demand Usenet streaming from
        // the /dav endpoint without waiting for the download to complete.
        if let Some(ref dav) = dav_handle {
            let dav_clone = Arc::clone(dav);
            let state_clone = result.state.clone();
            let mut additions = result.queue_manager.subscribe_additions();
            tokio::spawn(async move {
                loop {
                    match additions.recv().await {
                        Ok(event) => {
                            let dav_cfg = state_clone.config().dav.clone();
                            let should_send = dav_cfg.auto_send_all
                                || dav_cfg.category_rules.contains(&event.category);
                            if !should_send {
                                continue;
                            }
                            let nzb_data = match event.nzb_data {
                                Some(d) => d,
                                None => {
                                    tracing::warn!(
                                        job = %event.name,
                                        "DAV auto-send: no NZB data at add time"
                                    );
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
                                tracing::info!(job = %event.name, "DAV auto-send: queued for streaming");
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("DAV auto-send: missed {n} add events (lagged)");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let router = rustnzb::server::build_router_with_dav(result.state.clone(), dav_handle);

        rustnzb::server::serve(result.state, router).await?;
    }

    #[cfg(not(feature = "webdav"))]
    {
        rustnzb::server::run(result.state).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Args, resolve_log_filter};
    use clap::Parser;
    use serial_test::serial;

    // These tests mutate the real process environment, so they must run
    // serialized against each other (see issue #62 follow-up: the Docker
    // image used to hardcode `--port 9090`, which silently defeated
    // RUSTNZB_PORT — this locks in the precedence the fix relies on).

    #[test]
    #[serial(rustnzb_port_env)]
    fn port_env_var_is_used_when_no_explicit_flag_is_given() {
        unsafe {
            std::env::set_var("RUSTNZB_PORT", "8123");
        }
        let args = Args::parse_from(["rustnzb"]);
        unsafe {
            std::env::remove_var("RUSTNZB_PORT");
        }

        assert_eq!(args.port, Some(8123));
    }

    #[test]
    #[serial(rustnzb_port_env)]
    fn explicit_port_flag_still_wins_over_env_var() {
        unsafe {
            std::env::set_var("RUSTNZB_PORT", "8123");
        }
        let args = Args::parse_from(["rustnzb", "--port", "1234"]);
        unsafe {
            std::env::remove_var("RUSTNZB_PORT");
        }

        assert_eq!(args.port, Some(1234));
    }

    #[test]
    #[serial(rustnzb_port_env)]
    fn port_is_none_when_neither_flag_nor_env_var_is_set() {
        unsafe {
            std::env::remove_var("RUSTNZB_PORT");
        }
        let args = Args::parse_from(["rustnzb"]);

        assert_eq!(args.port, None);
    }

    // BUG-81: general.log_level in config.toml must take effect. Precedence is
    // RUST_LOG > --log-level > RUSTNZB_LOG_LEVEL > TOML general.log_level > "info".

    #[test]
    fn rust_log_wins_over_everything() {
        let (f, w) = resolve_log_filter(Some("nzb_nntp=trace"), Some("warn"), "debug");
        assert_eq!(f, "nzb_nntp=trace");
        assert!(w.is_none());
    }

    #[test]
    fn explicit_level_wins_over_toml() {
        let (f, w) = resolve_log_filter(None, Some("warn"), "debug");
        assert_eq!(f, "warn");
        assert!(w.is_none());
    }

    #[test]
    fn toml_level_used_when_no_cli_or_env() {
        let (f, w) = resolve_log_filter(None, None, "debug");
        assert_eq!(f, "debug");
        assert!(w.is_none());
    }

    #[test]
    fn defaults_to_info_when_nothing_set() {
        let (f, w) = resolve_log_filter(None, None, "");
        assert_eq!(f, "info");
        assert!(w.is_none());
        let (f, _) = resolve_log_filter(Some("  "), Some(""), "  ");
        assert_eq!(f, "info");
    }

    #[test]
    fn full_filter_directives_are_accepted() {
        let (f, w) = resolve_log_filter(None, None, "info,nzb_nntp=debug");
        assert_eq!(f, "info,nzb_nntp=debug");
        assert!(w.is_none());
    }

    #[test]
    fn invalid_level_falls_back_to_info_with_warning() {
        let (f, w) = resolve_log_filter(None, None, "verbose");
        assert_eq!(f, "info");
        assert!(w.unwrap().contains("verbose"));

        let (f, w) = resolve_log_filter(None, Some("loud"), "debug");
        assert_eq!(f, "info");
        assert!(w.unwrap().contains("loud"));
    }

    #[test]
    fn invalid_rust_log_falls_through_to_next_source() {
        let (f, w) = resolve_log_filter(Some("=[bad"), None, "debug");
        assert_eq!(f, "debug");
        assert!(w.unwrap().contains("RUST_LOG"));
    }

    #[test]
    fn level_names_are_case_insensitive() {
        let (f, w) = resolve_log_filter(None, None, "DEBUG");
        assert_eq!(f, "DEBUG");
        assert!(w.is_none());
    }

    #[test]
    #[serial(rustnzb_log_level_env)]
    fn log_level_arg_has_no_clap_default() {
        unsafe {
            std::env::remove_var("RUSTNZB_LOG_LEVEL");
        }
        let args = Args::parse_from(["rustnzb"]);
        assert_eq!(args.log_level, None);
    }

    #[test]
    #[serial(rustnzb_log_level_env)]
    fn log_level_env_var_is_used_and_explicit_flag_wins() {
        unsafe {
            std::env::set_var("RUSTNZB_LOG_LEVEL", "debug");
        }
        let from_env = Args::parse_from(["rustnzb"]);
        let from_flag = Args::parse_from(["rustnzb", "--log-level", "warn"]);
        unsafe {
            std::env::remove_var("RUSTNZB_LOG_LEVEL");
        }
        assert_eq!(from_env.log_level.as_deref(), Some("debug"));
        assert_eq!(from_flag.log_level.as_deref(), Some("warn"));
    }
}
