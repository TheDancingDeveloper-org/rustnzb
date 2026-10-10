// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use nzb_web::startup::InstanceLock;
use nzb_web::{LogBuffer, LogBufferLayer, QueueManager, StartupConfig};

/// Keeps the data-dir flock alive independently of the HTTP task.
///
/// `start_engine` returns one of these. Dropping `StartupResult` after the
/// server is spawned used to unlock immediately.
struct RetainedInstanceLock {
    lock: InstanceLock,
}

impl RetainedInstanceLock {
    fn new(lock: InstanceLock) -> Self {
        Self { lock }
    }
}

/// Shared state accessible from Tauri commands and background tasks.
struct EngineState {
    queue_manager: Arc<QueueManager>,
    server_port: u16,
    instance_lock: RetainedInstanceLock,
}

/// Determine the platform-appropriate config directory.
fn config_dir() -> anyhow::Result<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "rustnzb", "rustnzb")
        .context("Could not determine config directory")?;
    let config_dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&config_dir)?;
    Ok(config_dir)
}

/// Determine the platform-appropriate data directory.
fn data_dir() -> anyhow::Result<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "rustnzb", "rustnzb")
        .context("Could not determine data directory")?;
    let data_dir = dirs.data_dir().to_path_buf();
    std::fs::create_dir_all(&data_dir)?;
    Ok(data_dir)
}

/// Format bytes per second into a human-readable speed string.
fn format_speed(bps: f64) -> String {
    if bps < 1024.0 {
        format!("{:.0} B/s", bps)
    } else if bps < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", bps / 1024.0)
    } else if bps < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bps / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB/s", bps / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Build and show the system tray with menu items.
fn setup_tray(app: &AppHandle) -> anyhow::Result<()> {
    let speed_item = MenuItemBuilder::with_id("speed", "Speed: idle")
        .enabled(false)
        .build(app)?;

    let pause_item = MenuItemBuilder::with_id("pause", "Pause Downloads").build(app)?;

    let browser_item = MenuItemBuilder::with_id("browser", "Open in Browser").build(app)?;

    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&speed_item)
        .separator()
        .item(&pause_item)
        .item(&browser_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let app_handle = app.clone();
    TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("rustnzb")
        .on_menu_event(move |tray, event| {
            let app = tray.app_handle();
            match event.id().as_ref() {
                "pause" => {
                    if let Some(state) = app.try_state::<EngineState>() {
                        let qm = Arc::clone(&state.queue_manager);
                        if qm.is_paused() {
                            qm.resume_all();
                        } else {
                            qm.pause_all();
                        }
                    }
                }
                "browser" => {
                    if let Some(state) = app.try_state::<EngineState>() {
                        let url = format!("http://localhost:{}", state.server_port);
                        let _ = open::that(&url);
                    }
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(event, tauri::tray::TrayIconEvent::Click { .. }) {
                // Show/focus the main window on tray click
                if let Some(window) = tray.app_handle().get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(&app_handle)?;

    // Spawn background task to update tray menu speed display
    let app_for_tray = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            if let Some(state) = app_for_tray.try_state::<EngineState>() {
                let speed = state.queue_manager.get_speed();
                let paused = state.queue_manager.is_paused();
                let queue_size = state.queue_manager.queue_size();

                let speed_text = if paused {
                    "Speed: paused".to_string()
                } else if speed > 0 {
                    format!("Speed: {}", format_speed(speed as f64))
                } else {
                    "Speed: idle".to_string()
                };

                let tooltip = if queue_size > 0 {
                    format!(
                        "rustnzb - {} items - {}",
                        queue_size,
                        speed_text.trim_start_matches("Speed: ")
                    )
                } else {
                    "rustnzb".to_string()
                };

                // Update tray tooltip
                if let Some(tray) = app_for_tray.tray_by_id("main") {
                    let _ = tray.set_tooltip(Some(&tooltip));
                }
            }
        }
    });

    Ok(())
}

/// Start the rustnzb engine and HTTP server.
///
/// The returned lock must be kept alive for the process lifetime. Dropping
/// `StartupResult` after moving `state` out used to unlock immediately, so a
/// second desktop instance could start against the same data directory.
async fn start_engine() -> anyhow::Result<(Arc<QueueManager>, u16, RetainedInstanceLock)> {
    let config_dir = config_dir()?;
    let data_dir = data_dir()?;
    let config_path = config_dir.join("config.toml");

    // Initialize logging
    let log_buffer = LogBuffer::new();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);
    let log_layer = LogBufferLayer::new(log_buffer.clone());

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(log_layer)
        .init();

    info!("rustnzb-desktop starting");
    info!(config = %config_path.display(), "Config path");
    info!(data = %data_dir.display(), "Data path");

    // Initialize engine
    let result = nzb_web::startup::initialize(
        StartupConfig {
            config_path,
            listen_addr: Some("127.0.0.1".to_string()),
            port: None,
            data_dir: Some(data_dir),
            log_level: Some("info".to_string()),
        },
        Some(log_buffer),
    )
    .await?;

    let port = result.state.config().general.port;
    let queue_manager = Arc::clone(&result.queue_manager);
    // Take the lock before `result` is dropped by moving `state` below.
    let instance_lock = RetainedInstanceLock::new(result.instance_lock);

    // Spawn the HTTP server in the background
    let state = result.state;
    tokio::spawn(async move {
        if let Err(e) = rustnzb::server::run(state).await {
            error!("HTTP server error: {e}");
        }
    });

    // Wait for the server to be ready
    let health_url = format!("http://127.0.0.1:{port}/api/health");
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if reqwest::get(&health_url).await.is_ok() {
            info!("HTTP server ready on port {port}");
            return Ok((queue_manager, port, instance_lock));
        }
    }

    anyhow::bail!("HTTP server failed to become ready within 5 seconds");
}

async fn start() {
    tauri::async_runtime::set(tokio::runtime::Handle::current());

    // Install the rustls crypto provider before any TLS operations
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    // Start the engine and HTTP server
    let (queue_manager, port, instance_lock) = match start_engine().await {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to start engine: {e}");
            eprintln!("Fatal: Failed to start rustnzb engine: {e}");
            std::process::exit(1);
        }
    };

    let server_url = format!("http://127.0.0.1:{port}");

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .manage(EngineState {
            queue_manager,
            server_port: port,
            instance_lock,
        })
        .setup(move |app| {
            // Create main window pointing at the HTTP server
            let url = WebviewUrl::External(server_url.parse().unwrap());
            let _window = WebviewWindowBuilder::new(app, "main", url)
                .title("rustnzb")
                .inner_size(1280.0, 800.0)
                .build()?;

            // Set up system tray
            if let Err(e) = setup_tray(app.handle()) {
                warn!("Failed to set up system tray: {e}");
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![cmd_open_in_browser,])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn cmd_open_in_browser(state: tauri::State<'_, EngineState>) {
    let url = format!("http://localhost:{}", state.server_port);
    let _ = open::that(&url);
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("couldn't set up tokio runtime")
        .block_on(start())
}

#[cfg(test)]
mod tests {
    use super::format_speed;

    #[test]
    fn formats_speed_unit_boundaries() {
        assert_eq!(format_speed(0.0), "0 B/s");
        assert_eq!(format_speed(1023.0), "1023 B/s");
        assert_eq!(format_speed(1024.0), "1.0 KB/s");
        assert_eq!(format_speed(1024.0 * 1024.0), "1.0 MB/s");
        assert_eq!(format_speed(1024.0 * 1024.0 * 1024.0), "1.00 GB/s");
    }

    #[test]
    fn formats_fractional_speeds_with_unit_specific_precision() {
        assert_eq!(format_speed(1536.0), "1.5 KB/s");
        assert_eq!(format_speed(2.25 * 1024.0 * 1024.0), "2.2 MB/s");
        assert_eq!(format_speed(1.125 * 1024.0 * 1024.0 * 1024.0), "1.12 GB/s");
    }
}
