//! Archive extraction: RAR, 7z, ZIP.
//!
//! - RAR: Shell out to `unrar` binary
//! - 7z: Shell out to `7z`/`7zz`/`7za` binary
//! - ZIP: Uses std::fs + zip crate

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tracing::{info, warn};

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

    info!(file = %rar_file.display(), dest = %output_dir.display(), extractor = %bin, "Extracting RAR");

    std::fs::create_dir_all(output_dir)?;
    let before = output_files(output_dir)?;

    let output = if use_7z {
        // Do not pass `-p-` to 7z when no password is set. p7zip's built-in
        // RAR handler treats it like a passworded archive hint and fails on
        // valid multi-volume RAR sets.
        Command::new(&bin)
            .args(rar_extract_args_with_7z(rar_file, output_dir, password))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?
    } else {
        Command::new(&bin)
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
            anyhow::bail!("archive is password-protected");
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

    if !success {
        let is_encrypted = SEVENZ_PASSWORD_PATTERNS
            .iter()
            .any(|p| combined.contains(p));
        if is_encrypted {
            warn!(
                file = %archive_file.display(),
                "7z extraction failed — archive is password-protected"
            );
            anyhow::bail!("archive is password-protected");
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

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let outpath = out_path.join(entry.mangled_name());

            if entry.is_dir() {
                std::fs::create_dir_all(&outpath)?;
            } else {
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
}
