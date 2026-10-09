//! Archive extraction: RAR, 7z, TAR, ZIP.
//!
//! - RAR: Shell out to `unrar` binary
//! - 7z: Shell out to `7z`/`7zz`/`7za` binary
//! - ZIP: Uses std::fs + zip crate

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tracing::{info, warn};

#[derive(Debug, thiserror::Error)]
#[error("archive password required")]
pub(crate) struct ArchivePasswordRequired;

/// Result of an unpack operation.
#[derive(Debug)]
pub struct UnpackResult {
    pub success: bool,
    pub files_extracted: Vec<String>,
    pub output: String,
    /// Captured stderr from the extractor, retained for actionable history
    /// diagnostics when a command exits non-zero.
    pub error_output: String,
}

fn unrar_password_flag(password: Option<&str>) -> String {
    match password {
        Some(pw) => format!("-p{pw}"),
        None => "-p-".to_string(),
    }
}

fn sevenz_password_arg(password: Option<&str>) -> Option<String> {
    password.map(|pw| format!("-p{pw}"))
}

fn rar_extract_args_with_7z(
    rar_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "x".to_string(),
        "-y".to_string(),
        format!("-o{}", output_dir.display()),
        rar_file.display().to_string(),
    ];
    if let Some(flag) = sevenz_password_arg(password) {
        args.insert(2, flag);
    }
    args
}

fn rar_extract_args_with_unrar(
    rar_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> Vec<String> {
    vec![
        "x".to_string(),
        "-o+".to_string(),
        "-y".to_string(),
        unrar_password_flag(password),
        "-ai".to_string(),
        "-idp".to_string(),
        rar_file.display().to_string(),
        output_dir.display().to_string(),
    ]
}

fn sevenz_extract_args(
    archive_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "x".to_string(),
        "-y".to_string(),
        format!("-o{}", output_dir.display()),
        archive_file.display().to_string(),
    ];
    if let Some(flag) = sevenz_password_arg(password) {
        args.insert(2, flag);
    }
    args
}

/// Return the regular files currently present below an extraction directory.
/// Extractors do not expose a portable machine-readable file list, so a
/// before/after snapshot is more reliable than parsing localized console text.
fn output_files(root: &Path) -> std::io::Result<HashSet<PathBuf>> {
    let mut files = HashSet::new();
    let mut directories = vec![root.to_path_buf()];

    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(path);
            } else if file_type.is_file() {
                files.insert(path);
            }
        }
    }

    Ok(files)
}

/// Reject links and non-directory path components left by an external
/// extractor. Native formats are checked before each write; this is the
/// equivalent postcondition for unrar/7z, whose archive member lists are not
/// exposed through a stable API.
fn validate_extraction_tree(root: &Path) -> anyhow::Result<()> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                anyhow::bail!(
                    "archive extraction produced a symbolic link `{}`",
                    path.display()
                );
            }
            if file_type.is_dir() {
                directories.push(path);
            } else if !file_type.is_file() {
                anyhow::bail!(
                    "archive extraction produced unsupported path `{}`",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn newly_extracted_files(
    output_dir: &Path,
    before: &HashSet<PathBuf>,
) -> std::io::Result<Vec<String>> {
    let mut files = output_files(output_dir)?
        .difference(before)
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn safe_archive_output_path(root: &Path, name: &str, kind: &str) -> anyhow::Result<PathBuf> {
    nzb_core::path::safe_join(root, name)
        .ok_or_else(|| anyhow::anyhow!("{kind} archive contains unsafe path `{name}`"))
}

fn reject_symlinked_path(root: &Path, path: &Path) -> anyhow::Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("archive path is outside extraction directory"))?;
    let mut current = root.to_path_buf();
    if let Ok(metadata) = std::fs::symlink_metadata(&current)
        && metadata.file_type().is_symlink()
    {
        anyhow::bail!("archive extraction directory is a symbolic link");
    }

    for component in relative.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            anyhow::bail!("archive path crosses a symbolic link");
        }
        if current != path && !metadata.is_dir() {
            anyhow::bail!("archive path crosses a non-directory");
        }
    }
    Ok(())
}

/// Give everything an external extractor wrote below `root` the same modes
/// as any other file this process creates, except the other-write bit is
/// always cleared: files `(0o666 & !umask) & !0o002` and directories
/// `(0o777 & !umask) & !0o002`. A container running with umask 0 would
/// otherwise get 0666 and 0777.
///
/// unrar and 7z restore the mode bits stored in the archive, which are
/// often owner-only (0700). That makes extracted media unreadable by a
/// media server running as another user. `root` itself and symlinks are
/// left untouched. No-op on non-Unix platforms.
pub fn normalize_extracted_permissions(root: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let umask = process_umask(root.parent().unwrap_or(root));
        // Always clear other-write, even when the process umask is 0.
        let file_mode = (0o666 & !umask) & !0o002;
        let dir_mode = (0o777 & !umask) & !0o002;
        let mut directories = vec![root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                let path = entry.path();
                if file_type.is_dir() {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(dir_mode))?;
                    directories.push(path);
                } else if file_type.is_file() {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(file_mode))?;
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

/// Read the process umask and restore it before returning. `umask(2)` is
/// process-global, so the window is only the two calls.
#[cfg(unix)]
fn process_umask(_dir: &Path) -> u32 {
    let current = unsafe { libc::umask(0) };
    unsafe { libc::umask(current) };
    current
}

/// Extract RAR archives in a directory.
///
/// If `password` is `Some`, it is passed to the extractor (`-p<pw>` for unrar,
/// `-p<pw>` for 7z). When `None`, `-p-` is used to suppress password prompts.
pub async fn extract_rar(
    rar_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> anyhow::Result<UnpackResult> {
    // Prefer an unrar-capable binary. Some 7z builds (notably Alpine's
    // p7zip package) are deliberately compiled without the proprietary RAR
    // codec, so treating every 7z binary as a RAR fallback creates a late
    // post-processing failure after an otherwise successful download.
    let (bin, use_7z) = if let Some(unrar) = find_unrar() {
        (unrar, false)
    } else if let Some(sevenz) = find_7z() {
        (sevenz, true)
    } else {
        anyhow::bail!("No RAR extractor found (tried unrar, unrar-free, rar, 7z)");
    };
    extract_rar_with(&bin, use_7z, rar_file, output_dir, password).await
}

async fn extract_rar_with(
    bin: &str,
    use_7z: bool,
    rar_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> anyhow::Result<UnpackResult> {
    info!(file = %rar_file.display(), dest = %output_dir.display(), extractor = %bin, "Extracting RAR");

    std::fs::create_dir_all(output_dir)?;
    let before = output_files(output_dir)?;

    let output = if use_7z {
        // Do not pass `-p-` to 7z when no password is set. p7zip's built-in
        // RAR handler treats it like a passworded archive hint and fails on
        // valid multi-volume RAR sets.
        Command::new(bin)
            .args(rar_extract_args_with_7z(rar_file, output_dir, password))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?
    } else {
        Command::new(bin)
            .args(rar_extract_args_with_unrar(rar_file, output_dir, password))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");
    let success = output.status.success();

    if success {
        validate_extraction_tree(output_dir)?;
        normalize_extracted_permissions(output_dir)?;
    }

    if !success {
        // Detect password-protected archives (unrar exit code 255 + password prompt)
        let is_encrypted = combined.contains("Enter password")
            || combined.contains("password is incorrect")
            || combined.contains("Encrypted file");
        if is_encrypted {
            warn!(
                file = %rar_file.display(),
                "RAR extraction failed — archive is password-protected"
            );
            return Err(ArchivePasswordRequired.into());
        }
        warn!(
            file = %rar_file.display(),
            exit_code = ?output.status.code(),
            stderr = %stderr,
            "RAR extraction failed"
        );
    }

    Ok(UnpackResult {
        success,
        files_extracted: if success {
            newly_extracted_files(output_dir, &before)?
        } else {
            Vec::new()
        },
        output: stdout,
        error_output: stderr,
    })
}

/// Strings in 7z stderr/stdout that indicate a password-protected archive.
const SEVENZ_PASSWORD_PATTERNS: &[&str] = &[
    "Wrong password",
    "Can not open encrypted archive",
    "Enter password",
    "ERROR: Data Error in encrypted file",
    "password is incorrect",
];

/// Extract 7z archives by shelling out to the 7z binary.
pub async fn extract_7z(
    archive_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> anyhow::Result<UnpackResult> {
    let sevenz_bin =
        find_7z().ok_or_else(|| anyhow::anyhow!("7z/7zz/7za binary not found on PATH"))?;

    info!(file = %archive_file.display(), dest = %output_dir.display(), "Extracting 7z");

    std::fs::create_dir_all(output_dir)?;
    let before = output_files(output_dir)?;

    let output = Command::new(&sevenz_bin)
        .args(sevenz_extract_args(archive_file, output_dir, password))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");
    let success = output.status.success();

    if success {
        validate_extraction_tree(output_dir)?;
        normalize_extracted_permissions(output_dir)?;
    }

    if !success {
        let is_encrypted = SEVENZ_PASSWORD_PATTERNS
            .iter()
            .any(|p| combined.contains(p));
        if is_encrypted {
            warn!(
                file = %archive_file.display(),
                "7z extraction failed — archive is password-protected"
            );
            return Err(ArchivePasswordRequired.into());
        }
        warn!(
            file = %archive_file.display(),
            exit_code = ?output.status.code(),
            "7z extraction failed"
        );
    }

    Ok(UnpackResult {
        success,
        files_extracted: if success {
            newly_extracted_files(output_dir, &before)?
        } else {
            Vec::new()
        },
        output: stdout,
        error_output: stderr,
    })
}

/// Extract ZIP archives.
pub async fn extract_zip(zip_file: &Path, output_dir: &Path) -> anyhow::Result<UnpackResult> {
    info!(file = %zip_file.display(), dest = %output_dir.display(), "Extracting ZIP");

    // Use tokio spawn_blocking since zip extraction is CPU-bound
    let zip_path = zip_file.to_path_buf();
    let out_path = output_dir.to_path_buf();

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<UnpackResult> {
        let file = std::fs::File::open(&zip_path)?;
        let mut archive = zip::ZipArchive::new(file)?;
        let mut extracted = Vec::new();
        let mut output_paths = HashSet::new();

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            // Never materialize links from an untrusted archive. Treating a
            // link payload as a regular file also makes the policy explicit
            // on platforms where link metadata is partially supported.
            if entry.is_symlink() {
                anyhow::bail!(
                    "ZIP archive contains unsupported symbolic link `{}`",
                    entry.name()
                );
            }
            let outpath = safe_archive_output_path(&out_path, entry.name(), "ZIP")?;
            if !output_paths.insert(outpath.clone()) {
                anyhow::bail!(
                    "ZIP archive contains duplicate output path `{}`",
                    entry.name()
                );
            }

            if entry.is_dir() {
                reject_symlinked_path(&out_path, &outpath)?;
                std::fs::create_dir_all(&outpath)?;
            } else {
                reject_symlinked_path(&out_path, &outpath)?;
                if let Some(parent) = outpath.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut outfile = std::fs::File::create(&outpath)?;
                std::io::copy(&mut entry, &mut outfile)?;
                extracted.push(outpath.to_string_lossy().to_string());
            }
        }

        Ok(UnpackResult {
            success: true,
            files_extracted: extracted,
            output: String::new(),
            error_output: String::new(),
        })
    })
    .await??;

    Ok(result)
}

/// Extract a TAR archive without materializing links or paths outside the job.
/// TAR permits both symbolic and hard links; rejecting both keeps extraction
/// deterministic and prevents a later cleanup or overwrite from escaping the
/// output directory.
pub async fn extract_tar(tar_file: &Path, output_dir: &Path) -> anyhow::Result<UnpackResult> {
    info!(file = %tar_file.display(), dest = %output_dir.display(), "Extracting TAR");
    let tar_path = tar_file.to_path_buf();
    let out_path = output_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<UnpackResult> {
        let file = std::fs::File::open(&tar_path)?;
        let mut archive = tar::Archive::new(file);
        std::fs::create_dir_all(&out_path)?;
        let mut extracted = Vec::new();
        let mut output_paths = HashSet::new();

        for entry in archive.entries()? {
            let mut entry = entry?;
            let entry_path = entry.path()?.to_string_lossy().into_owned();
            let entry_type = entry.header().entry_type();
            if entry_type.is_symlink() {
                anyhow::bail!("TAR archive contains unsupported symbolic link `{entry_path}`");
            }
            if entry_type.is_hard_link() {
                anyhow::bail!("TAR archive contains unsupported hard link `{entry_path}`");
            }

            let outpath = safe_archive_output_path(&out_path, &entry_path, "TAR")?;
            if !output_paths.insert(outpath.clone()) {
                anyhow::bail!("TAR archive contains duplicate output path `{entry_path}`");
            }

            if entry_type.is_dir() {
                reject_symlinked_path(&out_path, &outpath)?;
                std::fs::create_dir_all(&outpath)?;
                continue;
            }
            if !entry_type.is_file() {
                anyhow::bail!("TAR archive contains unsupported entry `{entry_path}`");
            }

            reject_symlinked_path(&out_path, &outpath)?;
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut outfile = std::fs::File::create(&outpath)?;
            std::io::copy(&mut entry, &mut outfile)?;
            outfile.flush()?;
            extracted.push(outpath.to_string_lossy().into_owned());
        }

        Ok(UnpackResult {
            success: true,
            files_extracted: extracted,
            output: String::new(),
            error_output: String::new(),
        })
    })
    .await?
}

pub fn find_unrar() -> Option<String> {
    for name in &["unrar", "unrar-free", "rar"] {
        if which_exists(name) {
            return Some(name.to_string());
        }
    }
    None
}

/// Find the 7z binary on the system. Checks `7z`, `7zz` (7-Zip standalone),
/// and `7za` (7-Zip standalone, older naming).
pub fn find_7z() -> Option<String> {
    for name in &["7z", "7zz", "7za"] {
        if which_exists(name) {
            return Some(name.to_string());
        }
    }
    None
}

fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[tokio::test]
    async fn test_extract_zip_valid() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("test.zip");
        let out_dir = dir.path().join("out");

        // Create a real zip file
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip_writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip_writer.start_file("hello.txt", options).unwrap();
            zip_writer.write_all(b"Hello, world!").unwrap();
            zip_writer.finish().unwrap();
        }

        let result = extract_zip(&zip_path, &out_dir).await.unwrap();
        assert!(result.success);
        assert_eq!(result.files_extracted.len(), 1);
        let content = std::fs::read_to_string(out_dir.join("hello.txt")).unwrap();
        assert_eq!(content, "Hello, world!");
    }

    #[tokio::test]
    async fn test_extract_zip_nonexistent() {
        let result = extract_zip(
            Path::new("/no/such/file.zip"),
            Path::new("/tmp/nzb_test_out"),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_extract_zip_rejects_duplicate_output_paths() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("duplicate.zip");
        let out_dir = dir.path().join("out");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            writer.add_directory("same.txt/", options).unwrap();
            writer.start_file("same.txt", options).unwrap();
            writer.write_all(b"second").unwrap();
            writer.finish().unwrap();
        }

        let result = extract_zip(&zip_path, &out_dir).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("duplicate output path"), "{error}");
    }

    #[tokio::test]
    async fn test_extract_zip_rejects_parent_and_absolute_paths() {
        for (index, name) in [
            "../../outside.txt",
            "/absolute.txt",
            r"..\..\outside.txt",
            r"C:\outside.txt",
        ]
        .iter()
        .enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let zip_path = dir.path().join(format!("unsafe-{index}.zip"));
            let out_dir = dir.path().join("out");
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"outside").unwrap();
            writer.finish().unwrap();

            let result = extract_zip(&zip_path, &out_dir).await;
            let error = result.unwrap_err().to_string();
            assert!(error.contains("unsafe path"), "{error}");
            assert!(!dir.path().join("outside.txt").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_extract_zip_rejects_preexisting_symlink_ancestors() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("symlink-ancestor.zip");
        let out_dir = dir.path().join("out");
        let outside_dir = dir.path().join("outside");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, out_dir.join("link")).unwrap();

        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("link/escaped.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"outside").unwrap();
        writer.finish().unwrap();

        let result = extract_zip(&zip_path, &out_dir).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(!outside_dir.join("escaped.txt").exists());
    }

    #[tokio::test]
    async fn test_extract_zip_rejects_symbolic_links() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("symlink.zip");
        let out_dir = dir.path().join("out");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .add_symlink(
                    "link",
                    "outside.txt",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.finish().unwrap();
        }

        let result = extract_zip(&zip_path, &out_dir).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }

    fn write_tar(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut builder = tar::Builder::new(file);
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *contents).unwrap();
        }
        builder.finish().unwrap();
    }

    fn write_raw_tar(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut builder = tar::Builder::new(file);
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            let name_bytes = name.as_bytes();
            assert!(name_bytes.len() <= 100);
            header.as_mut_bytes()[..100].fill(0);
            header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *contents).unwrap();
        }
        builder.finish().unwrap();
    }

    #[tokio::test]
    async fn test_extract_tar_valid_and_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let tar_path = dir.path().join("payload.tar");
        let output = dir.path().join("output");
        let large = vec![b'x'; 128 * 1024];
        write_tar(
            &tar_path,
            &[("nested/hello.txt", b"hello"), ("large.bin", &large)],
        );

        let result = extract_tar(&tar_path, &output).await.unwrap();
        assert!(result.success);
        assert_eq!(fs::read(output.join("nested/hello.txt")).unwrap(), b"hello");
        assert_eq!(
            fs::metadata(output.join("large.bin")).unwrap().len(),
            large.len() as u64
        );
    }

    #[tokio::test]
    async fn test_extract_tar_rejects_traversal_and_duplicate_paths() {
        for (index, names) in [vec!["../../outside.txt"], vec!["same.txt", "same.txt"]]
            .into_iter()
            .enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let tar_path = dir.path().join(format!("unsafe-{index}.tar"));
            let entries: Vec<(&str, &[u8])> = names
                .iter()
                .map(|name| (*name, b"data".as_slice()))
                .collect();
            if names.iter().any(|name| name.contains("..")) {
                write_raw_tar(&tar_path, &entries);
            } else {
                write_tar(&tar_path, &entries);
            }
            let error = extract_tar(&tar_path, &dir.path().join("output"))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("unsafe path") || error.contains("duplicate output path"),
                "{error}"
            );
            assert!(!dir.path().join("outside.txt").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_extract_tar_rejects_symlink_and_hardlink_entries() {
        let dir = tempfile::tempdir().unwrap();
        let tar_path = dir.path().join("links.tar");
        let file = fs::File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut symlink_header = tar::Header::new_gnu();
        symlink_header.set_entry_type(tar::EntryType::Symlink);
        symlink_header.set_path("link").unwrap();
        symlink_header.set_link_name("outside").unwrap();
        symlink_header.set_size(0);
        symlink_header.set_cksum();
        builder.append(&symlink_header, &[][..]).unwrap();
        builder.finish().unwrap();
        let error = extract_tar(&tar_path, &dir.path().join("output"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("symbolic link"), "{error}");

        let hardlink_path = dir.path().join("hardlink.tar");
        let file = fs::File::create(&hardlink_path).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut hardlink_header = tar::Header::new_gnu();
        hardlink_header.set_entry_type(tar::EntryType::Link);
        hardlink_header.set_path("copy").unwrap();
        hardlink_header.set_link_name("original").unwrap();
        hardlink_header.set_size(0);
        hardlink_header.set_cksum();
        builder.append(&hardlink_header, &[][..]).unwrap();
        builder.finish().unwrap();
        let error = extract_tar(&hardlink_path, &dir.path().join("hardlink-output"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("hard link"), "{error}");
    }

    #[test]
    fn test_unpack_result_fields() {
        let result = UnpackResult {
            success: true,
            files_extracted: vec!["file1.txt".to_string()],
            output: "OK".to_string(),
            error_output: String::new(),
        };
        assert!(result.success);
        assert_eq!(result.files_extracted.len(), 1);
    }

    #[test]
    fn sevenz_password_arg_is_omitted_without_password() {
        assert_eq!(sevenz_password_arg(None), None);
        assert_eq!(
            sevenz_password_arg(Some("secret")).as_deref(),
            Some("-psecret")
        );
    }

    /// A stand-in for unrar that, like the real one, restores the archive's
    /// stored owner-only modes on what it extracts.
    #[cfg(unix)]
    fn owner_only_fake_unrar(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-unrar");
        fs::write(
            &path,
            "#!/bin/sh\nfor a; do out=$a; done\nmkdir -p \"$out/Season 1\"\n\
             printf x > \"$out/Season 1/episode.mkv\"\nprintf y > \"$out/movie.mkv\"\n\
             chmod 700 \"$out/Season 1/episode.mkv\" \"$out/movie.mkv\" \"$out/Season 1\"\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rar_extraction_normalises_owner_only_modes() {
        let tools = tempfile::tempdir().unwrap();
        let unrar = owner_only_fake_unrar(tools.path());
        let work = tempfile::tempdir().unwrap();
        let rar = work.path().join("release.rar");
        fs::write(&rar, b"Rar!").unwrap();
        let out = work.path().join("out");

        let result = extract_rar_with(unrar.to_str().unwrap(), false, &rar, &out, None)
            .await
            .unwrap();
        assert!(result.success, "{}", result.error_output);
        assert_eq!(result.files_extracted.len(), 2);

        // umask 077 is common in CI, so compare against the computed mode
        // rather than a freshly created probe file.
        let umask = process_umask(&out);
        assert_eq!(mode(&out.join("movie.mkv")), (0o666 & !umask) & !0o002);
        assert_eq!(
            mode(&out.join("Season 1/episode.mkv")),
            (0o666 & !umask) & !0o002
        );
        assert_eq!(mode(&out.join("Season 1")), (0o777 & !umask) & !0o002);
        assert_eq!(
            mode(&out.join("movie.mkv")) & 0o002,
            0,
            "never world-writable"
        );
    }

    /// umask 0 must not produce 0666/0777. The process umask is global, so it
    /// is set only for this call and restored before the assert can panic.
    #[cfg(unix)]
    #[test]
    fn extracted_permissions_clear_other_write_even_with_umask_zero() {
        use std::os::unix::fs::PermissionsExt;

        struct RestoreUmask(u32);
        impl Drop for RestoreUmask {
            fn drop(&mut self) {
                unsafe { libc::umask(self.0) };
            }
        }
        let _restore = RestoreUmask(unsafe { libc::umask(0) });
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("out");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/file.bin"), b"x").unwrap();
        fs::set_permissions(root.join("dir"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("dir/file.bin"), fs::Permissions::from_mode(0o600)).unwrap();
        normalize_extracted_permissions(&root).unwrap();
        assert_eq!(mode(&root.join("dir/file.bin")), 0o664);
        assert_eq!(mode(&root.join("dir")), 0o775);
    }

    #[test]
    fn rar_extract_args_keep_dash_password_only_for_unrar() {
        let rar = Path::new("/tmp/test.rar");
        let out = Path::new("/tmp/out");

        let sevenz_args = rar_extract_args_with_7z(rar, out, None);
        assert!(!sevenz_args.iter().any(|arg| arg == "-p-"));

        let unrar_args = rar_extract_args_with_unrar(rar, out, None);
        assert!(unrar_args.iter().any(|arg| arg == "-p-"));
    }

    #[test]
    fn sevenz_extract_args_do_not_include_dash_password_without_password() {
        let archive = Path::new("/tmp/test.7z");
        let out = Path::new("/tmp/out");

        let args = sevenz_extract_args(archive, out, None);
        assert!(!args.iter().any(|arg| arg == "-p-"));

        let args = sevenz_extract_args(archive, out, Some("secret"));
        assert!(args.iter().any(|arg| arg == "-psecret"));
    }

    #[test]
    fn password_failure_survives_as_a_typed_error() {
        let error: anyhow::Error = ArchivePasswordRequired.into();
        assert!(error.downcast_ref::<ArchivePasswordRequired>().is_some());
    }
}
