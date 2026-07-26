use crate::runner::ClientResult;
use anyhow::Result;
use std::path::Path;

pub fn write_json(
    results: &[(ClientResult, ClientResult)],
    dir: &Path,
    timestamp: &str,
) -> Result<()> {
    let data: Vec<serde_json::Value> = results
        .iter()
        .map(|(sab, rnzb)| {
            serde_json::json!({
                "scenario": sab.scenario,
                "sabnzbd": sab,
                "rustnzb": rnzb,
            })
        })
        .collect();
    let path = dir.join(format!("benchmark_{timestamp}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&data)?)?;
    tracing::info!("JSON: {}", path.display());
    Ok(())
}

pub fn write_csv(
    results: &[(ClientResult, ClientResult)],
    dir: &Path,
    timestamp: &str,
) -> Result<()> {
    let path = dir.join(format!("benchmark_{timestamp}.csv"));
    let mut out = String::from(
        "scenario,test_type,client,total_bytes,total_sec,download_sec,par2_sec,unpack_sec,\
         avg_speed_mbps,peak_speed_mbps,cpu_avg,cpu_peak,mem_avg_mb,mem_peak_mb,\
         net_rx_avg_mbps,net_rx_peak_mbps,disk_write_avg_mbps,disk_write_peak_mbps,\
         iowait_avg,iowait_peak,\
         int_dl_throughput_mbps,int_articles_downloaded,int_articles_failed,outcome,payload_verified,\
         peak_work_dir_bytes,fixture_payload_bytes,fixture_wire_bytes,fixture_article_requests,fixture_articles_served,fixture_article_not_found\n",
    );
    for (sab, rnzb) in results {
        for r in [sab, rnzb] {
            let (int_dl, int_art_ok, int_art_fail) = if let Some(ref im) = r.internal_metrics {
                (
                    im.download_throughput_mbps,
                    im.articles_downloaded,
                    im.articles_failed,
                )
            } else {
                (0.0, 0, 0)
            };
            out.push_str(&format!(
                "{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},\
                 {:.2},{:.2},{:.2},{:.2},{:.4},{:.4},\
                 {:.2},{},{},{:?},{},{},{},{},{},{},{}\n",
                r.scenario,
                r.test_type,
                r.client,
                r.total_bytes,
                r.total_sec,
                r.download_sec,
                r.par2_sec,
                r.unpack_sec,
                r.avg_speed_mbps,
                r.peak_speed_mbps,
                r.cpu_avg,
                r.cpu_peak,
                r.mem_avg_mb,
                r.mem_peak_mb,
                r.net_rx_avg_mbps,
                r.net_rx_peak_mbps,
                r.disk_write_avg_mbps,
                r.disk_write_peak_mbps,
                r.iowait_avg,
                r.iowait_peak,
                int_dl,
                int_art_ok,
                int_art_fail,
                r.outcome,
                r.payload_verified,
                r.peak_work_dir_bytes,
                r.fixture_metrics.payload_bytes_served,
                r.fixture_metrics.wire_bytes_served,
                r.fixture_metrics.article_requests,
                r.fixture_metrics.articles_served,
                r.fixture_metrics.article_not_found,
            ));
        }
    }
    std::fs::write(&path, &out)?;
    tracing::info!("CSV: {}", path.display());
    Ok(())
}

pub fn build_summary(results: &[(ClientResult, ClientResult)]) -> String {
    let mut lines = vec![
        String::new(),
        "=".repeat(84),
        "  BENCHMARK RESULTS: SABnzbd vs rustnzb".into(),
        "=".repeat(84),
    ];

    for (sab, rnzb) in results {
        lines.push(String::new());
        lines.push(format!(
            "  Scenario: {} — {} [{}]",
            sab.scenario, sab.scenario_description, sab.test_type
        ));
        lines.push("-".repeat(84));
        lines.push(format!(
            "  {:24} {:>15} {:>15} {:>14}",
            "Metric", "SABnzbd", "rustnzb", "Delta"
        ));
        lines.push("-".repeat(84));

        let metrics: Vec<(&str, String, String, f64, f64, bool)> = vec![
            (
                "Total Time",
                format!("{:.1}s", sab.total_sec),
                format!("{:.1}s", rnzb.total_sec),
                sab.total_sec,
                rnzb.total_sec,
                true,
            ),
            (
                "Download Time",
                format!("{:.1}s", sab.download_sec),
                format!("{:.1}s", rnzb.download_sec),
                sab.download_sec,
                rnzb.download_sec,
                true,
            ),
            (
                "Par2 Time",
                format!("{:.1}s", sab.par2_sec),
                format!("{:.1}s", rnzb.par2_sec),
                sab.par2_sec,
                rnzb.par2_sec,
                true,
            ),
            (
                "Unpack Time",
                format!("{:.1}s", sab.unpack_sec),
                format!("{:.1}s", rnzb.unpack_sec),
                sab.unpack_sec,
                rnzb.unpack_sec,
                true,
            ),
            (
                "Avg Speed",
                format!("{:.1} Mbps", sab.avg_speed_mbps),
                format!("{:.1} Mbps", rnzb.avg_speed_mbps),
                sab.avg_speed_mbps,
                rnzb.avg_speed_mbps,
                false,
            ),
            (
                "Peak Speed",
                format!("{:.1} Mbps", sab.peak_speed_mbps),
                format!("{:.1} Mbps", rnzb.peak_speed_mbps),
                sab.peak_speed_mbps,
                rnzb.peak_speed_mbps,
                false,
            ),
            (
                "CPU Avg",
                format!("{:.1}%", sab.cpu_avg),
                format!("{:.1}%", rnzb.cpu_avg),
                sab.cpu_avg,
                rnzb.cpu_avg,
                true,
            ),
            (
                "CPU Peak",
                format!("{:.1}%", sab.cpu_peak),
                format!("{:.1}%", rnzb.cpu_peak),
                sab.cpu_peak,
                rnzb.cpu_peak,
                true,
            ),
            (
                "Memory Avg",
                format!("{:.1} MB", sab.mem_avg_mb),
                format!("{:.1} MB", rnzb.mem_avg_mb),
                sab.mem_avg_mb,
                rnzb.mem_avg_mb,
                true,
            ),
            (
                "Memory Peak",
                format!("{:.1} MB", sab.mem_peak_mb),
                format!("{:.1} MB", rnzb.mem_peak_mb),
                sab.mem_peak_mb,
                rnzb.mem_peak_mb,
                true,
            ),
            (
                "Disk Write Avg",
                format!("{:.1} MB/s", sab.disk_write_avg_mbps),
                format!("{:.1} MB/s", rnzb.disk_write_avg_mbps),
                sab.disk_write_avg_mbps,
                rnzb.disk_write_avg_mbps,
                false,
            ),
        ];

        for (label, sab_s, rnzb_s, sab_v, rnzb_v, lower_better) in &metrics {
            if *sab_v == 0.0 && *rnzb_v == 0.0 {
                continue;
            }
            let delta = delta_str(*sab_v, *rnzb_v, *lower_better);
            lines.push(format!(
                "  {label:<24} {sab_s:>15} {rnzb_s:>15} {delta:>14}"
            ));
        }
        lines.push("-".repeat(84));
    }

    lines.push(String::new());
    lines.push("  Delta: ▲ = rustnzb better, ▼ = rustnzb worse".into());
    lines.push(String::new());
    lines.join("\n")
}

pub fn write_summary(summary: &str, dir: &Path, timestamp: &str) -> Result<()> {
    let path = dir.join(format!("summary_{timestamp}.txt"));
    std::fs::write(&path, summary)?;
    tracing::info!("Summary: {}", path.display());
    Ok(())
}

fn delta_str(sab: f64, rnzb: f64, lower_better: bool) -> String {
    if sab == 0.0 && rnzb == 0.0 {
        return "\u{2014}".to_string();
    }
    if sab == 0.0 {
        return "\u{2014}".to_string();
    }
    let mut pct = (sab - rnzb) / sab * 100.0;
    if !lower_better {
        pct = -pct;
    }
    if pct.abs() < 0.5 {
        return "~same".to_string();
    }
    let prefix = if pct > 0.0 { "+" } else { "" };
    // ▲ = rustnzb better, ▼ = rustnzb worse
    let arrow = if pct > 0.0 { " \u{25B2}" } else { " \u{25BC}" };
    format!("{prefix}{pct:.1}%{arrow}")
}
