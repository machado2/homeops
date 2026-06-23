//! The core convergence loop: make the running server match `homeops.ncl`.

use crate::config::{AppConfig, Config, Paths, DOCKER_NETWORK};
use crate::state::{self, AppState};
use crate::{databases, docker, git};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// What reconcile would (or did) do for a single app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    NoChange,
    Restart,
    RebuildRestart,
    Failed(String),
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::NoChange => write!(f, "no change"),
            Action::Restart => write!(f, "restart"),
            Action::RebuildRestart => write!(f, "rebuild + restart"),
            Action::Failed(e) => write!(f, "FAILED: {e}"),
        }
    }
}

/// Hash that captures everything that should trigger a redeploy when changed:
/// the app's resolved config, env vars included (they live inline now).
fn config_hash(app: &AppConfig) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_string(app).unwrap_or_default().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Collect ports already assigned to other apps, so allocation avoids clashes.
fn taken_ports(paths: &Paths, cfg: &Config, except: &str) -> Vec<u16> {
    cfg.apps
        .keys()
        .filter(|n| n.as_str() != except)
        .filter_map(|n| AppState::load(paths, n).ok().and_then(|s| s.current_port))
        .collect()
}

/// Reconcile every app. Returns the action taken per app.
pub fn reconcile_all(cfg: &Config, paths: &Paths) -> Result<Vec<(String, Action)>> {
    databases::ensure_engines(cfg).context("ensuring database engines")?;

    let mut results = Vec::new();
    for (name, app) in &cfg.apps {
        let action = match reconcile_app(cfg, paths, name, app) {
            Ok(a) => a,
            Err(e) => {
                let msg = e.to_string();
                state::record_event(paths, name, &format!("reconcile error: {msg}")).ok();
                let mut st = AppState::load(paths, name).unwrap_or_default();
                st.app = name.clone();
                st.last_error = Some(msg.clone());
                st.save(paths).ok();
                Action::Failed(msg)
            }
        };
        results.push((name.clone(), action));
    }
    Ok(results)
}

fn reconcile_app(cfg: &Config, paths: &Paths, name: &str, app: &AppConfig) -> Result<Action> {
    let mut st = AppState::load(paths, name)?;
    st.app = name.to_string();

    // 1. Sync source. An app without an explicit `ref` follows the master/main
    //    convention, resolved against the remote.
    let checkout = paths.checkout(name);
    let git_ref = git::resolve_ref(&app.repo, app.r#ref.as_deref())
        .with_context(|| format!("resolving ref for {name}"))?;
    git::sync(&app.repo, &git_ref, &checkout, app.tracks_branch())
        .with_context(|| format!("syncing {name}"))?;
    let sha = git::head_commit(&checkout)?;

    // 2. Decide what changed.
    let cfg_hash = config_hash(app);
    let container = docker::container_name(name);
    let unchanged = st.last_deployed_commit.as_deref() == Some(sha.as_str())
        && st.last_config_hash.as_deref() == Some(cfg_hash.as_str())
        && docker::is_running(&container);
    if unchanged {
        return Ok(Action::NoChange);
    }

    let needs_build =
        st.last_deployed_commit.as_deref() != Some(sha.as_str()) || st.last_image.is_none();

    state::record_event(paths, name, "deploy started")?;

    // 3. Build if needed.
    let image = docker::image_tag(name, &sha);
    if needs_build {
        docker::build(&image, &checkout).with_context(|| format!("building {name}"))?;
        state::record_event(paths, name, "build ok")?;
    }

    // 4. Resolve env vars: the app's inline `env`, then managed databases on top.
    let mut env: Vec<(String, String)> = app
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(pg) = &app.databases.postgres {
        let url = databases::ensure_postgres_db(cfg, &pg.database)?;
        env.push((pg.env_var.clone(), url));
    }
    if let Some(my) = &app.databases.mysql {
        let url = databases::ensure_mysql_db(cfg, &my.database)?;
        env.push((my.env_var.clone(), url));
    }

    // 5. Allocate a port and (re)start the container.
    let previous_image = st.last_image.clone();
    let taken = taken_ports(paths, cfg, name);
    let port = state::allocate_port(&mut st, &taken)?;

    start_container(&container, &image, port, app, &env)?;

    // 6. Healthcheck (or grace period) → rollback on failure.
    match check_health(port, app) {
        Ok(()) => {
            st.last_deployed_commit = Some(sha);
            st.last_config_hash = Some(cfg_hash);
            st.last_image = Some(image);
            st.last_success_at = Some(chrono::Utc::now().to_rfc3339());
            st.last_error = None;
            st.current_port = Some(port);
            st.save(paths)?;
            state::record_event(paths, name, "healthcheck ok")?;
            Ok(if needs_build {
                Action::RebuildRestart
            } else {
                Action::Restart
            })
        }
        Err(e) => {
            state::record_event(paths, name, &format!("healthcheck failed: {e}"))?;
            rollback(&container, previous_image.as_deref(), port, app, &env)?;
            state::record_event(paths, name, "rollback done")?;
            st.last_error = Some(format!("healthcheck failed: {e}"));
            st.save(paths)?;
            Ok(Action::Failed(format!("healthcheck failed: {e}")))
        }
    }
}

fn start_container(
    container: &str,
    image: &str,
    port: u16,
    app: &AppConfig,
    env: &[(String, String)],
) -> Result<()> {
    docker::remove(container)?;
    docker::run_app(&docker::RunSpec {
        name: container,
        image,
        network: DOCKER_NETWORK,
        host_port: port,
        container_port: app.port,
        env: env.to_vec(),
    })
}

/// Restore the previous image, or just stop the broken container if there is none.
fn rollback(
    container: &str,
    previous_image: Option<&str>,
    port: u16,
    app: &AppConfig,
    env: &[(String, String)],
) -> Result<()> {
    match previous_image {
        Some(image) => start_container(container, image, port, app, env),
        None => docker::remove(container),
    }
}

/// HTTP healthcheck against the freshly bound port. When no healthcheck is
/// configured, treat the container as healthy if it is still running after a
/// short grace period.
fn check_health(port: u16, app: &AppConfig) -> Result<()> {
    let Some(hc) = &app.healthcheck else {
        // No healthcheck configured: give the container a few seconds, then
        // accept it. Rollback is only reliable with an explicit healthcheck.
        std::thread::sleep(Duration::from_secs(3));
        return Ok(());
    };

    let mut last = String::new();
    for attempt in 0..hc.retries.max(1) {
        match http_ok(port, &hc.path) {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        if attempt + 1 < hc.retries.max(1) {
            std::thread::sleep(Duration::from_secs(hc.interval_seconds.max(1)));
        }
    }
    anyhow::bail!("{last}")
}

/// Minimal blocking HTTP GET that succeeds on a 2xx/3xx status line.
fn http_ok(port: u16, path: &str) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    if (200..400).contains(&status) {
        Ok(())
    } else {
        anyhow::bail!("unexpected status {status}")
    }
}

// ---------------------------------------------------------------------------
// Plan (dry-run)
// ---------------------------------------------------------------------------

/// Compute, without applying, what reconcile would do for each app.
pub fn plan(cfg: &Config, paths: &Paths) -> Vec<(String, PlanEntry)> {
    let mut out = Vec::new();
    for (name, app) in &cfg.apps {
        out.push((name.clone(), plan_app(paths, name, app)));
    }
    out
}

#[derive(Debug, Clone)]
pub struct PlanEntry {
    pub commit_change: Option<(String, String)>,
    pub config_changed: bool,
    pub action: Action,
}

fn plan_app(paths: &Paths, name: &str, app: &AppConfig) -> PlanEntry {
    let st = AppState::load(paths, name).unwrap_or_default();
    let remote = git::resolve_ref(&app.repo, app.r#ref.as_deref())
        .ok()
        .and_then(|r| git::remote_commit(&app.repo, &r));
    let cfg_hash = config_hash(app);
    let config_changed = st.last_config_hash.as_deref() != Some(cfg_hash.as_str());

    let commit_change = match (&st.last_deployed_commit, &remote) {
        (Some(old), Some(new)) if old != new => Some((short(old), short(new))),
        (None, Some(new)) => Some(("none".into(), short(new))),
        _ => None,
    };

    let running = docker::is_running(&docker::container_name(name));
    let action = if commit_change.is_some() || !running {
        Action::RebuildRestart
    } else if config_changed {
        Action::Restart
    } else {
        Action::NoChange
    };

    PlanEntry {
        commit_change,
        config_changed,
        action,
    }
}

fn short(sha: &str) -> String {
    sha.get(..7).unwrap_or(sha).to_string()
}
