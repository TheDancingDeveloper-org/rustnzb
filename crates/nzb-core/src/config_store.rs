//! Crash-safe persistence of the file layer of `config.toml`.
//!
//! A save merges the new configuration into the document already on disk
//! with `toml_edit`, so the user's comments, key order and formatting survive
//! and only changed values are rewritten. The result is written to
//! `config.toml.tmp` (mode 0600), fsynced, renamed over `config.toml`, and the
//! directory is fsynced. Before the rename the previous file is copied into
//! `config-backups/`, and that directory is pruned to the newest
//! [`MAX_BACKUPS`] plus one per day for [`BACKUP_DAYS`] days.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use chrono::{NaiveDate, Utc};
use toml_edit::{DocumentMut, Item, Table};

use crate::config::AppConfig;

/// Schema version written as `config_version` in every saved file.
pub const CONFIG_VERSION: u32 = 1;

/// Newest backups kept regardless of age.
pub const MAX_BACKUPS: usize = 20;

/// Days for which the newest backup of each day is also kept.
pub const BACKUP_DAYS: i64 = 7;

const BACKUP_DIR: &str = "config-backups";
const BACKUP_PREFIX: &str = "config-";
const BACKUP_TIME_FORMAT: &str = "%Y%m%dT%H%M%S%3fZ";

/// Bring a configuration loaded from an older schema up to
/// [`CONFIG_VERSION`]. Version 0 files (no `config_version` key) only need
/// the stamp; there are no renamed keys yet.
pub fn migrate(config: &mut AppConfig) {
    if config.config_version < CONFIG_VERSION {
        config.config_version = CONFIG_VERSION;
    }
}

/// Resolve the config path given on the command line or in `RUSTNZB_CONFIG`.
///
/// Call this once at startup, before any path is derived from it. The path is
/// operator input only: no API request can change it, so this is the single
/// point where it is checked. It must not contain `..`. Its directory is
/// created and canonicalised, so the temp file and `config-backups/` are always
/// built under a resolved base directory.
pub fn resolve_operator_config_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.components().any(|c| c == Component::ParentDir) {
        anyhow::bail!("config path must not contain '..': {}", path.display());
    }
    let file_name = path
        .file_name()
        .with_context(|| format!("config path has no file name: {}", path.display()))?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create config directory: {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve config directory: {}", dir.display()))?;
    Ok(dir.join(file_name))
}

/// Write `config` to `path` as the file layer.
///
/// Values equal to the file's current contents are left alone, and the
/// previous file is kept as a backup before it is replaced.
pub fn write_file_layer(config: &AppConfig, path: &Path) -> anyhow::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create config directory: {}", dir.display()))?;

    let existing = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read config file: {}", path.display()));
        }
    };

    let mut stamped = config.clone();
    stamped.config_version = CONFIG_VERSION;
    let existing_text = existing
        .as_deref()
        .map(|bytes| {
            std::str::from_utf8(bytes)
                .with_context(|| format!("config file is not UTF-8: {}", path.display()))
        })
        .transpose()?;
    let contents = merged_document(existing_text, &stamped)?;

    if let Some(previous) = existing.as_deref() {
        if previous == contents.as_bytes() {
            // Nothing to save. Replacing the file would cost a durable write
            // and a directory fsync for byte-identical content; on a contended
            // disk that is measurable start-up time. Permissions are still
            // tightened if an older save (or the user) left them open.
            if !needs_permission_tightening(path) {
                return Ok(());
            }
        } else {
            backup(dir, previous)?;
        }
    }
    write_atomic(path, contents.as_bytes())
}

/// Whether a save still has to replace `path` for its mode alone.
fn needs_permission_tightening(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777 != 0o600)
            .unwrap_or(true)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Render `config` as TOML, starting from the document already on disk so
/// that comments and formatting are kept.
fn merged_document(existing: Option<&str>, config: &AppConfig) -> anyhow::Result<String> {
    let fresh = toml::to_string_pretty(config)?;
    let Some(existing) = existing else {
        return Ok(fresh);
    };
    let mut doc: DocumentMut = existing
        .parse()
        .context("existing config file is not valid TOML; refusing to overwrite it")?;
    let new: DocumentMut = fresh
        .parse()
        .context("serialised config is not valid TOML")?;
    merge_table(doc.as_table_mut(), new.as_table());
    Ok(doc.to_string())
}

fn merge_table(existing: &mut Table, new: &Table) {
    let stale: Vec<String> = existing
        .iter()
        .map(|(key, _)| key.to_string())
        .filter(|key| !new.contains_key(key))
        .collect();
    for key in stale {
        existing.remove(&key);
    }
    for (key, new_item) in new.iter() {
        match existing.get_mut(key) {
            Some(item) => merge_item(item, new_item),
            None => {
                existing.insert(key, new_item.clone());
            }
        }
    }
}

fn merge_item(existing: &mut Item, new: &Item) {
    match (existing, new) {
        (Item::Table(e), Item::Table(n)) => merge_table(e, n),
        (Item::ArrayOfTables(e), Item::ArrayOfTables(n)) if e.len() == n.len() => {
            for (et, nt) in e.iter_mut().zip(n.iter()) {
                merge_table(et, nt);
            }
        }
        (Item::Value(e), Item::Value(n)) => {
            // Compare without the surrounding whitespace and comments, so an
            // unchanged value keeps the user's formatting around it.
            if e.clone().decorated("", "").to_string() != n.clone().decorated("", "").to_string() {
                let decor = e.decor().clone();
                *e = n.clone();
                *e.decor_mut() = decor;
            }
        }
        (existing, new) => *existing = new.clone(),
    }
}

/// Write `contents` to a temporary file beside `path`, fsync it, rename it
/// over `path`, and fsync the directory. A crash leaves either the old file or
/// the new one, plus at most a stale `*.tmp` that loading ignores.
fn write_atomic(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let tmp = tmp_path(path);
    {
        let mut file = open_private(&tmp, false)
            .with_context(|| format!("Failed to create {}", tmp.display()))?;
        file.write_all(contents)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to write config file: {}", path.display()))?;
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        sync_dir(dir)?;
    }
    Ok(())
}

/// A temporary name no other write in this process or another one uses, so
/// concurrent saves never share, or rename away, each other's temp file.
fn tmp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{unique}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Create a file readable and writable by its owner only. With `create_new`
/// the call fails if the file exists; otherwise an existing file is truncated.
fn open_private(path: &Path, create_new: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        // `mode` only applies when the file is created; a leftover temp file
        // keeps its old permissions, so set them explicitly.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        options.open(path)
    }
}

fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("Failed to sync directory {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Copy the previous file into `config-backups/` and prune old backups.
fn backup(config_dir: &Path, previous: &[u8]) -> anyhow::Result<()> {
    let dir = config_dir.join(BACKUP_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let stamp = Utc::now().format(BACKUP_TIME_FORMAT).to_string();

    // Two saves in the same millisecond must not share a name. Each candidate
    // is created exclusively, so no other save can take the name in between.
    let mut n = 0;
    let (target, mut file) = loop {
        let name = if n == 0 {
            format!("{BACKUP_PREFIX}{stamp}.toml")
        } else {
            format!("{BACKUP_PREFIX}{stamp}-{n}.toml")
        };
        let candidate = dir.join(name);
        match open_private(&candidate, true) {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to create backup {}", candidate.display()));
            }
        }
    };
    file.write_all(previous)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("Failed to write backup {}", target.display()))?;
    prune_backups(&dir, Utc::now().date_naive())
}

/// Keep the newest [`MAX_BACKUPS`] backups, and the newest backup of each of
/// the last [`BACKUP_DAYS`] days. Everything else is deleted.
fn prune_backups(dir: &Path, today: NaiveDate) -> anyhow::Result<()> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .with_context(|| format!("Failed to list {}", dir.display()))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with(BACKUP_PREFIX) && name.ends_with(".toml"))
        .collect();
    // The timestamp is fixed-width and UTC, so name order is time order.
    names.sort_unstable_by(|a, b| b.cmp(a));

    let oldest_kept_day = today - chrono::Duration::days(BACKUP_DAYS - 1);
    let mut kept_days = std::collections::HashSet::new();
    for (index, name) in names.iter().enumerate() {
        let day = backup_day(name);
        let newest_of_recent_day = day.is_some_and(|d| d >= oldest_kept_day && kept_days.insert(d));
        if index < MAX_BACKUPS || newest_of_recent_day {
            continue;
        }
        if let Err(e) = fs::remove_file(dir.join(name)) {
            tracing::warn!(file = %name, error = %e, "could not prune config backup");
        }
    }
    Ok(())
}

/// The UTC day encoded in a backup file name.
fn backup_day(name: &str) -> Option<NaiveDate> {
    let digits = name.strip_prefix(BACKUP_PREFIX)?.get(..8)?;
    NaiveDate::parse_from_str(digits, "%Y%m%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backups(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.join(BACKUP_DIR))
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn save_stamps_version_and_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# my hand-written file\n[general]\n# keep this comment\nport = 9090\n",
        )
        .unwrap();

        let mut config = AppConfig::load(&path).unwrap();
        config.general.speed_limit_bps = 1234;
        config.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my hand-written file\n"), "{text}");
        assert!(text.contains("# keep this comment"), "{text}");
        let reloaded: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.config_version, CONFIG_VERSION);
        assert_eq!(reloaded.general.speed_limit_bps, 1234);
        assert_eq!(reloaded.general.port, 9090);
    }

    #[test]
    fn unchanged_values_keep_their_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "config_version = 1\n[general]\nport = 9090   # the port\nlog_level = \"debug\"\n",
        )
        .unwrap();

        let config = AppConfig::load(&path).unwrap();
        config.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("port = 9090   # the port"), "{text}");
    }

    #[test]
    fn save_refuses_to_overwrite_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is = = not toml [").unwrap();

        let err = AppConfig::default().save(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to overwrite"),
            "{err:#}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "this is = = not toml [");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_and_backups_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[general]\nport = 9090\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let mut config = AppConfig::load(&path).unwrap();
        config.general.port = 9191;
        config.save(&path).unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        let backup = dir.path().join(BACKUP_DIR).join(&backups(dir.path())[0]);
        assert_eq!(mode(&backup), 0o600);
    }

    #[test]
    fn crash_leftover_tmp_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = AppConfig::default();
        config.general.port = 7000;
        config.save(&path).unwrap();

        // A crash after a temp file was written but before the rename.
        let stale = dir.path().join("config.toml.4242-0.tmp");
        fs::write(&stale, "[general]\nport = 1\nbroken = [").unwrap();

        let loaded = AppConfig::load(&path).unwrap();
        assert_eq!(loaded.general.port, 7000);

        // The next save is unaffected and leaves no temp file of its own.
        let mut next = loaded;
        next.general.port = 7001;
        next.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path).unwrap().general.port, 7001);
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".tmp") && n != "config.toml.4242-0.tmp")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn operator_config_path_is_resolved_and_confined() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("cfg");

        let resolved = resolve_operator_config_path(&nested.join("config.toml")).unwrap();
        assert!(nested.is_dir(), "the config directory is created");
        assert_eq!(resolved, nested.canonicalize().unwrap().join("config.toml"));

        let escape = nested.join("..").join("config.toml");
        assert!(resolve_operator_config_path(&escape).is_err());
        assert!(resolve_operator_config_path(Path::new("../config.toml")).is_err());
        assert!(resolve_operator_config_path(Path::new("/")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_save_does_not_replace_the_file() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = AppConfig::default();
        config.save(&path).unwrap();
        let before = fs::metadata(&path).unwrap().ino();

        config.save(&path).unwrap();
        let after = fs::metadata(&path).unwrap().ino();
        assert_eq!(before, after, "an unchanged save must not rewrite the file");

        let leftovers: Vec<_> = fs::read_dir(&path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_save_still_tightens_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = AppConfig::default();
        config.save(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        config.save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a save must not leave the config readable");
    }

    #[test]
    fn unchanged_save_writes_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = AppConfig::default();
        config.save(&path).unwrap();
        config.save(&path).unwrap();
        assert!(backups(dir.path()).is_empty());
    }

    #[test]
    fn every_change_keeps_the_previous_file_as_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = AppConfig::default();
        config.save(&path).unwrap();
        config.general.port = 8001;
        config.save(&path).unwrap();

        let names = backups(dir.path());
        assert_eq!(names.len(), 1);
        let kept = fs::read_to_string(dir.path().join(BACKUP_DIR).join(&names[0])).unwrap();
        assert!(kept.contains("port = 9090"), "{kept}");
    }

    #[test]
    fn rotation_keeps_newest_and_one_per_recent_day() {
        let dir = tempfile::tempdir().unwrap();
        let backup_dir = dir.path().join(BACKUP_DIR);
        fs::create_dir_all(&backup_dir).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();

        // 30 backups an hour apart across the last day, and 3 more on each of
        // the 10 days before that.
        let mut names = Vec::new();
        for h in 0..30u32 {
            let t = format!("20261016T{:02}{:02}00000Z", h / 60 % 24, h % 60);
            names.push(format!("{BACKUP_PREFIX}{t}.toml"));
        }
        for day in 1..=10u32 {
            for k in 0..3u32 {
                names.push(format!(
                    "{BACKUP_PREFIX}202610{:02}T10000{k}000Z.toml",
                    16 - day
                ));
            }
        }
        for name in &names {
            fs::write(backup_dir.join(name), "x").unwrap();
        }

        prune_backups(&backup_dir, today).unwrap();

        let mut left: Vec<String> = fs::read_dir(&backup_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();

        // The 20 newest are today's backups. The window holds Oct 10..=16, so
        // the newest backup of each older day in it (Oct 10..=15) also stays.
        let mut newest_first = names.clone();
        newest_first.sort_by(|a, b| b.cmp(a));
        for kept in &newest_first[..MAX_BACKUPS] {
            assert!(left.contains(kept), "newest backup {kept} was pruned");
        }
        assert_eq!(left.len(), MAX_BACKUPS + 6, "{left:?}");
        assert!(left.contains(&format!("{BACKUP_PREFIX}20261010T100002000Z.toml")));
        // Oct 9 and older fall outside the window and are gone.
        assert!(!left.iter().any(|n| n.starts_with("config-20261009")));
    }

    #[test]
    fn migrate_stamps_version_zero_files() {
        let mut config = AppConfig {
            config_version: 0,
            ..AppConfig::default()
        };
        migrate(&mut config);
        assert_eq!(config.config_version, CONFIG_VERSION);
    }
}
