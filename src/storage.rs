//! On-disk management of app volume directories.
//!
//! Volumes live as plain host directories under `<workdir>/data/<app>/<name>`.
//! HomeOps never deletes them during reconcile, build or rollback — removing
//! data is always an explicit operator action. This module finds and (only when
//! asked) prunes the directories an app no longer declares.

use crate::config::{AppConfig, Paths};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Directories under an app's data dir that are not declared as volumes anymore
/// (e.g. a renamed or removed volume). Returned sorted for stable output.
pub fn orphan_dirs(paths: &Paths, app: &str, cfg: &AppConfig) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths.app_data_dir(app)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            match entry.file_name().to_str() {
                Some(name) if !cfg.volumes.contains_key(name) => out.push(path),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Find orphaned volume directories for an app and, when `apply` is true, delete
/// them. Returns the directories found (the ones deleted when `apply`). This is
/// the only code path that removes volume data, and it is reachable only from
/// the explicit `volume-prune` command.
pub fn prune(paths: &Paths, app: &str, cfg: &AppConfig, apply: bool) -> Result<Vec<PathBuf>> {
    let orphans = orphan_dirs(paths, app, cfg);
    if apply {
        for dir in &orphans {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("removing orphaned volume dir {}", dir.display()))?;
        }
    }
    Ok(orphans)
}
