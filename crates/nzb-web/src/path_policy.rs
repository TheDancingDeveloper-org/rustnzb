use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::nzb_core::config::AppConfig;

pub const ALLOWED_ROOTS_ENV: &str = "RUSTNZB_ALLOWED_ROOTS";

#[derive(Debug, thiserror::Error)]
#[error("invalid configurable path `{field}`: {message}")]
pub struct PathPolicyError {
    pub field: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PathWarning {
    pub field: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigPathPolicy {
    roots: Option<Result<Vec<PathBuf>, String>>,
}

impl ConfigPathPolicy {
    pub fn from_env() -> Self {
        Self::from_value(std::env::var_os(ALLOWED_ROOTS_ENV))
    }

    pub fn from_value(value: Option<impl AsRef<OsStr>>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        if value.as_ref().is_empty() {
            return Self::default();
        }
        let paths = std::env::split_paths(value.as_ref()).collect::<Vec<_>>();
        if paths.is_empty() {
            return Self::default();
        }
        let roots = paths
            .into_iter()
            .map(|root| {
                if !root.is_absolute() {
                    return Err(format!(
                        "{ALLOWED_ROOTS_ENV} entries must be absolute: {}",
                        root.display()
                    ));
                }
                resolve_path(&root).map_err(|message| {
                    format!(
                        "invalid {ALLOWED_ROOTS_ENV} root {}: {message}",
                        root.display()
                    )
                })
            })
            .collect();
        Self { roots: Some(roots) }
    }

    pub fn is_enabled(&self) -> bool {
        self.roots.is_some()
    }

    pub fn validate_update(
        &self,
        previous: &AppConfig,
        next: &AppConfig,
    ) -> Result<(), PathPolicyError> {
        let Some(roots) = self.valid_roots("configuration")? else {
            return Ok(());
        };
        let previous = configurable_paths(previous);
        for (field, path) in configurable_paths(next) {
            if previous
                .iter()
                .any(|(old_field, old_path)| old_field == &field && old_path == &path)
            {
                continue;
            }
            validate_one(&field, &path, roots)?;
        }
        Ok(())
    }

    pub fn validate_browse_path(&self, path: &Path) -> Result<PathBuf, PathPolicyError> {
        let Some(roots) = self.valid_roots("path")? else {
            return resolve_path(path).map_err(|message| PathPolicyError {
                field: "path".to_string(),
                message,
            });
        };
        validate_one("path", path, roots)
    }

    pub fn warnings(&self, config: &AppConfig) -> Vec<PathWarning> {
        let Some(roots) = self.roots.as_ref() else {
            return Vec::new();
        };
        let roots = match roots {
            Ok(roots) => roots,
            Err(message) => {
                return vec![PathWarning {
                    field: ALLOWED_ROOTS_ENV.to_string(),
                    path: String::new(),
                    message: message.clone(),
                }];
            }
        };
        configurable_paths(config)
            .into_iter()
            .filter_map(|(field, path)| {
                validate_one(&field, &path, roots)
                    .err()
                    .map(|error| PathWarning {
                        field,
                        path: path.to_string_lossy().into_owned(),
                        message: error.message,
                    })
            })
            .collect()
    }

    fn valid_roots(&self, field: &str) -> Result<Option<&[PathBuf]>, PathPolicyError> {
        match self.roots.as_ref() {
            None => Ok(None),
            Some(Ok(roots)) => Ok(Some(roots)),
            Some(Err(message)) => Err(PathPolicyError {
                field: field.to_string(),
                message: message.clone(),
            }),
        }
    }
}

fn configurable_paths(config: &AppConfig) -> Vec<(String, PathBuf)> {
    let general = &config.general;
    let mut paths = vec![
        (
            "general.incomplete_dir".to_string(),
            general.incomplete_dir.clone(),
        ),
        (
            "general.complete_dir".to_string(),
            general.complete_dir.clone(),
        ),
        ("general.data_dir".to_string(), general.data_dir.clone()),
    ];
    for (field, path) in [
        ("general.log_file", general.log_file.as_ref()),
        ("general.watch_dir", general.watch_dir.as_ref()),
        (
            "general.local_import_dir",
            general.local_import_dir.as_ref(),
        ),
        ("general.scripts_dir", general.scripts_dir.as_ref()),
    ] {
        if let Some(path) = path {
            paths.push((field.to_string(), path.clone()));
        }
    }
    for category in &config.categories {
        if let Some(output_dir) = &category.output_dir {
            let path = if output_dir.is_absolute() {
                output_dir.clone()
            } else {
                general.complete_dir.join(output_dir)
            };
            paths.push((format!("categories.{}.output_dir", category.name), path));
        }
    }
    for (field, script) in [
        ("general.script_success", general.script_success.as_deref()),
        ("general.script_failure", general.script_failure.as_deref()),
    ] {
        if let Some(script) = script {
            let script = PathBuf::from(script);
            let path = if script.is_absolute() {
                script
            } else if let Some(scripts_dir) = &general.scripts_dir {
                scripts_dir.join(script)
            } else {
                continue;
            };
            paths.push((field.to_string(), path));
        }
    }
    paths
}

fn validate_one(field: &str, path: &Path, roots: &[PathBuf]) -> Result<PathBuf, PathPolicyError> {
    let resolved = resolve_path(path).map_err(|message| PathPolicyError {
        field: field.to_string(),
        message,
    })?;
    if roots.iter().any(|root| resolved.starts_with(root)) {
        return Ok(resolved);
    }
    Err(PathPolicyError {
        field: field.to_string(),
        message: format!("{} resolves outside {ALLOWED_ROOTS_ENV}", path.display()),
    })
}

fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("must be absolute".to_string());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("must not contain `..`".to_string());
    }
    if path
        .as_os_str()
        .to_string_lossy()
        .chars()
        .any(char::is_control)
    {
        return Err("must not contain control characters".to_string());
    }

    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| format!("cannot resolve {}", path.display()))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| format!("cannot resolve {}", path.display()))?;
    }
    let mut resolved = std::fs::canonicalize(existing)
        .map_err(|error| format!("cannot canonicalise {}: {error}", path.display()))?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_dirs(root: &Path) -> AppConfig {
        let mut config = AppConfig::default();
        config.general.incomplete_dir = root.join("incomplete");
        config.general.complete_dir = root.join("complete");
        config.general.data_dir = root.join("data");
        config
    }

    #[test]
    fn accepts_paths_inside_an_allowed_root() {
        let temp = tempfile::tempdir().unwrap();
        let policy = ConfigPathPolicy::from_value(Some(temp.path().as_os_str()));
        let previous = config_with_dirs(temp.path());
        let mut next = previous.clone();
        next.general.watch_dir = Some(temp.path().join("watch"));

        policy.validate_update(&previous, &next).unwrap();
    }

    #[test]
    fn empty_allowed_roots_value_disables_the_policy() {
        assert!(!ConfigPathPolicy::from_value(Some(OsStr::new(""))).is_enabled());
    }

    #[test]
    fn rejects_paths_outside_an_allowed_root() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let policy = ConfigPathPolicy::from_value(Some(allowed.path().as_os_str()));
        let previous = config_with_dirs(allowed.path());
        let mut next = previous.clone();
        next.general.watch_dir = Some(outside.path().join("watch"));

        let error = policy.validate_update(&previous, &next).unwrap_err();
        assert_eq!(error.field, "general.watch_dir");
        assert!(error.message.contains("outside RUSTNZB_ALLOWED_ROOTS"));
    }

    #[test]
    fn rejects_parent_traversal() {
        let allowed = tempfile::tempdir().unwrap();
        let policy = ConfigPathPolicy::from_value(Some(allowed.path().as_os_str()));
        let previous = config_with_dirs(allowed.path());
        let mut next = previous.clone();
        next.general.watch_dir = Some(allowed.path().join("watch/../escape"));

        let error = policy.validate_update(&previous, &next).unwrap_err();
        assert!(error.message.contains("must not contain `..`"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), allowed.path().join("link")).unwrap();
        let policy = ConfigPathPolicy::from_value(Some(allowed.path().as_os_str()));
        let previous = config_with_dirs(allowed.path());
        let mut next = previous.clone();
        next.general.watch_dir = Some(allowed.path().join("link/watch"));

        let error = policy.validate_update(&previous, &next).unwrap_err();
        assert!(error.message.contains("outside RUSTNZB_ALLOWED_ROOTS"));
    }

    #[test]
    fn unchanged_legacy_path_is_warned_but_does_not_block_other_edits() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let policy = ConfigPathPolicy::from_value(Some(allowed.path().as_os_str()));
        let mut previous = config_with_dirs(allowed.path());
        previous.general.watch_dir = Some(outside.path().join("legacy"));
        let mut next = previous.clone();
        next.general.speed_limit_bps = 1024;

        policy.validate_update(&previous, &next).unwrap();
        let warnings = policy.warnings(&next);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].field, "general.watch_dir");
    }
}
