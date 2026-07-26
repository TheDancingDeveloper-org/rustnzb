use crate::config::{self, Scenario, TestType, ARTICLE_SIZE, GB, MB, MSG_ID_DOMAIN};
use crate::mock_nntp::{ArticleIndex, FileEntry};
use crate::nzb::{self, NzbFile};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Maximum time to wait for a single par2 create subprocess (10 minutes).
const PAR2_TIMEOUT_SECS: u64 = 10 * 60;

pub async fn prepare_data(scenarios: &[Scenario], data_dir: &Path) -> Result<()> {
    let testdata_dir = data_dir.join("testdata");
    let nzb_dir = data_dir.join("nzbs");
    tokio::fs::create_dir_all(&testdata_dir).await?;
    tokio::fs::create_dir_all(&nzb_dir).await?;

    // Pre-flight: check disk space and memory before generating data
    preflight_check(scenarios, data_dir).await?;

    let mut all_entries: Vec<FileEntry> = Vec::new();
    let mut all_missing: Vec<String> = Vec::new();

    for sc in scenarios {
        let label = config::size_label(sc.total_size);
        tracing::info!("Preparing: {} — {}", sc.name, sc.description);

        // Raw data file (shared across test types for same size)
        let raw_file = testdata_dir.join(format!("bench_{label}.bin"));
        ensure_file(&raw_file, sc.total_size).await?;

        match sc.test_type {
            TestType::Raw => {
                let prefix = format!("d-{label}-raw-f000");
                all_entries.push(FileEntry {
                    msg_prefix: prefix.clone(),
                    data_file: raw_file.to_string_lossy().to_string(),
                    filename: format!("bench_{label}.bin"),
                    total_size: sc.total_size,
                });

                let segments = nzb::build_segments(&prefix, sc.total_size);
                let nzb_files = vec![NzbFile {
                    filename: format!("bench_{label}.bin"),
                    segments,
                }];
                let nzb_xml = nzb::generate_nzb(&nzb_files, "bench@benchnzb");
                let nzb_path = nzb_dir.join(format!("{}.nzb", sc.name));
                tokio::fs::write(&nzb_path, &nzb_xml).await?;
                tracing::info!("  NZB: {}", nzb_path.display());
            }
            TestType::Par2 => {
                let data_prefix = format!("d-{label}-par2-f000");
                all_entries.push(FileEntry {
                    msg_prefix: data_prefix.clone(),
                    data_file: raw_file.to_string_lossy().to_string(),
                    filename: format!("bench_{label}.bin"),
                    total_size: sc.total_size,
                });

                // Mark some data articles as missing (evenly spaced)
                let total_parts = sc.total_size.div_ceil(ARTICLE_SIZE) as u32;
                let missing_count = (total_parts as f64 * sc.missing_pct / 100.0) as u32;
                if missing_count > 0 {
                    let step = total_parts / missing_count;
                    for i in 0..missing_count {
                        let part = i * step + step / 2 + 1;
                        if part <= total_parts {
                            all_missing.push(format!("{data_prefix}-p{part:05}@{MSG_ID_DOMAIN}"));
                        }
                    }
                }

                let mut nzb_files = vec![NzbFile {
                    filename: format!("bench_{label}.bin"),
                    segments: nzb::build_segments(&data_prefix, sc.total_size),
                }];

                // Generate par2 recovery files
                let par2_files = generate_par2(&raw_file, sc.redundancy_pct).await?;
                for (idx, par2_path) in par2_files.iter().enumerate() {
                    let par2_size = tokio::fs::metadata(par2_path).await?.len();
                    let par2_name = par2_path.file_name().unwrap().to_string_lossy().to_string();
                    let par2_prefix = format!("d-{label}-par2-par{idx:02}");

                    all_entries.push(FileEntry {
                        msg_prefix: par2_prefix.clone(),
                        data_file: par2_path.to_string_lossy().to_string(),
                        filename: par2_name.clone(),
                        total_size: par2_size,
                    });
                    nzb_files.push(NzbFile {
                        filename: par2_name,
                        segments: nzb::build_segments(&par2_prefix, par2_size),
                    });
                }

                let nzb_xml = nzb::generate_nzb(&nzb_files, "bench@benchnzb");
                let nzb_path = nzb_dir.join(format!("{}.nzb", sc.name));
                tokio::fs::write(&nzb_path, &nzb_xml).await?;
                tracing::info!(
                    "  NZB: {} ({} missing articles)",
                    nzb_path.display(),
                    missing_count
                );
            }
            TestType::Unpack => {
                // Create 7z archive (store mode, no compression)
                let archive = create_7z_archive(&raw_file, &testdata_dir).await?;
                let archive_size = tokio::fs::metadata(&archive).await?.len();
                let archive_name = archive.file_name().unwrap().to_string_lossy().to_string();

                let archive_prefix = format!("d-{label}-unpack-f000");
                all_entries.push(FileEntry {
                    msg_prefix: archive_prefix.clone(),
                    data_file: archive.to_string_lossy().to_string(),
                    filename: archive_name.clone(),
                    total_size: archive_size,
                });

                let mut nzb_files = vec![NzbFile {
                    filename: archive_name,
                    segments: nzb::build_segments(&archive_prefix, archive_size),
                }];

                // Par2 for the archive
                let par2_files = generate_par2(&archive, sc.redundancy_pct).await?;
                for (idx, par2_path) in par2_files.iter().enumerate() {
                    let par2_size = tokio::fs::metadata(par2_path).await?.len();
                    let par2_name = par2_path.file_name().unwrap().to_string_lossy().to_string();
                    let par2_prefix = format!("d-{label}-unpack-par{idx:02}");

                    all_entries.push(FileEntry {
                        msg_prefix: par2_prefix.clone(),
                        data_file: par2_path.to_string_lossy().to_string(),
                        filename: par2_name.clone(),
                        total_size: par2_size,
                    });
                    nzb_files.push(NzbFile {
                        filename: par2_name,
                        segments: nzb::build_segments(&par2_prefix, par2_size),
                    });
                }

                let nzb_xml = nzb::generate_nzb(&nzb_files, "bench@benchnzb");
                let nzb_path = nzb_dir.join(format!("{}.nzb", sc.name));
                tokio::fs::write(&nzb_path, &nzb_xml).await?;
                tracing::info!("  NZB: {}", nzb_path.display());
            }
        }
    }

    // Write article index for mock-nntp
    let index = ArticleIndex {
        article_size: ARTICLE_SIZE,
        entries: all_entries,
        missing: all_missing,
    };
    let index_json = serde_json::to_string_pretty(&index)?;
    let index_path = data_dir.join("articles.json");
    tokio::fs::write(&index_path, &index_json).await?;
    tracing::info!(
        "Article index: {} entries, {} missing",
        index.entries.len(),
        index.missing.len()
    );

    Ok(())
}

async fn ensure_file(path: &Path, size: u64) -> Result<()> {
    if let Ok(meta) = tokio::fs::metadata(path).await {
        if meta.len() == size {
            tracing::info!("  Exists: {} ({} MB)", path.display(), size / MB);
            return Ok(());
        }
    }
    tracing::info!("  Generating {} ({} MB)...", path.display(), size / MB);
    generate_random_file(path, size).await
}

async fn generate_random_file(path: &Path, size: u64) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use rand::RngCore as _;
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f =
            std::io::BufWriter::with_capacity(4 * MB as usize, std::fs::File::create(&path)?);
        let mut rng = rand::rng();
        let chunk = 4 * MB as usize;
        let mut buf = vec![0u8; chunk];
        let mut written: u64 = 0;
        let start = std::time::Instant::now();

        while written < size {
            let n = std::cmp::min(chunk as u64, size - written) as usize;
            rng.fill_bytes(&mut buf[..n]);
            f.write_all(&buf[..n])?;
            written += n as u64;
            if size >= GB && written % (512 * MB) == 0 {
                let pct = written as f64 * 100.0 / size as f64;
                let rate = written as f64 / MB as f64 / start.elapsed().as_secs_f64();
                eprint!("\r    {pct:.0}% ({rate:.0} MB/s)");
            }
        }
        f.flush()?;
        if size >= GB {
            eprintln!();
        }
        tracing::info!(
            "Generated {} ({} MB) in {:.1}s",
            path.display(),
            size / MB,
            start.elapsed().as_secs_f64()
        );
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

async fn generate_par2(data_file: &Path, redundancy_pct: f64) -> Result<Vec<PathBuf>> {
    let par2_index = PathBuf::from(format!("{}.par2", data_file.display()));
    if par2_index.exists() {
        let files = collect_par2_files(data_file).await?;
        tracing::info!(
            "  Par2 cached for {} ({} file(s))",
            data_file.display(),
            files.len()
        );
        return Ok(files);
    }

    let redundancy = redundancy_pct as u32;
    let file_size_mb = tokio::fs::metadata(data_file).await?.len() / MB;

    // Log par2 version on first call
    let ver_output = tokio::process::Command::new("par2")
        .arg("--version")
        .output()
        .await;
    if let Ok(out) = ver_output {
        let ver = String::from_utf8_lossy(&out.stdout);
        let ver_line = ver.lines().next().unwrap_or("unknown");
        tracing::info!("  par2 binary: {ver_line}");
    }

    tracing::info!(
        "  Creating par2 ({}% redundancy) for {} ({} MB, timeout {}s)...",
        redundancy,
        data_file.display(),
        file_size_mb,
        PAR2_TIMEOUT_SECS
    );

    // Spawn par2 — par2cmdline-turbo auto-detects thread count
    // Set block size = article size so each missing article maps to exactly one par2
    // source block.  Without this, par2 auto-selects a large block size for big files,
    // producing too few recovery blocks to cover 3% missing articles at 8% redundancy.
    use tokio::process::Command;
    let mut child = Command::new("par2")
        .args([
            "create",
            &format!("-r{redundancy}"),
            &format!("-s{}", config::ARTICLE_SIZE),
        ])
        .arg(data_file)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn par2: {e}"))?;

    let timeout = std::time::Duration::from_secs(PAR2_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            if !status.success() {
                anyhow::bail!("par2 create failed with exit code {:?}", status.code());
            }
        }
        Ok(Err(e)) => {
            anyhow::bail!("par2 process error: {e}");
        }
        Err(_) => {
            tracing::error!(
                "  par2 create TIMED OUT after {}s for {} — killing process",
                PAR2_TIMEOUT_SECS,
                data_file.display()
            );
            let _ = child.kill().await;
            // Clean up partial par2 files
            let _ = cleanup_partial_par2(data_file).await;
            anyhow::bail!(
                "par2 create timed out after {}s for {}",
                PAR2_TIMEOUT_SECS,
                data_file.display()
            );
        }
    }

    collect_par2_files(data_file).await
}

/// Remove partial par2 files left behind by a timed-out or failed par2 create.
async fn cleanup_partial_par2(data_file: &Path) -> Result<()> {
    let dir = data_file.parent().unwrap_or(Path::new("."));
    let stem = data_file.file_name().unwrap().to_string_lossy().to_string();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&stem) && name.ends_with(".par2") {
            tracing::warn!("  Removing partial par2 file: {name}");
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

async fn collect_par2_files(data_file: &Path) -> Result<Vec<PathBuf>> {
    let dir = data_file.parent().unwrap_or(Path::new("."));
    let stem = data_file.file_name().unwrap().to_string_lossy().to_string();

    let mut par2_files = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&stem) && name.ends_with(".par2") {
            par2_files.push(entry.path());
        }
    }
    par2_files.sort();
    tracing::info!("  Found {} par2 file(s)", par2_files.len());
    Ok(par2_files)
}

async fn create_7z_archive(data_file: &Path, output_dir: &Path) -> Result<PathBuf> {
    let stem = data_file.file_stem().unwrap().to_string_lossy().to_string();
    let archive = output_dir.join(format!("{stem}.7z"));

    if archive.exists() {
        tracing::info!("  7z archive exists: {}", archive.display());
        return Ok(archive);
    }

    tracing::info!("  Creating 7z archive (store mode): {}", archive.display());
    let output = tokio::process::Command::new("7z")
        .args(["a", "-mx0"])
        .arg(&archive)
        .arg(data_file)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("7z create failed: {stderr}");
    }

    let size = tokio::fs::metadata(&archive).await?.len();
    tracing::info!("  Archive: {} ({} MB)", archive.display(), size / MB);
    Ok(archive)
}

/// Get NZB file path for a scenario.
pub fn nzb_path(sc: &Scenario, data_dir: &Path) -> PathBuf {
    data_dir.join("nzbs").join(format!("{}.nzb", sc.name))
}

/// Pre-flight check: verify sufficient disk space and memory before data generation.
async fn preflight_check(scenarios: &[Scenario], data_dir: &Path) -> Result<()> {
    // Estimate required disk: each size needs raw + 7z (same size) + par2 (~30%)
    // Unique sizes to avoid double-counting shared raw files
    let mut unique_sizes: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut needs_par2 = false;
    let mut needs_7z = false;
    for sc in scenarios {
        unique_sizes.insert(sc.total_size);
        if matches!(sc.test_type, TestType::Par2) {
            needs_par2 = true;
        }
        if matches!(sc.test_type, TestType::Unpack) {
            needs_7z = true;
        }
    }

    let raw_total: u64 = unique_sizes.iter().sum();
    let mut estimated_bytes = raw_total; // raw bin files
    if needs_7z {
        estimated_bytes += raw_total; // 7z archives (store mode ≈ same size)
    }
    if needs_par2 || needs_7z {
        // par2 at 30% redundancy for raw and/or 7z
        let par2_sources = if needs_7z { raw_total * 2 } else { raw_total };
        estimated_bytes += (par2_sources as f64 * 0.12) as u64; // ~12% for par2 overhead (8% + index)
    }

    let estimated_gb = estimated_bytes as f64 / GB as f64;
    tracing::info!(
        "Pre-flight: estimated {:.1} GB disk needed for test data",
        estimated_gb
    );

    // Check available disk space
    let disk_available = get_available_disk_bytes(data_dir).await;
    if let Some(avail) = disk_available {
        let avail_gb = avail as f64 / GB as f64;
        tracing::info!(
            "Pre-flight: {:.1} GB disk available at {}",
            avail_gb,
            data_dir.display()
        );
        if avail < estimated_bytes {
            anyhow::bail!(
                "Insufficient disk space: need ~{:.1} GB but only {:.1} GB available at {}",
                estimated_gb,
                avail_gb,
                data_dir.display()
            );
        }
        if avail < estimated_bytes * 12 / 10 {
            tracing::warn!(
                "Pre-flight: disk space is tight — {:.1} GB available, ~{:.1} GB needed",
                avail_gb,
                estimated_gb
            );
        }
    }

    // Check available memory — par2 is memory-intensive
    let mem_available = get_available_memory_bytes().await;
    if let Some(avail) = mem_available {
        let avail_mb = avail / MB;
        tracing::info!("Pre-flight: {} MB memory available", avail_mb);

        // par2 for large files needs roughly 1 GB per 5 GB of input
        let max_file_size = unique_sizes.iter().max().copied().unwrap_or(0);
        let estimated_par2_mem = max_file_size / 5;
        if (needs_par2 || needs_7z) && avail < estimated_par2_mem {
            tracing::warn!(
                    "Pre-flight: LOW MEMORY — par2 for {:.1} GB files may need ~{} MB, only {} MB available. \
                     par2 may be OOM-killed.",
                    max_file_size as f64 / GB as f64,
                    estimated_par2_mem / MB,
                    avail_mb
                );
        }
    }

    Ok(())
}

async fn get_available_disk_bytes(path: &Path) -> Option<u64> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // Use statvfs via nix or fallback to df
        let output = std::process::Command::new("df")
            .args(["--output=avail", "-B1"])
            .arg(&path)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Second line has the value
        stdout.lines().nth(1)?.trim().parse::<u64>().ok()
    })
    .await
    .ok()
    .flatten()
}

async fn get_available_memory_bytes() -> Option<u64> {
    // Read MemAvailable from /proc/meminfo
    let content = tokio::fs::read_to_string("/proc/meminfo").await.ok()?;
    for line in content.lines() {
        if line.starts_with("MemAvailable:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                // Value is in kB
                let kb = parts[1].parse::<u64>().ok()?;
                return Some(kb * 1024);
            }
        }
    }
    None
}
