use crate::config::MB;
use crate::runner::ClientResult;
use anyhow::Result;
use std::path::Path;

const SAB_COLOR: &str = "#F5A623";
const RNZB_COLOR: &str = "#4A90D9";
const BG_COLOR: &str = "#1a1a2e";
const PANEL_BG: &str = "#16213e";
const TEXT_COLOR: &str = "#ddd";
const GRID_COLOR: &str = "#333";
const MUTED_TEXT: &str = "#888";
const GREEN: &str = "#4CAF50";
const RED: &str = "#FF5722";

pub fn generate_all(results: &[(ClientResult, ClientResult)], dir: &Path) -> Result<()> {
    tracing::info!("Generating charts...");
    for (sab, rnzb) in results {
        write_bar_chart(sab, rnzb, dir)?;
        write_timeseries(sab, rnzb, dir)?;
    }
    if results.len() > 1 {
        write_cross_scenario(results, dir)?;
    }
    write_dashboard(results, dir)?;
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

type ComparisonMetric<'a> = (&'a str, f64, f64, &'a str);
type ComparisonCategory<'a> = (&'a str, Vec<ComparisonMetric<'a>>);
type MetricExtractor = Box<dyn Fn(&crate::metrics::MetricSample) -> f64>;

fn write_bar_chart(sab: &ClientResult, rnzb: &ClientResult, dir: &Path) -> Result<()> {
    let categories: Vec<ComparisonCategory<'_>> = vec![
        (
            "Timing",
            vec![
                ("Total Time", sab.total_sec, rnzb.total_sec, "s"),
                ("Download", sab.download_sec, rnzb.download_sec, "s"),
                ("Par2", sab.par2_sec, rnzb.par2_sec, "s"),
                ("Unpack", sab.unpack_sec, rnzb.unpack_sec, "s"),
            ],
        ),
        (
            "Speed",
            vec![
                ("Avg Speed", sab.avg_speed_mbps, rnzb.avg_speed_mbps, "Mbps"),
                (
                    "Peak Speed",
                    sab.peak_speed_mbps,
                    rnzb.peak_speed_mbps,
                    "Mbps",
                ),
            ],
        ),
        (
            "Resources",
            vec![
                ("CPU Avg", sab.cpu_avg, rnzb.cpu_avg, "%"),
                ("CPU Peak", sab.cpu_peak, rnzb.cpu_peak, "%"),
                ("Mem Peak", sab.mem_peak_mb, rnzb.mem_peak_mb, "MB"),
            ],
        ),
    ];

    let mut rows: Vec<(&str, &str, f64, f64, &str)> = Vec::new();
    for (cat, metrics) in &categories {
        for (label, s, r, unit) in metrics {
            if *s != 0.0 || *r != 0.0 {
                rows.push((cat, label, *s, *r, unit));
            }
        }
    }

    let w = 900;
    let row_h = 44;
    let mut cat_headers = 0;
    let mut prev_cat = "";
    for (cat, _, _, _, _) in &rows {
        if *cat != prev_cat {
            cat_headers += 1;
            prev_cat = cat;
        }
    }
    let h = 90 + rows.len() * row_h + cat_headers * 30;
    let bar_w = 320.0;
    let label_x = 160.0;
    let bar_x = label_x + 30.0;

    let desc = xml_escape(&sab.scenario_description);

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" style="background:{BG_COLOR}">
<text x="{}" y="28" fill="{TEXT_COLOR}" font-size="16" text-anchor="middle" font-family="monospace" font-weight="bold">{} — Comparison</text>
<text x="{}" y="48" fill="{MUTED_TEXT}" font-size="12" text-anchor="middle" font-family="monospace">{desc}</text>
<text x="{}" y="68" fill="{SAB_COLOR}" font-size="11" font-family="monospace">■ SABnzbd</text>
<text x="{}" y="68" fill="{RNZB_COLOR}" font-size="11" font-family="monospace">■ rustnzb</text>"#,
        w / 2,
        sab.scenario,
        w / 2,
        w - 220,
        w - 110,
    );

    let mut y = 80;
    prev_cat = "";
    for (cat, label, sab_v, rnzb_v, unit) in &rows {
        if *cat != prev_cat {
            y += 8;
            svg.push_str(&format!(
                r#"<text x="20" y="{}" fill="{MUTED_TEXT}" font-size="10" font-family="monospace">{cat}</text>"#,
                y + 12
            ));
            svg.push_str(&format!(
                r#"<line x1="20" y1="{}" x2="{}" y2="{}" stroke="{GRID_COLOR}" stroke-width="1"/>"#,
                y + 16,
                w - 20,
                y + 16
            ));
            y += 22;
            prev_cat = cat;
        }

        let max_val = sab_v.max(*rnzb_v).max(0.001);
        let sw = sab_v / max_val * bar_w;
        let rw = rnzb_v / max_val * bar_w;

        svg.push_str(&format!(
            r#"<text x="{label_x}" y="{}" fill="{TEXT_COLOR}" font-size="11" text-anchor="end" font-family="monospace">{label}</text>"#,
            y + 12
        ));
        svg.push_str(&format!(
            r#"<rect x="{bar_x}" y="{y}" width="{sw:.1}" height="16" fill="{SAB_COLOR}" opacity="0.9" rx="2"/>"#,
        ));
        svg.push_str(&format!(
            r#"<text x="{}" y="{}" fill="{TEXT_COLOR}" font-size="9" font-family="monospace">{:.1} {unit}</text>"#,
            bar_x + sw + 5.0, y + 12, sab_v
        ));

        let y2 = y + 20;
        svg.push_str(&format!(
            r#"<rect x="{bar_x}" y="{y2}" width="{rw:.1}" height="16" fill="{RNZB_COLOR}" opacity="0.9" rx="2"/>"#,
        ));
        svg.push_str(&format!(
            r#"<text x="{}" y="{}" fill="{TEXT_COLOR}" font-size="9" font-family="monospace">{:.1} {unit}</text>"#,
            bar_x + rw + 5.0, y2 + 12, rnzb_v
        ));

        y += row_h;
    }

    svg.push_str("</svg>");
    std::fs::write(dir.join(format!("{}_comparison.svg", sab.scenario)), &svg)?;
    Ok(())
}

fn write_timeseries(sab: &ClientResult, rnzb: &ClientResult, dir: &Path) -> Result<()> {
    let panels: Vec<(&str, &str, MetricExtractor)> = vec![
        ("CPU", "%", Box::new(|s| s.cpu_pct)),
        ("Memory", "MB", Box::new(|s| s.mem_bytes as f64 / MB as f64)),
        ("Net RX", "Mbps", Box::new(|s| s.net_rx_bps * 8.0 / 1e6)),
        (
            "Disk Write",
            "MB/s",
            Box::new(|s| s.disk_write_bps / MB as f64),
        ),
    ];

    let panel_h: usize = 160;
    let panel_gap: usize = 40;
    let w: usize = 850;
    let px: usize = 70;
    let pw: usize = w - 100;
    let h = 80 + panels.len() * (panel_h + panel_gap);

    let desc = xml_escape(&sab.scenario_description);

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" style="background:{BG_COLOR}">
<text x="{}" y="28" fill="{TEXT_COLOR}" font-size="16" text-anchor="middle" font-family="monospace" font-weight="bold">{} — Time Series</text>
<text x="{}" y="48" fill="{MUTED_TEXT}" font-size="12" text-anchor="middle" font-family="monospace">{desc}</text>
<text x="{}" y="66" fill="{SAB_COLOR}" font-size="11" font-family="monospace">— SABnzbd</text>
<text x="{}" y="66" fill="{RNZB_COLOR}" font-size="11" font-family="monospace">— rustnzb</text>"#,
        w / 2,
        sab.scenario,
        w / 2,
        w - 220,
        w - 110,
    );

    let sab_dur = if sab.timeseries.is_empty() {
        0.0
    } else {
        sab.timeseries.last().unwrap().ts - sab.timeseries[0].ts
    };
    let rnzb_dur = if rnzb.timeseries.is_empty() {
        0.0
    } else {
        rnzb.timeseries.last().unwrap().ts - rnzb.timeseries[0].ts
    };
    let max_t = sab_dur.max(rnzb_dur).max(1.0);

    for (pi, (label, unit, extractor)) in panels.iter().enumerate() {
        let py = 75 + pi * (panel_h + panel_gap);

        let sab_max = sab.timeseries.iter().map(extractor).fold(0.0f64, f64::max);
        let rnzb_max = rnzb.timeseries.iter().map(extractor).fold(0.0f64, f64::max);
        let max_v = sab_max.max(rnzb_max).max(0.01);

        svg.push_str(&format!(
            r#"<rect x="{px}" y="{py}" width="{pw}" height="{panel_h}" fill="{PANEL_BG}" rx="4"/>"#,
        ));
        svg.push_str(&format!(
            r#"<text x="{}" y="{}" fill="{TEXT_COLOR}" font-size="13" text-anchor="middle" font-family="monospace" font-weight="bold">{label} ({unit})</text>"#,
            px + pw / 2, py - 8,
        ));

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
                px - 4, gy + 3.0,
            ));
        }

        for (result, color) in [(sab, SAB_COLOR), (rnzb, RNZB_COLOR)] {
            if result.timeseries.is_empty() {
                continue;
            }
            let t0 = result.timeseries[0].ts;
            let points: Vec<String> = result
                .timeseries
                .iter()
                .map(|s| {
                    let t = s.ts - t0;
                    let v = extractor(s);
                    let x = px as f64 + (t / max_t) * pw as f64;
                    let y = (py + panel_h) as f64 - (v / max_v) * (panel_h - 10) as f64;
                    format!("{x:.1},{y:.1}")
                })
                .collect();
            if !points.is_empty() {
                svg.push_str(&format!(
                    r#"<polyline points="{}" fill="none" stroke="{color}" stroke-width="2" opacity="0.9"/>"#,
                    points.join(" ")
                ));
            }
        }
    }

    svg.push_str("</svg>");
    std::fs::write(dir.join(format!("{}_timeseries.svg", sab.scenario)), &svg)?;
    Ok(())
}

fn write_cross_scenario(results: &[(ClientResult, ClientResult)], dir: &Path) -> Result<()> {
    let n = results.len();
    let w = 950;
    let row_h = 50;
    let h = 90 + n * row_h;

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" style="background:{BG_COLOR}">
<text x="{}" y="28" fill="{TEXT_COLOR}" font-size="16" text-anchor="middle" font-family="monospace" font-weight="bold">Cross-Scenario: Speed Ratio (SABnzbd time / rustnzb time)</text>
<text x="{}" y="50" fill="{GREEN}" font-size="10" font-family="monospace">Green = rustnzb faster</text>
<text x="{}" y="50" fill="{RED}" font-size="10" font-family="monospace">Red = SABnzbd faster</text>"#,
        w / 2,
        w / 2 - 120,
        w / 2 + 60,
    );

    let max_bar = 400.0;
    let max_ratio = results
        .iter()
        .map(|(sab, rnzb)| {
            if rnzb.total_sec > 0.0 {
                sab.total_sec / rnzb.total_sec
            } else {
                1.0
            }
        })
        .fold(0.0f64, f64::max)
        .max(2.0);

    for (i, (sab, rnzb)) in results.iter().enumerate() {
        let y = 80 + i * row_h;
        let ratio = if rnzb.total_sec > 0.0 {
            sab.total_sec / rnzb.total_sec
        } else {
            1.0
        };
        let bar_w = (ratio / max_ratio * max_bar).min(max_bar);
        let color = if ratio >= 1.0 { GREEN } else { RED };

        svg.push_str(&format!(
            r#"<text x="290" y="{}" fill="{TEXT_COLOR}" font-size="10" text-anchor="end" font-family="monospace">{}</text>"#,
            y + 16, xml_escape(&sab.scenario),
        ));
        svg.push_str(&format!(
            r#"<text x="290" y="{}" fill="{MUTED_TEXT}" font-size="8" text-anchor="end" font-family="monospace">{}</text>"#,
            y + 28, xml_escape(&sab.scenario_description),
        ));
        svg.push_str(&format!(
            r#"<rect x="300" y="{}" width="{bar_w:.0}" height="24" fill="{color}" opacity="0.85" rx="3"/>"#,
            y + 6,
        ));
        svg.push_str(&format!(
            r#"<text x="{}" y="{}" fill="{TEXT_COLOR}" font-size="11" font-family="monospace" font-weight="bold">{ratio:.2}x</text>"#,
            300.0 + bar_w + 8.0, y + 23,
        ));
    }

    svg.push_str("</svg>");
    std::fs::write(dir.join("cross_scenario.svg"), &svg)?;
    Ok(())
}

fn write_dashboard(results: &[(ClientResult, ClientResult)], dir: &Path) -> Result<()> {
    let n = results.len();
    let w = 1100;
    let row_h = 90;
    let h = 110 + n * row_h;

    let col_labels = [
        "Total Time (s)",
        "Avg Speed (Mbps)",
        "CPU Peak (%)",
        "Mem Peak (MB)",
    ];

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" style="background:{BG_COLOR}">
<text x="{}" y="28" fill="{TEXT_COLOR}" font-size="18" text-anchor="middle" font-family="monospace" font-weight="bold">Benchmark Dashboard: SABnzbd vs rustnzb</text>
<text x="60" y="55" fill="{SAB_COLOR}" font-size="12" font-family="monospace">■ SABnzbd</text>
<text x="180" y="55" fill="{RNZB_COLOR}" font-size="12" font-family="monospace">■ rustnzb</text>"#,
        w / 2,
    );

    for (ci, col_label) in col_labels.iter().enumerate() {
        let cx = 320 + ci * 190;
        svg.push_str(&format!(
            r#"<text x="{cx}" y="75" fill="{MUTED_TEXT}" font-size="10" font-family="monospace">{col_label}</text>"#,
        ));
    }
    svg.push_str(&format!(
        r#"<line x1="10" y1="82" x2="{}" y2="82" stroke="{GRID_COLOR}" stroke-width="1"/>"#,
        w - 10,
    ));

    for (i, (sab, rnzb)) in results.iter().enumerate() {
        let y = 90 + i * row_h;
        let cols: Vec<(f64, f64, f64)> = vec![
            (
                sab.total_sec,
                rnzb.total_sec,
                sab.total_sec.max(rnzb.total_sec).max(1.0),
            ),
            (
                sab.avg_speed_mbps,
                rnzb.avg_speed_mbps,
                sab.avg_speed_mbps.max(rnzb.avg_speed_mbps).max(1.0),
            ),
            (
                sab.cpu_peak,
                rnzb.cpu_peak,
                sab.cpu_peak.max(rnzb.cpu_peak).max(0.1),
            ),
            (
                sab.mem_peak_mb,
                rnzb.mem_peak_mb,
                sab.mem_peak_mb.max(rnzb.mem_peak_mb).max(1.0),
            ),
        ];

        svg.push_str(&format!(
            r#"<text x="15" y="{}" fill="{TEXT_COLOR}" font-size="11" font-family="monospace" font-weight="bold">{}</text>"#,
            y + 18, xml_escape(&sab.scenario),
        ));
        svg.push_str(&format!(
            r#"<text x="15" y="{}" fill="{MUTED_TEXT}" font-size="9" font-family="monospace">{}</text>"#,
            y + 32, xml_escape(&sab.scenario_description),
        ));

        for (ci, (sv, rv, maxv)) in cols.iter().enumerate() {
            let cx = 320 + ci * 190;
            let bw = 140.0;
            let sw = sv / maxv * bw;
            let rw = rv / maxv * bw;

            svg.push_str(&format!(
                r#"<rect x="{cx}" y="{}" width="{sw:.0}" height="14" fill="{SAB_COLOR}" opacity="0.9" rx="2"/>"#,
                y + 5,
            ));
            svg.push_str(&format!(
                r#"<rect x="{cx}" y="{}" width="{rw:.0}" height="14" fill="{RNZB_COLOR}" opacity="0.9" rx="2"/>"#,
                y + 22,
            ));
            svg.push_str(&format!(
                r#"<text x="{}" y="{}" fill="{TEXT_COLOR}" font-size="8" font-family="monospace">{:.1}</text>"#,
                cx as f64 + sw + 3.0, y + 15, sv,
            ));
            svg.push_str(&format!(
                r#"<text x="{}" y="{}" fill="{TEXT_COLOR}" font-size="8" font-family="monospace">{:.1}</text>"#,
                cx as f64 + rw + 3.0, y + 32, rv,
            ));
        }

        if i < n - 1 {
            svg.push_str(&format!(
                r#"<line x1="10" y1="{}" x2="{}" y2="{}" stroke="{GRID_COLOR}" stroke-width="1"/>"#,
                y + row_h - 5,
                w - 10,
                y + row_h - 5,
            ));
        }
    }

    svg.push_str("</svg>");
    std::fs::write(dir.join("dashboard.svg"), &svg)?;
    Ok(())
}
