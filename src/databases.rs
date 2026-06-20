//! Managed Postgres and MySQL.
//!
//! Databases are managed by HomeOps, never by apps. Each engine runs as a single
//! container with one admin user; apps get a database created for them and a
//! connection string injected as an env var. Deliberately simple.

use crate::config::{Config, DOCKER_NETWORK};
use crate::{docker, proc};
use anyhow::Result;

pub const PG_CONTAINER: &str = "homeops-postgres";
pub const MYSQL_CONTAINER: &str = "homeops-mysql";

/// Bring up enabled database engines (idempotent).
pub fn ensure_engines(cfg: &Config) -> Result<()> {
    docker::ensure_network(DOCKER_NETWORK)?;
    if let Some(pg) = &cfg.databases.postgres {
        if pg.enabled {
            ensure_postgres(&pg.version, &pg.admin_user, &pg.admin_password)?;
        }
    }
    if let Some(my) = &cfg.databases.mysql {
        if my.enabled {
            ensure_mysql(&my.version, &my.admin_user, &my.admin_password)?;
        }
    }
    Ok(())
}

fn ensure_postgres(version: &str, user: &str, password: &str) -> Result<()> {
    if docker::is_running(PG_CONTAINER) {
        return Ok(());
    }
    docker::remove(PG_CONTAINER)?;
    proc::run(
        "docker",
        &[
            "run",
            "-d",
            "--name",
            PG_CONTAINER,
            "--network",
            DOCKER_NETWORK,
            "--restart",
            "unless-stopped",
            "-e",
            &format!("POSTGRES_USER={user}"),
            "-e",
            &format!("POSTGRES_PASSWORD={password}"),
            "-v",
            "homeops-postgres-data:/var/lib/postgresql/data",
            &format!("postgres:{version}"),
        ],
    )?;
    wait_ready(PG_CONTAINER, &["pg_isready", "-U", user])?;
    Ok(())
}

fn ensure_mysql(version: &str, user: &str, password: &str) -> Result<()> {
    if docker::is_running(MYSQL_CONTAINER) {
        return Ok(());
    }
    docker::remove(MYSQL_CONTAINER)?;
    proc::run(
        "docker",
        &[
            "run",
            "-d",
            "--name",
            MYSQL_CONTAINER,
            "--network",
            DOCKER_NETWORK,
            "--restart",
            "unless-stopped",
            "-e",
            &format!("MYSQL_ROOT_PASSWORD={password}"),
            "-v",
            "homeops-mysql-data:/var/lib/mysql",
            &format!("mysql:{version}"),
        ],
    )?;
    // MySQL takes a while to initialize; best-effort wait.
    wait_ready(
        MYSQL_CONTAINER,
        &["mysqladmin", "ping", "-u", "root", &format!("-p{password}")],
    )?;
    let _ = user; // root is used as admin for MySQL
    Ok(())
}

/// Poll a container until an in-container readiness command succeeds.
fn wait_ready(container: &str, check: &[&str]) -> Result<()> {
    for _ in 0..60 {
        let mut args = vec!["exec", container];
        args.extend_from_slice(check);
        if proc::run("docker", &args).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    anyhow::bail!("database container {container} did not become ready in time");
}

/// Create a Postgres database if it does not exist. Returns the connection URL.
pub fn ensure_postgres_db(cfg: &Config, database: &str) -> Result<String> {
    let pg = cfg
        .databases
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| anyhow::anyhow!("postgres is not enabled but an app requested it"))?;

    let exists = proc::run(
        "docker",
        &[
            "exec",
            PG_CONTAINER,
            "psql",
            "-U",
            &pg.admin_user,
            "-tAc",
            &format!("SELECT 1 FROM pg_database WHERE datname='{database}'"),
        ],
    )?;
    if exists.trim() != "1" {
        proc::run(
            "docker",
            &[
                "exec",
                PG_CONTAINER,
                "createdb",
                "-U",
                &pg.admin_user,
                database,
            ],
        )?;
    }
    Ok(format!(
        "postgres://{}:{}@{}:5432/{}",
        pg.admin_user, pg.admin_password, PG_CONTAINER, database
    ))
}

/// Create a MySQL database if it does not exist. Returns the connection URL.
pub fn ensure_mysql_db(cfg: &Config, database: &str) -> Result<String> {
    let my = cfg
        .databases
        .mysql
        .as_ref()
        .filter(|m| m.enabled)
        .ok_or_else(|| anyhow::anyhow!("mysql is not enabled but an app requested it"))?;

    proc::run(
        "docker",
        &[
            "exec",
            MYSQL_CONTAINER,
            "mysql",
            "-u",
            "root",
            &format!("-p{}", my.admin_password),
            "-e",
            &format!("CREATE DATABASE IF NOT EXISTS `{database}`"),
        ],
    )?;
    Ok(format!(
        "mysql://root:{}@{}:3306/{}",
        my.admin_password, MYSQL_CONTAINER, database
    ))
}
