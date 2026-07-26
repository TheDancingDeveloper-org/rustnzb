mod charts;
mod clients;
mod config;
mod datagen;
mod docker;
mod metrics;
mod mock_nntp;
mod nzb;
mod report;
mod runner;
mod stress;
mod stress_charts;
mod stress_report;
mod synth_nntp;
mod yenc;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "benchnzb",
    about = "Usenet client benchmark: SABnzbd vs rustnzb"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the benchmark orchestrator (v1: SABnzbd vs rustnzb comparison)
    Run {
        #[arg(long, default_value = "quick", env = "SCENARIOS")]
        scenarios: String,
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
        #[arg(long, default_value = "/results")]
        results_dir: PathBuf,
    },
    /// Run the mock NNTP server (file-backed, for v1 benchmarks)
    MockNntp {
        #[arg(long, default_value = "119")]
        port: u16,
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
        #[arg(long, default_value = "8080")]
        health_port: u16,
    },
    /// Regenerate charts from existing benchmark JSON
    RegenCharts {
        #[arg(long)]
        json: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Run synthetic NNTP server (generates articles on-the-fly, for v2 stress tests)
    SynthNntp {
        #[arg(long, default_value = "119")]
        port: u16,
        #[arg(long, default_value = "8080")]
        health_port: u16,
    },
    /// Run stress-test benchmark (v2: duration-based, continuous load)
    Stress {
        /// Target client: "rustnzb" or "sabnzbd"
        #[arg(long, default_value = "rustnzb", env = "CLIENT")]
        client: String,
        /// How long to run (e.g. "1h", "4h30m", "30m")
        #[arg(long, default_value = "1h", env = "DURATION")]
        duration: String,
        /// Size of each NZB download (e.g. "5gb", "1gb", "500mb")
        #[arg(long, default_value = "5gb", env = "NZB_SIZE")]
        nzb_size: String,
        /// Number of NZBs to keep queued (feeder maintains this depth)
        #[arg(long, default_value = "5", env = "CONCURRENCY")]
        concurrency: usize,
        /// Seconds between metrics samples
        #[arg(long, default_value = "5")]
        poll_interval: u64,
        /// Seconds between completed-download cleanup sweeps
        #[arg(long, default_value = "30")]
        cleanup_interval: u64,
        /// Results output directory
        #[arg(long, default_value = "/results")]
        results_dir: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    eprintln!(
        "[benchnzb] PID={} args={:?}",
        std::process::id(),
        std::env::args().collect::<Vec<_>>()
    );

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = rt.block_on(async {
        match cli.command {
            Command::Run {
                scenarios,
                data_dir,
                results_dir,
            } => runner::run(scenarios, data_dir, results_dir).await,
            Command::MockNntp {
                port,
                data_dir,
                health_port,
            } => mock_nntp::run(port, data_dir, health_port).await,
            Command::RegenCharts { json, out } => {
                let data = std::fs::read_to_string(&json)?;
                let items: Vec<serde_json::Value> = serde_json::from_str(&data)?;
                let mut results = Vec::new();
                for item in &items {
                    let sab: runner::ClientResult =
                        serde_json::from_value(item["sabnzbd"].clone())?;
                    let rnzb: runner::ClientResult =
                        serde_json::from_value(item["rustnzb"].clone())?;
                    results.push((sab, rnzb));
                }
                std::fs::create_dir_all(&out)?;
                charts::generate_all(&results, &out)?;
                tracing::info!("Charts written to {}", out.display());
                Ok(())
            }
            Command::SynthNntp { port, health_port } => synth_nntp::run(port, health_port).await,
            Command::Stress {
                client,
                duration,
                nzb_size,
                concurrency,
                poll_interval,
                cleanup_interval,
                results_dir,
            } => {
                stress::run(stress::StressConfig {
                    client,
                    duration,
                    nzb_size,
                    concurrency,
                    poll_interval_secs: poll_interval,
                    cleanup_interval_secs: cleanup_interval,
                    results_dir,
                })
                .await
            }
        }
    });

    if let Err(ref e) = result {
        tracing::error!("Fatal error: {e:#}");
    }
    result
}
