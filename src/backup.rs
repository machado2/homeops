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

/// Back up every database referenced by the config.
pub fn backup_all(cfg: &Config, paths: &Paths) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for app in cfg.apps.values() {
        if let Some(pg) = &app.databases.postgres {
            out.push(backup(cfg, paths, Engine::Postgres, &pg.database)?);
        }
        if let Some(my) = &app.databases.mysql {
            out.push(backup(cfg, paths, Engine::Mysql, &my.database)?);
        }
    }
    Ok(out)
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
