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

/// How an archive password reaches the extractor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasswordArg {
    /// No password is known.
    None,
    /// Written to the extractor's stdin (one line, then EOF), so it never
    /// appears in `ps` or `/proc/<pid>/cmdline`.
    Stdin,
    /// Passed as `-p<password>`, visible to other local users through `ps`
    /// and `/proc/<pid>/cmdline`. Used only for extractors that are not known
    /// to read a piped password (see `docs/KNOWN_ISSUES.md`).
    Argv(String),
}

impl PasswordArg {
    /// The password to write to stdin, if any.
    fn stdin_password<'a>(&self, password: Option<&'a str>) -> Option<&'a str> {
        match self {
            Self::Stdin => password,
            _ => None,
        }
    }

    /// How many password prompts the caller answers itself.
    fn answered_prompts(&self) -> usize {
        usize::from(*self == Self::Stdin)
    }
}

/// Choose how to hand `password` to the extractor `bin`.
async fn password_arg_for(bin: &str, password: Option<&str>) -> PasswordArg {
    let Some(password) = password.filter(|pw| !pw.is_empty()) else {
        return PasswordArg::None;
    };
    // A password is read as one line, so one containing a line break can only
    // be passed as an argument.
    if !password.contains(['\n', '\r']) {
        let probe_bin = bin.to_string();
        if tokio::task::spawn_blocking(move || extractor_reads_password_from_stdin(&probe_bin))
            .await
            .unwrap_or(false)
        {
            return PasswordArg::Stdin;
        }
    }
    PasswordArg::Argv(password.to_string())
}

/// Whether the extractor `bin` reads a password from a piped stdin when it
/// prompts for one (unrar given `-p` with no value; 7-Zip on an encrypted
/// archive), instead of from the terminal.
///
/// Decided from the banner it prints when run with no arguments, and cached
/// per binary. Verified behaviour: rarlab UNRAR 6.21 and 7.20 and 7-Zip 25.01
/// read the piped line even with a controlling terminal. Older unrar
/// releases, unrar-free and p7zip 16.02 (which uses `getpass`, i.e. the
/// terminal) are treated as unsupported and get the password as an argument.
pub fn extractor_reads_password_from_stdin(bin: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&known) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(bin) {
        return known;
    }
    let supported = std::process::Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map(|output| {
            let mut banner = String::from_utf8_lossy(&output.stdout).into_owned();
            banner.push('\n');
            banner.push_str(&String::from_utf8_lossy(&output.stderr));
            banner_reads_password_from_stdin(&banner)
        })
        .unwrap_or(false);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(bin.to_string(), supported);
    supported
}

/// Parse an extractor banner: rarlab `UNRAR`/`RAR` 6 or later, or 7-Zip 21
/// or later (the first 7-Zip release for Linux; not p7zip).
fn banner_reads_password_from_stdin(banner: &str) -> bool {
    if banner.contains("p7zip") {
        return false;
    }
    for line in banner.lines() {
        let line = line.trim();
        let (rest, min_major) = if let Some(rest) = line
            .strip_prefix("UNRAR ")
            .or_else(|| line.strip_prefix("RAR "))
        {
            (rest, 6)
        } else if let Some(rest) = line.strip_prefix("7-Zip") {
            (rest, 21)
        } else {
            continue;
        };
        let major = rest.split_whitespace().find_map(|token| {
            token
                .split_once('.')
                .and_then(|(major, _)| major.parse::<u32>().ok())
        });
        if let Some(major) = major {
            return major >= min_major;
        }
    }
    false
}

/// Run an extractor, writing `stdin_password` (if any) as a single line to
/// its stdin and then closing it. Closing matters: a wrong password makes the
/// extractor fail or hit EOF on a re-prompt instead of waiting for input.
async fn run_extractor(
    bin: &str,
    args: Vec<String>,
    stdin_password: Option<&str>,
) -> std::io::Result<std::process::Output> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Some(password) = stdin_password else {
        return command.stdin(Stdio::null()).output().await;
    };
    let mut child = command.stdin(Stdio::piped()).kill_on_drop(true).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        // The line is far below the pipe buffer size, so this cannot block on
        // an extractor that is busy writing output. An extractor that never
        // asks (an unencrypted 7z) may already have exited: ignore EPIPE.
        let _ = stdin.write_all(format!("{password}\n").as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    child.wait_with_output().await
}

/// The `-p` switch for unrar: `-p-` (never prompt) without a password, bare
/// `-p` (prompt, answered on stdin) or `-p<pw>`.
fn unrar_password_flag(password: &PasswordArg) -> String {
    match password {
        PasswordArg::None => "-p-".to_string(),
        PasswordArg::Stdin => "-p".to_string(),
        PasswordArg::Argv(pw) => format!("-p{pw}"),
    }
}

/// The `-p` switch for 7z. Without a password and in stdin mode there is
/// none: 7z prompts by itself when the archive turns out to be encrypted.
fn sevenz_password_arg(password: &PasswordArg) -> Option<String> {
    match password {
        PasswordArg::Argv(pw) => Some(format!("-p{pw}")),
        PasswordArg::None | PasswordArg::Stdin => None,
    }
}

fn rar_extract_args_with_7z(
    rar_file: &Path,
    output_dir: &Path,
    password: &PasswordArg,
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
    password: &PasswordArg,
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
    password: &PasswordArg,
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

/// Whether the extractor asked for a password more often than we answered:
/// with `-p` on stdin, unrar and 7z always print one prompt we answer.
fn unanswered_password_prompt(output: &str, password: &PasswordArg) -> bool {
    output.matches("Enter password").count() > password.answered_prompts()
}

/// Whether a failed unrar run was a missing or wrong password. unrar 6/7
/// print `Incorrect password for <file>` and exit with code 11 (RARX_BADPWD).
fn unrar_password_failure(exit_code: Option<i32>, output: &str, password: &PasswordArg) -> bool {
    exit_code == Some(11)
        || output.contains("Incorrect password")
        || output.contains("password is incorrect")
        || output.contains("Encrypted file")
        || unanswered_password_prompt(output, password)
}

/// Whether a failed 7z run was a missing or wrong password.
fn sevenz_password_failure(output: &str, password: &PasswordArg) -> bool {
    SEVENZ_PASSWORD_PATTERNS
        .iter()
        .any(|pattern| output.contains(pattern))
        || unanswered_password_prompt(output, password)
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
        normalize_with_umask(root, read_process_umask())?;
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

/// Apply `umask` to the modes of files and directories under `root`.
///
/// Split out from [`normalize_extracted_permissions`] so tests can pass an
/// umask without calling `umask(2)`, which is process-global and would let
/// another thread create a world-writable file in the window.
#[cfg(unix)]
pub fn normalize_with_umask(root: &Path, umask: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

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
    Ok(())
}

/// The process umask, read from `/proc/self/status` so `umask(2)` is never
/// called after startup. Falls back to `0o022` when the file is unreadable
/// or has no `Umask:` line (non-Linux Unix).
#[cfg(unix)]
fn read_process_umask() -> u32 {
    const FALLBACK: u32 = 0o022;
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return FALLBACK;
    };
    for line in status.lines() {
        let Some(value) = line.strip_prefix("Umask:") else {
            continue;
        };
        let value = value.trim();
        if let Ok(mask) = u32::from_str_radix(value, 8) {
            return mask & 0o777;
        }
    }
    FALLBACK
}

/// Extract RAR archives in a directory.
///
/// If `password` is `Some`, it is written to the extractor's stdin when the
/// extractor is known to read it from there (unrar 6+, 7-Zip 21+), so it is
/// not visible in the process list; older extractors get `-p<pw>`. When
/// `None`, unrar gets `-p-` to suppress password prompts.
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

    let password_arg = password_arg_for(bin, password).await;
    let args = if use_7z {
        // Do not pass `-p-` to 7z when no password is set. p7zip's built-in
        // RAR handler treats it like a passworded archive hint and fails on
        // valid multi-volume RAR sets.
        rar_extract_args_with_7z(rar_file, output_dir, &password_arg)
    } else {
        rar_extract_args_with_unrar(rar_file, output_dir, &password_arg)
    };
    let output = run_extractor(bin, args, password_arg.stdin_password(password)).await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");
    let success = output.status.success();

    if success {
        validate_extraction_tree(output_dir)?;
        normalize_extracted_permissions(output_dir)?;
    }

    if !success {
        let is_encrypted = if use_7z {
            sevenz_password_failure(&combined, &password_arg)
        } else {
            unrar_password_failure(output.status.code(), &combined, &password_arg)
        };
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

/// Strings in 7z stderr/stdout that indicate a missing or wrong password.
/// The `Enter password` prompt is handled separately: in stdin mode 7z
/// prints it once for every encrypted archive, whatever the outcome.
const SEVENZ_PASSWORD_PATTERNS: &[&str] = &[
    "Wrong password",
    "Can not open encrypted archive",
    "Cannot open encrypted archive",
    "ERROR: Data Error in encrypted file",
    "password is incorrect",
];

/// Extract 7z archives by shelling out to the 7z binary. A password is fed
/// on stdin where the binary supports it, as for [`extract_rar`].
pub async fn extract_7z(
    archive_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> anyhow::Result<UnpackResult> {
    let sevenz_bin =
        find_7z().ok_or_else(|| anyhow::anyhow!("7z/7zz/7za binary not found on PATH"))?;
    extract_7z_with(&sevenz_bin, archive_file, output_dir, password).await
}

async fn extract_7z_with(
    sevenz_bin: &str,
    archive_file: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> anyhow::Result<UnpackResult> {
    info!(file = %archive_file.display(), dest = %output_dir.display(), "Extracting 7z");

    std::fs::create_dir_all(output_dir)?;
    let before = output_files(output_dir)?;

    let password_arg = password_arg_for(sevenz_bin, password).await;
    let output = run_extractor(
        sevenz_bin,
        sevenz_extract_args(archive_file, output_dir, &password_arg),
        password_arg.stdin_password(password),
    )
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
        if sevenz_password_failure(&combined, &password_arg) {
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
        assert_eq!(sevenz_password_arg(&PasswordArg::None), None);
        assert_eq!(sevenz_password_arg(&PasswordArg::Stdin), None);
        assert_eq!(
            sevenz_password_arg(&PasswordArg::Argv("secret".into())).as_deref(),
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
        let umask = read_process_umask();
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

    /// umask 0 must not produce 0666/0777. The umask is passed in, so the test
    /// never calls `umask(2)` and cannot change it for other threads.
    #[cfg(unix)]
    #[test]
    fn extracted_permissions_clear_other_write_even_with_umask_zero() {
        use std::os::unix::fs::PermissionsExt;

        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("out");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/file.bin"), b"x").unwrap();
        fs::set_permissions(root.join("dir"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("dir/file.bin"), fs::Permissions::from_mode(0o600)).unwrap();
        normalize_with_umask(&root, 0).unwrap();
        assert_eq!(mode(&root.join("dir/file.bin")), 0o664);
        assert_eq!(mode(&root.join("dir")), 0o775);
    }

    #[test]
    fn rar_extract_args_keep_dash_password_only_for_unrar() {
        let rar = Path::new("/tmp/test.rar");
        let out = Path::new("/tmp/out");

        let sevenz_args = rar_extract_args_with_7z(rar, out, &PasswordArg::None);
        assert!(!sevenz_args.iter().any(|arg| arg == "-p-"));

        let unrar_args = rar_extract_args_with_unrar(rar, out, &PasswordArg::None);
        assert!(unrar_args.iter().any(|arg| arg == "-p-"));
    }

    #[test]
    fn sevenz_extract_args_do_not_include_dash_password_without_password() {
        let archive = Path::new("/tmp/test.7z");
        let out = Path::new("/tmp/out");

        let args = sevenz_extract_args(archive, out, &PasswordArg::None);
        assert!(!args.iter().any(|arg| arg == "-p-"));

        let args = sevenz_extract_args(archive, out, &PasswordArg::Argv("secret".into()));
        assert!(args.iter().any(|arg| arg == "-psecret"));
    }

    #[test]
    fn password_failure_survives_as_a_typed_error() {
        let error: anyhow::Error = ArchivePasswordRequired.into();
        assert!(error.downcast_ref::<ArchivePasswordRequired>().is_some());
    }

    // -- Archive passwords stay off the command line ----------------------

    /// A stand-in extractor. Invoked with no arguments it prints `banner`
    /// (the version probe); otherwise it runs `body`. `$secret_in_argv` is
    /// `yes` when any argument contains the test password.
    #[cfg(unix)]
    fn fake_extractor(dir: &Path, name: &str, banner: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ $# -eq 0 ]; then printf '%s\\n' '{banner}'; exit 0; fi\n\
                 secret_in_argv=no\nfor a; do case \"$a\" in *s3cr3t*) secret_in_argv=yes;; esac; out=$a; done\n\
                 {body}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Reads the password from stdin like unrar >= 6 / 7-Zip >= 21 do, and
    /// fails if it was also passed as an argument.
    #[cfg(unix)]
    const STDIN_PASSWORD_BODY: &str = "[ \"$secret_in_argv\" = no ] || { echo 'password leaked into argv' >&2; exit 7; }\n\
         printf 'Enter password (will not be echoed): ' >&2\n\
         IFS= read -r pw\n\
         if [ \"$pw\" = 's3cr3t pw' ]; then mkdir -p \"$out\"; printf x > \"$out/movie.mkv\"; echo 'All OK'; exit 0; fi\n\
         echo 'Incorrect password for movie.mkv' >&2; exit 11";

    #[cfg(unix)]
    async fn run_rar(
        unrar: &Path,
        use_7z: bool,
        password: Option<&str>,
    ) -> anyhow::Result<UnpackResult> {
        let work = tempfile::tempdir().unwrap();
        let rar = work.path().join("release.rar");
        fs::write(&rar, b"Rar!").unwrap();
        let out = work.path().join("out");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            extract_rar_with(unrar.to_str().unwrap(), use_7z, &rar, &out, password),
        )
        .await
        .expect("extractor must not hang waiting for a password")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrar_password_is_fed_on_stdin_not_argv() {
        let tools = tempfile::tempdir().unwrap();
        let unrar = fake_extractor(
            tools.path(),
            "unrar",
            "UNRAR 7.20 freeware      Copyright (c) 1993-2025 Alexander Roshal",
            STDIN_PASSWORD_BODY,
        );
        let result = run_rar(&unrar, false, Some("s3cr3t pw")).await.unwrap();
        assert!(result.success, "{}", result.error_output);
        assert_eq!(result.files_extracted.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sevenz_password_is_fed_on_stdin_not_argv() {
        let tools = tempfile::tempdir().unwrap();
        let sevenz = fake_extractor(
            tools.path(),
            "7zz",
            "7-Zip (z) 25.01 (arm64) : Copyright (c) 1999-2025 Igor Pavlov : 2025-08-03",
            STDIN_PASSWORD_BODY,
        );
        let result = run_rar(&sevenz, true, Some("s3cr3t pw")).await.unwrap();
        assert!(result.success, "{}", result.error_output);
    }

    /// unrar 7 reports a missing or wrong password as `Incorrect password
    /// for ...` with exit code 11; that must stay a typed error.
    #[cfg(unix)]
    #[tokio::test]
    async fn wrong_password_on_stdin_is_typed_and_does_not_hang() {
        let tools = tempfile::tempdir().unwrap();
        // Asks twice: the second read must see EOF, not block.
        let body = "IFS= read -r pw\nIFS= read -r again\n\
                    echo 'Incorrect password for movie.mkv' >&2; exit 11";
        let unrar = fake_extractor(tools.path(), "unrar", "UNRAR 7.20 freeware", body);
        let error = run_rar(&unrar, false, Some("wrong")).await.unwrap_err();
        assert!(
            error.downcast_ref::<ArchivePasswordRequired>().is_some(),
            "{error:#}"
        );

        let sevenz = fake_extractor(
            tools.path(),
            "7zz",
            "7-Zip (z) 25.01 (arm64)",
            "printf 'Enter password:' ; IFS= read -r pw\n\
             echo 'ERROR: Data Error in encrypted file. Wrong password? : movie.mkv' >&2; exit 2",
        );
        let error = run_rar(&sevenz, true, Some("wrong")).await.unwrap_err();
        assert!(
            error.downcast_ref::<ArchivePasswordRequired>().is_some(),
            "{error:#}"
        );
    }

    /// The prompt we answer ourselves is not evidence of a password problem:
    /// any other failure stays an ordinary failed extraction.
    #[cfg(unix)]
    #[tokio::test]
    async fn answered_prompt_does_not_turn_other_failures_into_password_errors() {
        let tools = tempfile::tempdir().unwrap();
        let body = "printf 'Enter password (will not be echoed): ' >&2\nIFS= read -r pw\n\
                    echo 'movie.mkv - CRC failed' >&2; exit 3";
        let unrar = fake_extractor(tools.path(), "unrar", "UNRAR 7.20 freeware", body);
        let result = run_rar(&unrar, false, Some("s3cr3t pw")).await.unwrap();
        assert!(!result.success);
    }

    /// Extractors that are not known to read a piped password (old unrar,
    /// unrar-free, p7zip) still get it as an argument.
    #[cfg(unix)]
    #[tokio::test]
    async fn unknown_extractors_fall_back_to_the_password_argument() {
        let tools = tempfile::tempdir().unwrap();
        let body = "[ \"$secret_in_argv\" = yes ] || exit 7\nmkdir -p \"$out\"; printf x > \"$out/movie.mkv\"; echo 'All OK'";
        for (name, banner) in [
            (
                "unrar",
                "UNRAR 5.61 beta 1 freeware      Copyright (c) 1993-2018 Alexander Roshal",
            ),
            ("unrar-free", "unrar-free 0.3.1"),
            (
                "7z",
                "7-Zip [64] 16.02 : Copyright (c) 1999-2016 Igor Pavlov : 2016-05-21\np7zip Version 16.02",
            ),
        ] {
            let bin = fake_extractor(tools.path(), name, banner, body);
            let result = run_rar(&bin, name == "7z", Some("s3cr3t")).await.unwrap();
            assert!(result.success, "{name}: {}", result.error_output);
        }
    }

    #[test]
    fn stdin_password_support_is_read_from_the_banner() {
        for (banner, expected) in [
            (
                "UNRAR 7.20 beta 3 freeware      Copyright (c) 1993-2025 Alexander Roshal",
                true,
            ),
            (
                "UNRAR 6.21 freeware      Copyright (c) 1993-2023 Alexander Roshal",
                true,
            ),
            ("RAR 7.01   Copyright (c) 1993-2024 Alexander Roshal", true),
            ("UNRAR 5.61 beta 1 freeware", false),
            ("unrar-free 0.3.1", false),
            (
                "7-Zip (z) 25.01 (arm64) : Copyright (c) 1999-2025 Igor Pavlov : 2025-08-03",
                true,
            ),
            (
                "7-Zip 23.01 (x64) : Copyright (c) 1999-2023 Igor Pavlov : 2023-06-20",
                true,
            ),
            (
                "7-Zip [64] 16.02 : Copyright (c) 1999-2016 Igor Pavlov\np7zip Version 16.02",
                false,
            ),
            ("", false),
        ] {
            assert_eq!(
                banner_reads_password_from_stdin(banner),
                expected,
                "{banner}"
            );
        }
    }

    #[test]
    fn password_args_never_carry_the_password_in_stdin_mode() {
        let rar = Path::new("/tmp/test.rar");
        let out = Path::new("/tmp/out");
        let stdin = PasswordArg::Stdin;
        assert!(
            rar_extract_args_with_unrar(rar, out, &stdin)
                .iter()
                .any(|a| a == "-p")
        );
        assert!(
            !rar_extract_args_with_7z(rar, out, &stdin)
                .iter()
                .any(|a| a.starts_with("-p"))
        );
        assert!(
            !sevenz_extract_args(rar, out, &stdin)
                .iter()
                .any(|a| a.starts_with("-p"))
        );
    }

    /// End to end against a real 7z, when one is installed: an encrypted
    /// archive (with encrypted headers) extracts with the right password and
    /// reports a typed error for a wrong or missing one, without hanging.
    #[tokio::test]
    async fn real_7z_encrypted_archive_round_trip() {
        let Some(sevenz) = find_7z() else {
            eprintln!("skipping: no 7z/7zz/7za on PATH");
            return;
        };
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("movie.mkv"), b"payload").unwrap();
        let archive = work.path().join("release.7z");
        let created = std::process::Command::new(&sevenz)
            .current_dir(work.path())
            .args(["a", "-pp4ss word", "-mhe=on", "release.7z", "movie.mkv"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");

        let run = |password: Option<&'static str>, out: &'static str| {
            let sevenz = sevenz.clone();
            let archive = archive.clone();
            let out = work.path().join(out);
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    extract_7z_with(&sevenz, &archive, &out, password),
                )
                .await
                .expect("7z must not hang waiting for a password")
            }
        };

        let ok = run(Some("p4ss word"), "ok").await.unwrap();
        assert!(ok.success, "{}", ok.error_output);
        assert_eq!(
            fs::read(work.path().join("ok/movie.mkv")).unwrap(),
            b"payload"
        );

        for (password, out) in [(Some("wrong"), "wrong"), (None, "none")] {
            let error = run(password, out).await.unwrap_err();
            assert!(
                error.downcast_ref::<ArchivePasswordRequired>().is_some(),
                "{password:?}: {error:#}"
            );
        }
    }
}
