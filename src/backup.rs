//! Logical backup and restore for managed databases.
//!
//! v1 supports local backups only (`pg_dump`/`mysqldump`, gzip-compressed).
//! Remote targets (SSH/S3) are on the roadmap; the config already models them.

use crate::config::{Config, Paths};
use crate::databases::{MYSQL_CONTAINER, PG_CONTAINER};
use crate::{docker, proc, state};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    Mysql,
}

impl Engine {
    fn dir(&self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Mysql => "mysql",
        }
    }
    pub fn parse(s: &str) -> Result<Engine> {
        match s {
            "postgres" | "pg" => Ok(Engine::Postgres),
            "mysql" => Ok(Engine::Mysql),
            other => anyhow::bail!("unknown engine `{other}` (use postgres|mysql)"),
        }
    }
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Create a backup of one database. Returns the path written.
pub fn backup(cfg: &Config, paths: &Paths, engine: Engine, database: &str) -> Result<PathBuf> {
    let dir = paths.backups.join(engine.dir());
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{database}-{}.sql.gz", timestamp()));
    let file_str = file.to_string_lossy().into_owned();

    let script = match engine {
        Engine::Postgres => {
            let user = pg_user(cfg)?;
            format!("docker exec {PG_CONTAINER} pg_dump -U {user} {database} | gzip > '{file_str}'")
        }
        Engine::Mysql => {
            let pw = mysql_password(cfg)?;
            format!(
                "docker exec {MYSQL_CONTAINER} mysqldump -u root -p{pw} {database} | gzip > '{file_str}'"
            )
        }
    };
    proc::shell(&script).with_context(|| format!("backing up {database}"))?;
    state::record_event(paths, database, &format!("backup created: {file_str}"))?;
    apply_retention(&dir, cfg)?;
    Ok(file)
}

/// Back up every database and volume referenced by the config. This is the
/// "escape hatch" that keeps the disposable-server promise honest: after it
/// runs, all app state lives in the backup target, not only on the box.
pub fn backup_all(cfg: &Config, paths: &Paths) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for (app_name, app) in &cfg.apps {
        if let Some(pg) = &app.databases.postgres {
            out.push(backup(cfg, paths, Engine::Postgres, &pg.database)?);
        }
        if let Some(my) = &app.databases.mysql {
            out.push(backup(cfg, paths, Engine::Mysql, &my.database)?);
        }
        for vol_name in app.volumes.keys() {
            // Skip volumes whose data dir does not exist yet (app never started).
            if paths.volume_dir(app_name, vol_name).is_dir() {
                out.push(backup_volume(cfg, paths, app_name, vol_name)?);
            }
        }
    }
    Ok(out)
}

/// Directory holding the backups of one app volume. Each volume gets its own
/// directory so retention is applied per volume, not mixed across an app.
fn volume_backup_dir(paths: &Paths, app: &str, name: &str) -> PathBuf {
    paths.backups.join("volumes").join(app).join(name)
}

/// Archive one named volume of an app into a gzip-compressed tarball. Returns
/// the path written.
pub fn backup_volume(cfg: &Config, paths: &Paths, app: &str, name: &str) -> Result<PathBuf> {
    let src = paths.volume_dir(app, name);
    anyhow::ensure!(
        src.is_dir(),
        "volume `{name}` of app `{app}` has no data directory at {}",
        src.display()
    );
    let dir = volume_backup_dir(paths, app, name);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.tar.gz", timestamp()));
    let file_str = file.to_string_lossy().into_owned();
    let src_str = src.to_string_lossy().into_owned();
    // tar the directory contents (`.`) so the archive restores cleanly into any
    // target directory regardless of its name.
    let script = format!("tar -C '{src_str}' -czf '{file_str}' .");
    proc::shell(&script).with_context(|| format!("backing up volume {app}/{name}"))?;
    state::record_event(paths, app, &format!("volume `{name}` backup: {file_str}"))?;
    apply_retention(&dir, cfg)?;
    Ok(file)
}

/// Back up every volume of one app. Returns the paths written.
pub fn backup_app_volumes(cfg: &Config, paths: &Paths, app: &str) -> Result<Vec<PathBuf>> {
    let volumes = cfg
        .apps
        .get(app)
        .ok_or_else(|| anyhow::anyhow!("unknown app `{app}`"))?
        .volumes
        .clone();
    let mut out = Vec::new();
    for name in volumes.keys() {
        out.push(backup_volume(cfg, paths, app, name)?);
    }
    Ok(out)
}

/// Restore one app volume from a tarball. Destructive: it clears the live data
/// directory and extracts the archive over it. Always takes a safety backup of
/// the current contents first, and stops the app container so nothing writes
/// mid-restore. The next reconcile (timer or `homeops reconcile`) brings the
/// app back up against the restored data.
pub fn restore_volume(
    cfg: &Config,
    paths: &Paths,
    app: &str,
    name: &str,
    file: &Path,
) -> Result<()> {
    anyhow::ensure!(file.exists(), "backup file not found: {}", file.display());
    let size = std::fs::metadata(file)?.len();
    anyhow::ensure!(size > 0, "backup file is empty: {}", file.display());

    let src = paths.volume_dir(app, name);
    if src.is_dir() {
        let safety =
            backup_volume(cfg, paths, app, name).context("taking safety backup before restore")?;
        state::record_event(
            paths,
            app,
            &format!("volume safety backup: {}", safety.display()),
        )?;
    } else {
        std::fs::create_dir_all(&src)?;
    }

    // Stop the app so nothing writes to the directory during extraction.
    let container = docker::container_name(app);
    docker::remove(&container)?;

    let src_str = src.to_string_lossy().into_owned();
    let file_str = file.to_string_lossy().into_owned();
    // Clear existing contents (without removing the mount target itself), then
    // extract. `find -mindepth 1 -delete` is a no-op on an empty directory.
    let script =
        format!("find '{src_str}' -mindepth 1 -delete && tar -C '{src_str}' -xzf '{file_str}'");
    proc::shell(&script).with_context(|| format!("restoring volume {app}/{name}"))?;
    state::record_event(
        paths,
        app,
        &format!("volume `{name}` restored from {file_str}"),
    )?;
    Ok(())
}

/// Restore a database from a gzip-compressed dump. Always takes a safety backup
/// of the current contents first.
pub fn restore(
    cfg: &Config,
    paths: &Paths,
    engine: Engine,
    database: &str,
    file: &Path,
) -> Result<()> {
    anyhow::ensure!(file.exists(), "backup file not found: {}", file.display());
    let size = std::fs::metadata(file)?.len();
    anyhow::ensure!(size > 0, "backup file is empty: {}", file.display());

    // Safety backup before clobbering.
    let safety =
        backup(cfg, paths, engine, database).context("taking safety backup before restore")?;
    state::record_event(
        paths,
        database,
        &format!("safety backup: {}", safety.display()),
    )?;

    let file_str = file.to_string_lossy().into_owned();
    let script = match engine {
        Engine::Postgres => {
            let user = pg_user(cfg)?;
            format!(
                "gunzip -c '{file_str}' | docker exec -i {PG_CONTAINER} psql -U {user} {database}"
            )
        }
        Engine::Mysql => {
            let pw = mysql_password(cfg)?;
            format!(
                "gunzip -c '{file_str}' | docker exec -i {MYSQL_CONTAINER} mysql -u root -p{pw} {database}"
            )
        }
    };
    proc::shell(&script).with_context(|| format!("restoring {database}"))?;
    state::record_event(paths, database, &format!("restored from {file_str}"))?;
    Ok(())
}

/// Find the most recent backup file across all engines.
pub fn latest(paths: &Paths) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for engine_dir in ["postgres", "mysql"] {
        let dir = paths.backups.join(engine_dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if let Ok(meta) = e.metadata() {
                if let Ok(modified) = meta.modified() {
                    if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                        newest = Some((modified, e.path()));
                    }
                }
            }
        }
    }
    newest.map(|(_, p)| p)
}

/// Keep only the newest `retention` files in a directory.
fn apply_retention(dir: &Path, cfg: &Config) -> Result<()> {
    let Some(keep) = cfg.backups.retention else {
        return Ok(());
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, e.path()))
        })
        .collect();
    files.sort_by_key(|(t, _)| std::cmp::Reverse(*t)); // newest first
    for (_, path) in files.into_iter().skip(keep as usize) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn pg_user(cfg: &Config) -> Result<String> {
    cfg.databases
        .postgres
        .as_ref()
        .map(|p| p.admin_user.clone())
        .ok_or_else(|| anyhow::anyhow!("postgres is not configured"))
}

fn mysql_password(cfg: &Config) -> Result<String> {
    cfg.databases
        .mysql
        .as_ref()
        .map(|m| m.admin_password.clone())
        .ok_or_else(|| anyhow::anyhow!("mysql is not configured"))
}

// Surface for the admin UI / status without importing docker elsewhere.
pub fn engine_running(engine: Engine) -> bool {
    match engine {
        Engine::Postgres => docker::is_running(PG_CONTAINER),
        Engine::Mysql => docker::is_running(MYSQL_CONTAINER),
    }
}
