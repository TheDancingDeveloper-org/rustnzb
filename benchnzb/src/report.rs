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

/// Write a self-contained, shareable report for a single deterministic run.
///
/// This intentionally reports only what the fixture measured. It does not
/// extrapolate a 32 MiB loopback run into an Internet-provider comparison.
pub fn write_html(
    results: &[(ClientResult, ClientResult)],
    dir: &Path,
    timestamp: &str,
) -> Result<()> {
    let path = dir.join(format!("benchmark_{timestamp}.html"));
    let mut rows = String::new();

    for (sab, rustnzb) in results {
        rows.push_str(&format!(
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&sab.scenario_description),
            client_cell(sab),
            client_cell(rustnzb),
            bytes(sab.fixture_metrics.wire_bytes_served),
            bytes(rustnzb.fixture_metrics.wire_bytes_served),
            sab.fixture_metrics.article_requests,
            rustnzb.fixture_metrics.article_requests,
            bytes(sab.peak_work_dir_bytes),
            bytes(rustnzb.peak_work_dir_bytes),
        ));
    }

    let document = format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>rustnzb benchmark report · {timestamp}</title>
<style>
:root {{ color-scheme: dark; --bg:#101722; --panel:#182334; --ink:#e9f1fc; --muted:#aebdd1; --line:#31445d; --accent:#61d8a0; --warn:#ffd166; }}
body {{ margin:0; background:var(--bg); color:var(--ink); font:16px/1.55 system-ui,-apple-system,Segoe UI,sans-serif; }}
main {{ max-width:1120px; margin:auto; padding:42px 22px 72px; }} h1 {{ font-size:clamp(2rem,5vw,3.5rem); line-height:1.08; margin:.1em 0; }}
h2 {{ margin-top:2.4em; }} .kicker {{ color:var(--accent); font-weight:700; text-transform:uppercase; letter-spacing:.09em; font-size:.78rem; }}
.sub,.muted {{ color:var(--muted); }} .grid {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(210px,1fr)); gap:14px; }}
.card, .callout {{ background:var(--panel); border:1px solid var(--line); border-radius:12px; padding:18px; }} .card b {{ display:block; font-size:1.35rem; }}
.callout {{ border-left:4px solid var(--warn); }} table {{ width:100%; border-collapse:collapse; margin-top:14px; font-size:.92rem; }} th,td {{ border-bottom:1px solid var(--line); padding:12px 10px; text-align:right; vertical-align:top; }} th:first-child,td:first-child {{ text-align:left; }}
.ok {{ color:var(--accent); font-weight:700; }} .bad {{ color:#ff8b8b; font-weight:700; }} code {{ background:#0b111a; padding:.15em .35em; border-radius:4px; }} a {{ color:#8fc5ff; }}
</style></head><body><main>
<p class="kicker">Measured locally · reproducible fixture</p><h1>rustnzb benchmark report</h1>
<p class="sub">Generated {timestamp}. This is a controlled Docker/loopback comparison of rustnzb and SABnzbd, not a claim about public Usenet-provider throughput.</p>
<div class="grid"><div class="card"><span class="muted">Scenarios</span><b>{scenario_count}</b><span>clean verification and injected-miss repair paths</span></div><div class="card"><span class="muted">Correctness gate</span><b>payload hash</b><span>each successful result must verify the reconstructed payload</span></div><div class="card"><span class="muted">Primary metric</span><b>time to usable output</b><span>terminal success plus payload verification</span></div></div>
<h2>Methodology</h2><ul><li>Both clients use the same generated NZB and deterministic mock NNTP server.</li><li>The fixture records decoded payload bytes, emitted yEnc-body bytes, article requests, served articles, and injected 430 responses.</li><li>Peak working-directory size is sampled during the run; it is not inferred from the final directory.</li><li>The clean scenario has no injected misses. The fault scenario deliberately returns one missing article and requires the PAR2/recovery path to still yield a verified payload.</li></ul>
<h2>Results</h2><div style="overflow:auto"><table><thead><tr><th>Scenario</th><th>SABnzbd outcome / time</th><th>rustnzb outcome / time</th><th>SAB wire bytes</th><th>rustnzb wire bytes</th><th>SAB requests</th><th>rustnzb requests</th><th>SAB peak work disk</th><th>rustnzb peak work disk</th></tr></thead><tbody>{rows}</tbody></table></div>
<h2>What this establishes</h2><div class="callout"><b>Scope matters.</b> These results establish the harness contract: both clients are measured against identical fixture data; rustnzb is only counted successful when its output hash verifies; and the fault leg exposes duplicate/retry traffic instead of silently treating it as a healthy download. They do not establish performance on multi-gigabyte public posts, provider latency, encrypted archives, or arbitrary production configurations.</div>
<h2>Reproduce</h2><p>From the repository root, run <code>cd benchnzb &amp;&amp; ./run.sh --scenarios verify,verify-fault</code>. Raw JSON, CSV, text summary, container logs, SVG charts, and this HTML report are written to <code>benchnzb/results/</code>.</p>
<p class="muted">Source: <a href="https://github.com/TheDancingDeveloper-org/rustnzb">TheDancingDeveloper-org/rustnzb</a>. No release, tag, or container publication is created by this report.</p>
</main></body></html>"##,
        scenario_count = results.len(),
    );
    std::fs::write(&path, document)?;
    tracing::info!("HTML: {}", path.display());
    Ok(())
}

fn client_cell(result: &ClientResult) -> String {
    let outcome = match result.outcome {
        crate::runner::BenchmarkOutcome::Succeeded if result.payload_verified => "<span class=\"ok\">verified</span>",
        crate::runner::BenchmarkOutcome::Succeeded => "<span class=\"bad\">unverified</span>",
        _ => "<span class=\"bad\">failed</span>",
    };
    format!("{outcome}<br>{:.2} s", result.total_sec)
}

fn bytes(value: u64) -> String {
    if value >= 1_048_576 {
        format!("{:.2} MiB", value as f64 / 1_048_576.0)
    } else if value >= 1024 {
        format!("{:.1} KiB", value as f64 / 1024.0)
    } else {
        format!("{value} B")
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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

#[cfg(test)]
mod tests {
    use super::escape_html;

    #[test]
    fn html_escapes_fixture_labels() {
        assert_eq!(escape_html("a<&>'\""), "a&lt;&amp;&gt;&#39;&quot;");
    }
}
