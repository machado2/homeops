//! systemd integration: install/uninstall units and the install manifest.

use crate::config::{Bootstrap, Paths};
use crate::proc;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SYSTEMD_DIR: &str = "/etc/systemd/system";
const SERVICE_USER: &str = "homeops";

/// Record of everything `install` created, so `uninstall` can undo exactly that.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct InstallManifest {
    pub created_files: Vec<String>,
    pub created_dirs: Vec<String>,
    pub created_users: Vec<String>,
}

impl InstallManifest {
    pub fn load(path: &Path) -> Result<InstallManifest> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        Ok(serde_json::from_str(&raw)?)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

fn web_service(exe: &str) -> String {
    format!(
        "[Unit]\n\
         Description=HomeOps web/proxy server\n\
         After=network.target docker.service\n\
         Wants=docker.service\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} serve\n\
         Restart=always\n\
         RestartSec=2\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

fn reconcile_service(exe: &str) -> String {
    format!(
        "[Unit]\n\
         Description=HomeOps reconcile (oneshot)\n\
         After=network.target docker.service\n\
         Wants=docker.service\n\n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe} reconcile\n"
    )
}

fn reconcile_timer(interval: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Run HomeOps reconcile periodically\n\n\
         [Timer]\n\
         OnBootSec=1min\n\
         OnUnitActiveSec={interval}\n\
         Persistent=true\n\n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// Install HomeOps into systemd. Writes the bootstrap file (if provided), the
/// units, the directory layout, and the install manifest.
pub fn install(
    paths: &Paths,
    bootstrap: Option<&Bootstrap>,
    bootstrap_path: &Path,
    interval: &str,
) -> Result<()> {
    let exe = std::env::current_exe()
        .context("resolving own executable path")?
        .to_string_lossy()
        .into_owned();

    let mut manifest = InstallManifest::default();

    // Service user (best-effort; ignore "already exists").
    if proc::run("id", &["-u", SERVICE_USER]).is_err() {
        let _ = proc::run(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                SERVICE_USER,
            ],
        );
        manifest.created_users.push(SERVICE_USER.to_string());
    }

    // Directories.
    paths.ensure_dirs()?;
    for d in [
        &paths.workdir,
        &paths.infra,
        &paths.checkouts,
        &paths.state,
        &paths.backups,
    ] {
        manifest.created_dirs.push(d.display().to_string());
    }

    // Bootstrap file.
    if let Some(b) = bootstrap {
        b.save(bootstrap_path)?;
        manifest
            .created_files
            .push(bootstrap_path.display().to_string());
    }

    // Units.
    let units: [(&str, String); 3] = [
        ("homeops-web.service", web_service(&exe)),
        ("homeops-reconcile.service", reconcile_service(&exe)),
        ("homeops-reconcile.timer", reconcile_timer(interval)),
    ];
    for (name, body) in &units {
        let path = PathBuf::from(SYSTEMD_DIR).join(name);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        manifest.created_files.push(path.display().to_string());
    }

    manifest.save(&paths.install_manifest)?;

    // Activate.
    proc::run("systemctl", &["daemon-reload"])?;
    proc::run("systemctl", &["enable", "--now", "homeops-web.service"])?;
    proc::run("systemctl", &["enable", "--now", "homeops-reconcile.timer"])?;

    Ok(())
}

/// Remove what `install` created. With `purge`, also delete data directories.
pub fn uninstall(paths: &Paths, purge: bool) -> Result<()> {
    let manifest = InstallManifest::load(&paths.install_manifest).unwrap_or_default();

    // Stop & disable units first.
    for unit in [
        "homeops-reconcile.timer",
        "homeops-reconcile.service",
        "homeops-web.service",
    ] {
        let _ = proc::run("systemctl", &["disable", "--now", unit]);
    }

    for file in &manifest.created_files {
        // Preserve the bootstrap file unless purging.
        if !purge && file.ends_with("bootstrap.toml") {
            continue;
        }
        let _ = std::fs::remove_file(file);
    }
    let _ = proc::run("systemctl", &["daemon-reload"]);

    if purge {
        // Remove containers, volumes and all local data.
        let _ = proc::run(
            "sh",
            &[
                "-c",
                "docker rm -f $(docker ps -aq --filter name=^homeops-) 2>/dev/null || true",
            ],
        );
        let _ = proc::run(
            "sh",
            &[
                "-c",
                "docker volume rm homeops-postgres-data homeops-mysql-data 2>/dev/null || true",
            ],
        );
        let _ = std::fs::remove_dir_all(&paths.workdir);
        for user in &manifest.created_users {
            let _ = proc::run("userdel", &[user.as_str()]);
        }
    }

    Ok(())
}
