//! Host services: GitOps-managed systemd units for workloads that must run on
//! the host with native Docker access (which a HomeOps *container* never gets).
//!
//! The lifecycle mirrors an app's, with "container" swapped for "systemd unit":
//! sync the repo, run an idempotent `setup` script on source change, render a
//! unit running as a dedicated user, and (re)start it. Routing is handled by the
//! Caddy generator and the proxy, which treat a host service's domains like an
//! app's but point at its fixed loopback port instead of an allocated one.

use crate::config::{HostServiceConfig, Paths};
use crate::reconcile::Action;
use crate::state::{self, AppState};
use crate::{git, proc};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// systemd unit directory (same as `systemd.rs`).
const UNIT_DIR: &str = "/etc/systemd/system";
/// Root-only directory holding each host service's `EnvironmentFile`.
const ENV_DIR: &str = "/etc/homeops/host-services";
/// Prefix for host-service units, so they namespace cleanly and `uninstall` can
/// find them.
const UNIT_PREFIX: &str = "homeops-";

/// The systemd unit name for a host service.
pub fn unit_name(name: &str) -> String {
    format!("{UNIT_PREFIX}{name}.service")
}

fn unit_path(name: &str) -> std::path::PathBuf {
    Path::new(UNIT_DIR).join(unit_name(name))
}

fn env_path(name: &str) -> std::path::PathBuf {
    Path::new(ENV_DIR).join(format!("{name}.env"))
}

/// Hash that captures everything that should trigger a unit rewrite/restart.
pub fn config_hash(svc: &HostServiceConfig) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_string(svc).unwrap_or_default().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Reconcile a single host service. Returns the action taken.
pub fn reconcile_one(paths: &Paths, name: &str, svc: &HostServiceConfig) -> Result<Action> {
    let mut st = AppState::load(paths, name)?;
    st.app = name.to_string();

    // 1. Sync source.
    let checkout = paths.checkout(name);
    let git_ref = git::resolve_ref(&svc.repo, svc.r#ref.as_deref())
        .with_context(|| format!("resolving ref for {name}"))?;
    git::sync(&svc.repo, &git_ref, &checkout, svc.tracks_branch())
        .with_context(|| format!("syncing {name}"))?;
    let sha = git::head_commit(&checkout)?;

    // 2. Decide what changed.
    let cfg_hash = config_hash(svc);
    let unit_body = render_unit(name, svc, &checkout, &env_path(name));
    let env_body = render_env(&svc.env);
    let unit = unit_path(name);
    let envf = env_path(name);
    let commit_changed = st.last_deployed_commit.as_deref() != Some(sha.as_str());
    let config_changed = st.last_config_hash.as_deref() != Some(cfg_hash.as_str());
    let files_current = file_matches(&unit, &unit_body) && file_matches(&envf, &env_body);
    if !commit_changed && !config_changed && files_current && is_active(name) {
        return Ok(Action::NoChange);
    }

    state::record_event(paths, name, "host service: reconcile started")?;

    // 3. Ensure the run user exists and can reach Docker (best-effort; needs root).
    ensure_user(&svc.run_as);

    // 4. Write the EnvironmentFile (root-only: it may hold secrets) and the unit.
    write_env_file(&envf, &env_body)?;
    write_unit(&unit, &unit_body)?;

    // 5. Re-provision on source change (idempotent script).
    let reran_setup = if commit_changed {
        if let Some(setup) = svc.setup.as_deref().filter(|s| !s.trim().is_empty()) {
            run_setup(&checkout, setup, &svc.run_as)
                .with_context(|| format!("running setup `{setup}` for {name}"))?;
            state::record_event(paths, name, "host service: setup ok")?;
            true
        } else {
            false
        }
    } else {
        false
    };

    // 6. Activate.
    proc::run("systemctl", &["daemon-reload"])?;
    proc::run("systemctl", &["enable", &unit_name(name)])?;
    proc::run("systemctl", &["restart", &unit_name(name)])
        .with_context(|| format!("starting {}", unit_name(name)))?;

    st.last_deployed_commit = Some(sha);
    st.last_config_hash = Some(cfg_hash);
    st.current_port = Some(svc.port);
    st.last_success_at = Some(chrono::Utc::now().to_rfc3339());
    st.last_error = None;
    st.save(paths)?;
    state::record_event(paths, name, "host service: running")?;

    Ok(if reran_setup {
        Action::RebuildRestart
    } else {
        Action::Restart
    })
}

/// Render the systemd unit text. Pure (no I/O) so it can be unit-tested.
fn render_unit(name: &str, svc: &HostServiceConfig, checkout: &Path, env_file: &Path) -> String {
    let exec = checkout.join(&svc.exec);
    format!(
        "# managed-by: homeops (host_service `{name}`)\n\
         [Unit]\n\
         Description=HomeOps host service {name}\n\
         After=network-online.target docker.service\n\
         Wants=network-online.target docker.service\n\n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         WorkingDirectory={wd}\n\
         EnvironmentFile=-{env}\n\
         ExecStart={exec}\n\
         Restart=always\n\
         RestartSec=3\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        user = svc.run_as,
        wd = checkout.display(),
        env = env_file.display(),
        exec = exec.display(),
    )
}

/// Render the `EnvironmentFile` body. systemd reads `KEY=value` lines literally
/// (no shell expansion), so values are written verbatim. Pure for testability.
fn render_env(env: &BTreeMap<String, String>) -> String {
    let mut out = String::from("# managed-by: homeops — do not edit\n");
    for (k, v) in env {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// Whether a file exists and already holds exactly `body`.
fn file_matches(path: &Path, body: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|c| c == body)
        .unwrap_or(false)
}

/// Whether the unit is currently active (running).
pub fn is_active(name: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", &unit_name(name)])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "active")
        .unwrap_or(false)
}

/// Create the run user (system, nologin) if missing and add it to the `docker`
/// group. Best-effort: requires root, and a non-root reconcile (e.g. local
/// testing) simply skips it — the units it writes would not start, which is the
/// honest outcome.
fn ensure_user(user: &str) {
    if proc::run("id", &["-u", user]).is_err() {
        let _ = proc::run(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                user,
            ],
        );
    }
    let _ = proc::run("usermod", &["-aG", "docker", user]);
}

fn write_env_file(path: &Path, body: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    set_mode_0600(path);
    Ok(())
}

fn write_unit(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Restrict a freshly written EnvironmentFile to root (it may hold secrets).
#[cfg(unix)]
fn set_mode_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) {}

/// Run the idempotent setup script as root, from the checkout, telling it which
/// user the service will run as so it can chown user-owned artifacts.
fn run_setup(checkout: &Path, setup_rel: &str, run_as: &str) -> Result<()> {
    let status = Command::new("bash")
        .arg(setup_rel)
        .current_dir(checkout)
        .env("HOMEOPS_SERVICE_USER", run_as)
        .status()
        .with_context(|| format!("spawning setup `{setup_rel}`"))?;
    if !status.success() {
        anyhow::bail!("setup `{setup_rel}` exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn svc() -> HostServiceConfig {
        nickel_lang_core::deserialize::from_str(
            r#"{
                repo = "git@github.com:you/stackbench.git",
                run_as = "stackbench",
                setup = "deploy/setup.sh",
                exec = "deploy/run.sh",
                port = 8077,
                domains = ["stackbench.example.com"],
                env = { STACKBENCH_LLM_BASE_URL = "https://llm.example.com/v1", KEY = "v" },
            }"#,
        )
        .expect("valid host service")
    }

    #[test]
    fn unit_runs_as_user_from_checkout() {
        let s = svc();
        let checkout = PathBuf::from("/var/lib/homeops/checkouts/stackbench");
        let unit = render_unit("stackbench", &s, &checkout, &env_path("stackbench"));
        assert!(unit.contains("User=stackbench"));
        assert!(unit.contains("WorkingDirectory=/var/lib/homeops/checkouts/stackbench"));
        assert!(unit.contains("ExecStart=/var/lib/homeops/checkouts/stackbench/deploy/run.sh"));
        assert!(unit.contains("EnvironmentFile=-/etc/homeops/host-services/stackbench.env"));
        assert!(unit.contains("# managed-by: homeops"));
    }

    #[test]
    fn env_file_lists_sorted_keys() {
        let s = svc();
        let env = render_env(&s.env);
        // BTreeMap keeps keys sorted: KEY before STACKBENCH_*.
        let key_pos = env.find("KEY=v").unwrap();
        let base_pos = env.find("STACKBENCH_LLM_BASE_URL=").unwrap();
        assert!(key_pos < base_pos);
        assert!(env.starts_with("# managed-by: homeops"));
    }

    #[test]
    fn unit_and_env_paths_are_namespaced() {
        assert_eq!(unit_name("stackbench"), "homeops-stackbench.service");
        assert!(env_path("stackbench").ends_with("stackbench.env"));
    }
}
