use crate::config::MB;
use crate::stress::StressResult;
use anyhow::Result;
use std::path::Path;

const RNZB_COLOR: &str = "#4A90D9";
const BG_COLOR: &str = "#1a1a2e";
const PANEL_BG: &str = "#16213e";
const TEXT_COLOR: &str = "#ddd";
const GRID_COLOR: &str = "#333";
const MUTED_TEXT: &str = "#888";
const TREND_COLOR: &str = "#FF9800";
const GREEN: &str = "#4CAF50";
const RED: &str = "#FF5722";

pub fn generate_all(result: &StressResult, dir: &Path) -> Result<()> {
    tracing::info!("Generating stress test charts...");

    write_timeseries_chart(result, dir)?;
    write_window_bars(result, dir)?;
    write_dashboard(result, dir)?;

    let count = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "svg"))
        .count();
    tracing::info!("  Generated {count} chart(s)");
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

type StressMetricExtractor = Box<dyn Fn(&crate::stress::StressSample) -> f64>;

/// Multi-panel time-series chart: Speed, Memory, CPU over time with trend lines.
fn write_timeseries_chart(result: &StressResult, dir: &Path) -> Result<()> {
    let panels: Vec<(&str, &str, StressMetricExtractor)> = vec![
        (
            "Download Speed",
            "Mbps",
            Box::new(|s| s.speed_bps as f64 * 8.0 / 1_000_000.0),
        ),
        ("Memory", "MB", Box::new(|s| s.mem_bytes as f64 / MB as f64)),
        ("CPU", "%", Box::new(|s| s.cpu_pct)),
        (
            "Disk Write",
            "MB/s",
            Box::new(|s| s.disk_write_bps / MB as f64),
        ),
    ];

    let samples = &result.timeseries;
    if samples.is_empty() {
        return Ok(());
    }

    let panel_h: usize = 180;
    let panel_gap: usize = 50;
    let w: usize = 1000;
    let px: usize = 80;
    let pw: usize = w - 120;
    let h = 100 + panels.len() * (panel_h + panel_gap);

    let max_elapsed = samples.last().unwrap().elapsed_secs.max(1.0);
    let max_hours = max_elapsed / 3600.0;

    let duration_str = xml_escape(&result.config_summary.duration);
    let size_str = xml_escape(&result.config_summary.nzb_size);

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" style="background:{BG_COLOR}">
<text x="{}" y="28" fill="{TEXT_COLOR}" font-size="16" text-anchor="middle" font-family="monospace" font-weight="bold">Stress Test — Time Series</text>
<text x="{}" y="50" fill="{MUTED_TEXT}" font-size="12" text-anchor="middle" font-family="monospace">{duration_str} run, {size_str} NZBs, concurrency={}</text>
<text x="{}" y="70" fill="{RNZB_COLOR}" font-size="11" font-family="monospace">— measured</text>
<text x="{}" y="70" fill="{TREND_COLOR}" font-size="11" font-family="monospace">— trend</text>"#,
        w / 2,
        w / 2,
        result.config_summary.concurrency,
        w - 250,
        w - 140,
    );

    for (pi, (label, unit, extractor)) in panels.iter().enumerate() {
        let py = 85 + pi * (panel_h + panel_gap);

        let values: Vec<f64> = samples.iter().map(extractor).collect();
        let max_v = values.iter().cloned().fold(0.01f64, f64::max);

        // Panel background
        svg.push_str(&format!(
            r#"<rect x="{px}" y="{py}" width="{pw}" height="{panel_h}" fill="{PANEL_BG}" rx="4"/>"#,
        ));
        svg.push_str(&format!(
            r#"<text x="{}" y="{}" fill="{TEXT_COLOR}" font-size="13" text-anchor="middle" font-family="monospace" font-weight="bold">{label} ({unit})</text>"#,
            px + pw / 2,
            py - 10,
        ));

        // Y-axis grid
        for tick in 0..=4 {
            let frac = tick as f64 / 4.0;
            let val = max_v * frac;
            let gy = (py + panel_h) as f64 - frac * (panel_h - 10) as f64;
            svg.push_str(&format!(
                r#"<line x1="{px}" y1="{gy:.0}" x2="{}" y2="{gy:.0}" stroke="{GRID_COLOR}" stroke-width="0.5"/>"#,
                px + pw,
            ));
            let lbl = if max_v >= 100.0 {
                format!("{val:.0}")
            } else {
                format!("{val:.1}")
            };
            svg.push_str(&format!(
                r#"<text x="{}" y="{}" fill="{MUTED_TEXT}" font-size="8" text-anchor="end" font-family="monospace">{lbl}</text>"#,
                px - 4,
                gy + 3.0,
            ));
        }

        // X-axis labels (hours)
        let hour_step = if max_hours > 8.0 {
            2.0
        } else if max_hours > 4.0 {
            1.0
        } else {
            0.5
        };
        let mut hour_mark = 0.0;
        while hour_mark <= max_hours {
            let x = px as f64 + (hour_mark / max_hours) * pw as f64;
            svg.push_str(&format!(
                r#"<line x1="{x:.0}" y1="{py}" x2="{x:.0}" y2="{}" stroke="{GRID_COLOR}" stroke-width="0.5"/>"#,
                py + panel_h,
            ));
            if pi == panels.len() - 1 {
                svg.push_str(&format!(
                    r#"<text x="{x:.0}" y="{}" fill="{MUTED_TEXT}" font-size="8" text-anchor="middle" font-family="monospace">{:.1}h</text>"#,
                    py + panel_h + 14,
                    hour_mark,
                ));
            }
            hour_mark += hour_step;
        }

        // Data line
        let points: Vec<String> = samples
            .iter()
            .map(|s| {
                let x = px as f64 + (s.elapsed_secs / max_elapsed) * pw as f64;
                let v = extractor(s);
                let y = (py + panel_h) as f64 - (v / max_v) * (panel_h - 10) as f64;
                format!("{x:.1},{y:.1}")
            })
            .collect();

        if !points.is_empty() {
            svg.push_str(&format!(
                r#"<polyline points="{}" fill="none" stroke="{RNZB_COLOR}" stroke-width="1.5" opacity="0.85"/>"#,
                points.join(" ")
            ));
        }

        // Trend line (linear regression)
        if values.len() >= 10 {
            let xs: Vec<f64> = samples.iter().map(|s| s.elapsed_secs).collect();
            let (slope, intercept) = linear_regression(&xs, &values);

            let x1 = xs.first().copied().unwrap_or(0.0);
            let x2 = xs.last().copied().unwrap_or(1.0);
            let y1 = slope * x1 + intercept;
            let y2 = slope * x2 + intercept;

            let sx1 = px as f64 + (x1 / max_elapsed) * pw as f64;
            let sx2 = px as f64 + (x2 / max_elapsed) * pw as f64;
            let sy1 = (py + panel_h) as f64 - (y1 / max_v) * (panel_h - 10) as f64;
            let sy2 = (py + panel_h) as f64 - (y2 / max_v) * (panel_h - 10) as f64;

            svg.push_str(&format!(
                r#"<line x1="{sx1:.1}" y1="{sy1:.1}" x2="{sx2:.1}" y2="{sy2:.1}" stroke="{TREND_COLOR}" stroke-width="2" stroke-dasharray="6,3" opacity="0.8"/>"#,
            ));
        }
    }

    svg.push_str("</svg>");
    std::fs::write(dir.join("stress_timeseries.svg"), &svg)?;
    Ok(())
}

/// Bar chart showing per-window throughput consistency.
fn write_window_bars(result: &StressResult, dir: &Path) -> Result<()> {
    let windows = &result.windows;
    if windows.is_empty() {
        return Ok(());
    }

    let n = windows.len();
    let bar_w: usize = 12;
    let gap: usize = 4;
    let chart_w = n * (bar_w + gap) + 160;
    let w = chart_w.max(600);
    let chart_h: usize = 200;
    let h = chart_h + 120;
    let px: usize = 80;

    let max_speed = windows
        .iter()
        .map(|w| w.avg_speed_mbps)
        .fold(0.01f64, f64::max);

    let avg_speed: f64 = windows.iter().map(|w| w.avg_speed_mbps).sum::<f64>() / n as f64;

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" style="background:{BG_COLOR}">
<text x="{}" y="28" fill="{TEXT_COLOR}" font-size="14" text-anchor="middle" font-family="monospace" font-weight="bold">Per-Window Speed (Mbps)</text>
<text x="{}" y="48" fill="{MUTED_TEXT}" font-size="11" text-anchor="middle" font-family="monospace">{n} windows of {}s each</text>"#,
        w / 2,
        w / 2,
        crate::config::STRESS_WINDOW_SECS,
    );

    // Y-axis
    for tick in 0..=4 {
        let frac = tick as f64 / 4.0;
        let val = max_speed * frac;
        let gy = (60 + chart_h) as f64 - frac * chart_h as f64;
        svg.push_str(&format!(
            r#"<line x1="{px}" y1="{gy:.0}" x2="{}" y2="{gy:.0}" stroke="{GRID_COLOR}" stroke-width="0.5"/>"#,
            px + n * (bar_w + gap),
        ));
        svg.push_str(&format!(
            r#"<text x="{}" y="{}" fill="{MUTED_TEXT}" font-size="8" text-anchor="end" font-family="monospace">{:.0}</text>"#,
            px - 4,
            gy + 3.0,
            val,
        ));
    }

    // Average line
    let avg_y = (60 + chart_h) as f64 - (avg_speed / max_speed) * chart_h as f64;
    svg.push_str(&format!(
        r#"<line x1="{px}" y1="{avg_y:.0}" x2="{}" y2="{avg_y:.0}" stroke="{TREND_COLOR}" stroke-width="1" stroke-dasharray="4,2"/>"#,
        px + n * (bar_w + gap),
    ));
    svg.push_str(&format!(
        r#"<text x="{}" y="{}" fill="{TREND_COLOR}" font-size="8" font-family="monospace">avg {avg_speed:.0}</text>"#,
        px + n * (bar_w + gap) + 4,
        avg_y + 3.0,
    ));

    // Bars
    for (i, window) in windows.iter().enumerate() {
        let x = px + i * (bar_w + gap);
        let bar_h = (window.avg_speed_mbps / max_speed * chart_h as f64) as usize;
        let y = 60 + chart_h - bar_h;

        let color = if window.avg_speed_mbps >= avg_speed * 0.9 {
            RNZB_COLOR
        } else {
            RED
        };

        svg.push_str(&format!(
            r#"<rect x="{x}" y="{y}" width="{bar_w}" height="{bar_h}" fill="{color}" opacity="0.85" rx="1"/>"#,
        ));
    }

    svg.push_str("</svg>");
    std::fs::write(dir.join("stress_windows.svg"), &svg)?;
    Ok(())
}

/// Combined dashboard with key metrics and degradation verdict.
fn write_dashboard(result: &StressResult, dir: &Path) -> Result<()> {
    let w = 900;
    let h = 400;
    let d = &result.degradation;

    let verdict_color = if d.degradation_detected { RED } else { GREEN };
    let verdict_text = if d.degradation_detected {
        "DEGRADATION DETECTED"
    } else {
        "STABLE"
    };

    let duration_str = xml_escape(&result.config_summary.duration);
    let size_str = xml_escape(&result.config_summary.nzb_size);

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" style="background:{BG_COLOR}">
<text x="{}" y="30" fill="{TEXT_COLOR}" font-size="18" text-anchor="middle" font-family="monospace" font-weight="bold">Stress Test Dashboard</text>
<text x="{}" y="52" fill="{MUTED_TEXT}" font-size="12" text-anchor="middle" font-family="monospace">{duration_str} | {size_str} NZBs | concurrency={}</text>"#,
        w / 2,
        w / 2,
        result.config_summary.concurrency,
    );

    // Verdict box
    svg.push_str(&format!(
        r#"<rect x="300" y="70" width="300" height="40" fill="{verdict_color}" opacity="0.2" rx="6"/>
<text x="450" y="96" fill="{verdict_color}" font-size="16" text-anchor="middle" font-family="monospace" font-weight="bold">{verdict_text}</text>"#,
    ));

    // Key metrics in a grid
    let metrics: Vec<(&str, String)> = vec![
        (
            "Total Downloaded",
            crate::config::format_size(result.total_bytes_downloaded),
        ),
        ("NZBs Completed", result.total_nzbs_completed.to_string()),
        (
            "Avg Speed",
            format!("{:.1} Mbps", result.overall_avg_speed_mbps),
        ),
        (
            "Duration",
            crate::config::format_duration(std::time::Duration::from_secs_f64(
                result.duration_secs,
            )),
        ),
        (
            "Speed Trend",
            format!("{:+.2}%/hour", d.speed_trend_pct_per_hour),
        ),
        (
            "Memory Trend",
            format!("{:+.1} MB/hour", d.memory_trend_mb_per_hour),
        ),
        (
            "Speed (first)",
            format!("{:.1} Mbps", d.speed_first_window_mbps),
        ),
        (
            "Speed (last)",
            format!("{:.1} Mbps", d.speed_last_window_mbps),
        ),
        (
            "Memory (first)",
            format!("{:.1} MB", d.memory_first_window_mb),
        ),
        (
            "Memory (last)",
            format!("{:.1} MB", d.memory_last_window_mb),
        ),
    ];

    let col_w = 280;
    let row_h = 26;
    let start_y = 130;

    for (i, (label, value)) in metrics.iter().enumerate() {
        let col = i / 5;
        let row = i % 5;
        let x = 60 + col * col_w;
        let y = start_y + row * row_h;

        svg.push_str(&format!(
            r#"<text x="{x}" y="{y}" fill="{MUTED_TEXT}" font-size="11" font-family="monospace">{label}:</text>"#,
        ));
        svg.push_str(&format!(
            r#"<text x="{}" y="{y}" fill="{TEXT_COLOR}" font-size="11" font-family="monospace">{value}</text>"#,
            x + 160,
        ));
    }

    // Notes
    let notes_y = start_y + 5 * row_h + 20;
    for (i, note) in d.notes.iter().enumerate() {
        svg.push_str(&format!(
            r#"<text x="60" y="{}" fill="{MUTED_TEXT}" font-size="10" font-family="monospace">- {}</text>"#,
            notes_y + i * 18,
            xml_escape(note),
        ));
    }

    svg.push_str("</svg>");
    std::fs::write(dir.join("stress_dashboard.svg"), &svg)?;
    Ok(())
}

fn linear_regression(x: &[f64], y: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    if n < 2.0 {
        return (0.0, y.first().copied().unwrap_or(0.0));
    }

    let sum_x: f64 = x.iter().sum();
    let sum_y: f64 = y.iter().sum();
    let sum_xy: f64 = x.iter().zip(y.iter()).map(|(a, b)| a * b).sum();
    let sum_x2: f64 = x.iter().map(|a| a * a).sum();

    let denom = n * sum_x2 - sum_x * sum_x;
    if denom.abs() < 1e-10 {
        return (0.0, sum_y / n);
    }

    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n;

    (slope, intercept)
}
