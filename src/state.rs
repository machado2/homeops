//! Local observed state, the reconcile lock, and the per-app event timeline.
//!
//! State lives outside Git on purpose: it records what actually happened on this
//! machine (deployed commit, current image/port) so reconcile can avoid
//! redundant work and so rollback has something to fall back to.

use crate::config::Paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AppState {
    pub app: String,
    #[serde(default)]
    pub last_deployed_commit: Option<String>,
    #[serde(default)]
    pub last_config_hash: Option<String>,
    #[serde(default)]
    pub last_image: Option<String>,
    #[serde(default)]
    pub last_success_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub current_port: Option<u16>,
}

impl AppState {
    pub fn load(paths: &Paths, app: &str) -> Result<AppState> {
        let file = paths.state_file(app);
        if !file.exists() {
            return Ok(AppState {
                app: app.to_string(),
                ..Default::default()
            });
        }
        let raw = std::fs::read_to_string(&file)
            .with_context(|| format!("reading state {}", file.display()))?;
        let st: AppState = serde_json::from_str(&raw)
            .with_context(|| format!("parsing state {}", file.display()))?;
        Ok(st)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.state)?;
        let file = paths.state_file(&self.app);
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(&file, raw).with_context(|| format!("writing state {}", file.display()))?;
        Ok(())
    }
}

/// Allocate a stable local port for an app, persisting it in state.
///
/// Reuses the already-assigned port when present; otherwise picks the lowest
/// free port in the configured range that no other app currently holds.
pub fn allocate_port(app_state: &mut AppState, taken: &[u16]) -> Result<u16> {
    if let Some(p) = app_state.current_port {
        return Ok(p);
    }
    for port in crate::config::PORT_RANGE_START..=crate::config::PORT_RANGE_END {
        if !taken.contains(&port) {
            app_state.current_port = Some(port);
            return Ok(port);
        }
    }
    anyhow::bail!("no free ports left in the HomeOps range");
}

// ---------------------------------------------------------------------------
// Event timeline
// ---------------------------------------------------------------------------

/// Append a line to an app's event timeline (human-readable, newest last).
pub fn record_event(paths: &Paths, app: &str, message: &str) -> Result<()> {
    std::fs::create_dir_all(&paths.state)?;
    let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!("{ts} {app} {message}\n");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.events_file(app))?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Read the last `n` events for an app.
pub fn recent_events(paths: &Paths, app: &str, n: usize) -> Vec<String> {
    let file = paths.events_file(app);
    let Ok(raw) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let all: Vec<String> = raw.lines().map(|s| s.to_string()).collect();
    let start = all.len().saturating_sub(n);
    all[start..].to_vec()
}

// ---------------------------------------------------------------------------
// Reconcile lock
// ---------------------------------------------------------------------------

/// A process-wide lock that prevents concurrent reconciles (timer, "deploy now",
/// manual CLI runs). The lock file is removed when this guard is dropped.
pub struct ReconcileLock {
    path: PathBuf,
}

impl ReconcileLock {
    pub fn acquire(paths: &Paths) -> Result<ReconcileLock> {
        std::fs::create_dir_all(&paths.run)
            .with_context(|| format!("creating {}", paths.run.display()))?;
        let path = paths.lock_file();
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                Ok(ReconcileLock { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                anyhow::bail!(
                    "another reconcile is already running (lock: {})",
                    path.display()
                )
            }
            Err(e) => Err(e).with_context(|| format!("creating lock {}", path.display())),
        }
    }
}

impl Drop for ReconcileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PORT_RANGE_START;

    #[test]
    fn allocates_lowest_free_port() {
        let mut st = AppState::default();
        let port = allocate_port(&mut st, &[]).unwrap();
        assert_eq!(port, PORT_RANGE_START);
        assert_eq!(st.current_port, Some(PORT_RANGE_START));
    }

    #[test]
    fn skips_taken_ports() {
        let mut st = AppState::default();
        let port = allocate_port(&mut st, &[PORT_RANGE_START, PORT_RANGE_START + 1]).unwrap();
        assert_eq!(port, PORT_RANGE_START + 2);
    }

    #[test]
    fn reuses_assigned_port() {
        let mut st = AppState {
            current_port: Some(41500),
            ..Default::default()
        };
        // Even though 41500 is "taken", it is this app's own port, so it is reused.
        let port = allocate_port(&mut st, &[41500]).unwrap();
        assert_eq!(port, 41500);
    }
}
