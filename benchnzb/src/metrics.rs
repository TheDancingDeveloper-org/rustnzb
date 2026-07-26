use bollard::container::StatsOptions;
use bollard::Docker;
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct MetricSample {
    pub ts: f64,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    pub iowait_pct: f64,
}

pub struct MetricsCollector {
    docker: Docker,
    container_ids: HashMap<String, String>,
}

impl MetricsCollector {
    pub fn new(docker: Docker) -> Self {
        Self {
            docker,
            container_ids: HashMap::new(),
        }
    }

    pub async fn resolve_container_id(&mut self, service: &str) {
        if self.container_ids.contains_key(service) {
            return;
        }
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("com.docker.compose.service={service}")],
        )]);
        if let Ok(containers) = self
            .docker
            .list_containers(Some(bollard::container::ListContainersOptions {
                filters,
                ..Default::default()
            }))
            .await
        {
            if let Some(c) = containers.first() {
                if let Some(id) = &c.id {
                    self.container_ids.insert(service.to_string(), id.clone());
                    tracing::info!("Resolved {service} -> {}", &id[..12]);
                }
            }
        }
    }

    pub fn start_collecting(&self, service: &str) -> Option<StatsHandle> {
        let container_id = self.container_ids.get(service)?.clone();
        let samples = Arc::new(Mutex::new(Vec::<MetricSample>::new()));
        let samples_clone = samples.clone();
        let docker = self.docker.clone();
        let service_name = service.to_string();

        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let task = tokio::spawn(async move {
            let mut stream = docker.stats(
                &container_id,
                Some(StatsOptions {
                    stream: true,
                    one_shot: false,
                }),
            );

            let mut prev_cpu_total: u64 = 0;
            let mut prev_cpu_system: u64 = 0;
            let mut prev_net_rx: u64 = 0;
            let mut prev_net_tx: u64 = 0;
            let mut prev_disk_read: u64 = 0;
            let mut prev_disk_write: u64 = 0;
            let mut prev_ts: f64 = 0.0;
            let mut first = true;

            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    stat = stream.next() => {
                        let Some(Ok(stat)) = stat else { break };

                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs_f64();

                        let cpu_total = stat.cpu_stats.cpu_usage.total_usage;
                        let cpu_system = stat.cpu_stats.system_cpu_usage.unwrap_or(0);
                        let num_cpus = stat.cpu_stats.online_cpus.unwrap_or(
                            stat.cpu_stats.cpu_usage.percpu_usage
                                .as_ref()
                                .map(|v| v.len() as u64)
                                .unwrap_or(1)
                        );

                        let mem_bytes = stat.memory_stats.usage.unwrap_or(0)
                            - stat.memory_stats.stats
                                .as_ref()
                                .map(|s| match s {
                                    bollard::container::MemoryStatsStats::V1(v1) => v1.total_inactive_file,
                                    bollard::container::MemoryStatsStats::V2(v2) => v2.inactive_file,
                                })
                                .unwrap_or(0);

                        let mut net_rx: u64 = 0;
                        let mut net_tx: u64 = 0;
                        if let Some(networks) = &stat.networks {
                            for net in networks.values() {
                                net_rx += net.rx_bytes;
                                net_tx += net.tx_bytes;
                            }
                        }

                        let mut disk_read: u64 = 0;
                        let mut disk_write: u64 = 0;
                        if let Some(bio) = &stat.blkio_stats.io_service_bytes_recursive {
                            for entry in bio {
                                match entry.op.to_lowercase().as_str() {
                                    "read" => disk_read += entry.value,
                                    "write" => disk_write += entry.value,
                                    _ => {}
                                }
                            }
                        }

                        if first {
                            prev_cpu_total = cpu_total;
                            prev_cpu_system = cpu_system;
                            prev_net_rx = net_rx;
                            prev_net_tx = net_tx;
                            prev_disk_read = disk_read;
                            prev_disk_write = disk_write;
                            prev_ts = now;
                            first = false;
                            continue;
                        }

                        let dt = now - prev_ts;
                        if dt < 0.01 { continue; }

                        let cpu_delta = cpu_total.saturating_sub(prev_cpu_total) as f64;
                        let sys_delta = cpu_system.saturating_sub(prev_cpu_system) as f64;
                        let cpu_pct = if sys_delta > 0.0 {
                            (cpu_delta / sys_delta) * num_cpus as f64 * 100.0
                        } else { 0.0 };

                        let sample = MetricSample {
                            ts: now,
                            cpu_pct,
                            mem_bytes,
                            net_rx_bps: net_rx.saturating_sub(prev_net_rx) as f64 / dt,
                            net_tx_bps: net_tx.saturating_sub(prev_net_tx) as f64 / dt,
                            disk_read_bps: disk_read.saturating_sub(prev_disk_read) as f64 / dt,
                            disk_write_bps: disk_write.saturating_sub(prev_disk_write) as f64 / dt,
                            iowait_pct: 0.0,
                        };

                        if let Ok(mut vec) = samples_clone.lock() {
                            vec.push(sample);
                        }

                        prev_cpu_total = cpu_total;
                        prev_cpu_system = cpu_system;
                        prev_net_rx = net_rx;
                        prev_net_tx = net_tx;
                        prev_disk_read = disk_read;
                        prev_disk_write = disk_write;
                        prev_ts = now;
                    }
                }
            }
            tracing::debug!("[{service_name}] Stats collector stopped");
        });

        Some(StatsHandle {
            cancel,
            task,
            samples,
        })
    }

    pub fn container_id(&self, service: &str) -> Option<&str> {
        self.container_ids.get(service).map(String::as_str)
    }
}

pub struct StatsHandle {
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
    samples: Arc<Mutex<Vec<MetricSample>>>,
}

impl StatsHandle {
    pub async fn stop(self) -> Vec<MetricSample> {
        self.cancel.cancel();
        let _ = self.task.await;
        Arc::try_unwrap(self.samples)
            .unwrap_or_else(|arc| arc.lock().unwrap().clone().into())
            .into_inner()
            .unwrap_or_default()
    }
}
