//! On-disk management of app volume directories.
//!
//! Volumes live as plain host directories under `<workdir>/data/<app>/<name>`.
//! HomeOps never deletes them during reconcile, build or rollback — removing
//! data is always an explicit operator action. This module finds and (only when
//! asked, and after a safety backup) prunes the directories an app no longer
//! declares, and owns the permission policy for a freshly provisioned dir.

use crate::config::{AppConfig, Paths, VolumeSpec};
use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Apply the permissions a freshly provisioned volume directory should have.
/// This is the single source of truth shared by reconcile (on first create) and
/// restore (after clearing+extracting), so a restored volume ends up exactly as
/// writable as a reconciled one:
///
/// * with `uid` set, the dir is chowned to that UID and given `0755`, so a
///   non-root container can write its own data without world access;
/// * without `uid`, the dir is made world-writable (`0777`) — the zero-config
///   default that works for a container running as any UID.
pub fn apply_dir_perms(dir: &Path, spec: Option<&VolumeSpec>) -> Result<()> {
    match spec.and_then(|s| s.uid()) {
        Some(uid) => {
            std::os::unix::fs::chown(dir, Some(uid), Some(uid))
                .with_context(|| format!("chowning {} to uid {uid}", dir.display()))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("setting permissions on {}", dir.display()))?;
        }
        None => {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777))
                .with_context(|| format!("setting permissions on {}", dir.display()))?;
        }
    }
    Ok(())
}

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
/// them — but only after archiving each into the un-pruned `safety/` tree, so a
/// directory removed because of a config typo or a temporarily commented-out
/// volume can still be recovered. Returns the directories found (the ones
/// deleted when `apply`). This is the only code path that removes volume data,
/// and it is reachable only from the explicit `volume-prune` command.
pub fn prune(paths: &Paths, app: &str, cfg: &AppConfig, apply: bool) -> Result<Vec<PathBuf>> {
    let orphans = orphan_dirs(paths, app, cfg);
    if apply {
        for dir in &orphans {
            let label = dir.file_name().and_then(|n| n.to_str()).unwrap_or("orphan");
            crate::backup::safety_backup(paths, app, label, dir)
                .with_context(|| format!("safety backup before pruning {}", dir.display()))?;
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("removing orphaned volume dir {}", dir.display()))?;
        }
    }
    Ok(orphans)
}
