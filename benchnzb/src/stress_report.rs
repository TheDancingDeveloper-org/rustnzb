use crate::config;
use crate::stress::StressResult;
use anyhow::Result;
use std::path::Path;

pub fn write_json(result: &StressResult, dir: &Path, timestamp: &str) -> Result<()> {
    let path = dir.join(format!("stress_{timestamp}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(result)?)?;
    tracing::info!("JSON: {}", path.display());
    Ok(())
}

pub fn write_csv(result: &StressResult, dir: &Path, timestamp: &str) -> Result<()> {
    let path = dir.join(format!("stress_{timestamp}.csv"));

    let mut out = String::from(
        "window_start_secs,window_end_secs,avg_speed_mbps,avg_cpu_pct,avg_mem_mb,peak_mem_mb,nzbs_completed,bytes_downloaded\n",
    );

    for w in &result.windows {
        out.push_str(&format!(
            "{:.0},{:.0},{:.2},{:.2},{:.2},{:.2},{},{}\n",
            w.window_start_secs,
            w.window_end_secs,
            w.avg_speed_mbps,
            w.avg_cpu_pct,
            w.avg_mem_mb,
            w.peak_mem_mb,
            w.nzbs_completed,
            w.bytes_downloaded,
        ));
    }

    std::fs::write(&path, &out)?;
    tracing::info!("CSV: {}", path.display());
    Ok(())
}

pub fn build_summary(result: &StressResult) -> String {
    let mut lines = Vec::new();
    lines.push(String::new());
    lines.push("=".repeat(72));
    lines.push(format!(
        "  STRESS TEST RESULTS: {}",
        result.config_summary.client
    ));
    lines.push("=".repeat(72));

    lines.push(String::new());
    lines.push("  Configuration".into());
    lines.push("-".repeat(72));
    lines.push(format!(
        "  Duration:       {}",
        result.config_summary.duration
    ));
    lines.push(format!(
        "  NZB Size:       {}",
        result.config_summary.nzb_size
    ));
    lines.push(format!(
        "  Concurrency:    {} NZBs queued",
        result.config_summary.concurrency
    ));

    lines.push(String::new());
    lines.push("  Overall Results".into());
    lines.push("-".repeat(72));
    lines.push(format!(
        "  Total Duration:    {}",
        config::format_duration(std::time::Duration::from_secs_f64(result.duration_secs))
    ));
    lines.push(format!(
        "  NZBs Submitted:    {}",
        result.total_nzbs_submitted
    ));
    lines.push(format!(
        "  NZBs Completed:    {}",
        result.total_nzbs_completed
    ));
    lines.push(format!(
        "  Total Downloaded:  {}",
        config::format_size(result.total_bytes_downloaded)
    ));
    lines.push(format!(
        "  Avg Speed:         {:.1} Mbps ({:.1} MB/s)",
        result.overall_avg_speed_mbps,
        result.overall_avg_speed_mbps / 8.0
    ));

    // Windowed stats summary
    if !result.windows.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "  Windowed Stats ({} windows of {}s)",
            result.windows.len(),
            config::STRESS_WINDOW_SECS
        ));
        lines.push("-".repeat(72));

        let speeds: Vec<f64> = result.windows.iter().map(|w| w.avg_speed_mbps).collect();
        let cpus: Vec<f64> = result.windows.iter().map(|w| w.avg_cpu_pct).collect();
        let mems: Vec<f64> = result.windows.iter().map(|w| w.avg_mem_mb).collect();
        let peak_mems: Vec<f64> = result.windows.iter().map(|w| w.peak_mem_mb).collect();

        let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        let min_f = |v: &[f64]| v.iter().cloned().fold(f64::MAX, f64::min);
        let max_f = |v: &[f64]| v.iter().cloned().fold(0.0f64, f64::max);

        lines.push(format!(
            "  {:20} {:>10} {:>10} {:>10}",
            "Metric", "Min", "Avg", "Max"
        ));
        lines.push(format!(
            "  {:20} {:>10.1} {:>10.1} {:>10.1}",
            "Speed (Mbps)",
            min_f(&speeds),
            avg(&speeds),
            max_f(&speeds)
        ));
        lines.push(format!(
            "  {:20} {:>10.1} {:>10.1} {:>10.1}",
            "CPU (%)",
            min_f(&cpus),
            avg(&cpus),
            max_f(&cpus)
        ));
        lines.push(format!(
            "  {:20} {:>10.1} {:>10.1} {:>10.1}",
            "Memory Avg (MB)",
            min_f(&mems),
            avg(&mems),
            max_f(&mems)
        ));
        lines.push(format!(
            "  {:20} {:>10.1} {:>10.1} {:>10.1}",
            "Memory Peak (MB)",
            min_f(&peak_mems),
            avg(&peak_mems),
            max_f(&peak_mems)
        ));
    }

    // Degradation analysis
    lines.push(String::new());
    lines.push("  Degradation Analysis".into());
    lines.push("-".repeat(72));

    let d = &result.degradation;
    lines.push(format!(
        "  Speed Trend:       {:+.2}%/hour",
        d.speed_trend_pct_per_hour
    ));
    lines.push(format!(
        "  Memory Trend:      {:+.1} MB/hour",
        d.memory_trend_mb_per_hour
    ));

    if d.speed_first_window_mbps > 0.0 || d.speed_last_window_mbps > 0.0 {
        lines.push(format!(
            "  Speed (first):     {:.1} Mbps",
            d.speed_first_window_mbps
        ));
        lines.push(format!(
            "  Speed (last):      {:.1} Mbps",
            d.speed_last_window_mbps
        ));
    }
    if d.memory_first_window_mb > 0.0 || d.memory_last_window_mb > 0.0 {
        lines.push(format!(
            "  Memory (first):    {:.1} MB",
            d.memory_first_window_mb
        ));
        lines.push(format!(
            "  Memory (last):     {:.1} MB",
            d.memory_last_window_mb
        ));
    }

    let verdict = if d.degradation_detected {
        "DEGRADATION DETECTED"
    } else {
        "STABLE"
    };
    lines.push(format!("  Verdict:           {verdict}"));

    for note in &d.notes {
        lines.push(format!("    - {note}"));
    }

    lines.push(String::new());
    lines.push("=".repeat(72));
    lines.push(String::new());

    lines.join("\n")
}

pub fn write_summary(summary: &str, dir: &Path, timestamp: &str) -> Result<()> {
    let path = dir.join(format!("stress_summary_{timestamp}.txt"));
    std::fs::write(&path, summary)?;
    tracing::info!("Summary: {}", path.display());
    Ok(())
}
