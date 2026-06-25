//! Thin wrappers over the `docker` CLI.

use crate::proc;
use anyhow::Result;
use std::path::Path;

/// Prefix for every container HomeOps manages.
pub const CONTAINER_PREFIX: &str = "homeops-";

pub fn container_name(app: &str) -> String {
    format!("{CONTAINER_PREFIX}{app}")
}

pub fn image_tag(app: &str, sha: &str) -> String {
    let short = sha.get(..12).unwrap_or(sha);
    format!("homeops/{app}:{short}")
}

/// Ensure the shared user-defined network exists.
pub fn ensure_network(name: &str) -> Result<()> {
    let existing = proc::run("docker", &["network", "ls", "--format", "{{.Name}}"])?;
    if existing.lines().any(|n| n == name) {
        return Ok(());
    }
    proc::run("docker", &["network", "create", name])?;
    Ok(())
}

/// Build an image from the `Dockerfile` at the root of `context_dir`.
pub fn build(image: &str, context_dir: &Path) -> Result<()> {
    proc::run_inherit(
        None,
        "docker",
        &["build", "-t", image, context_dir.to_str().unwrap_or(".")],
    )
}

/// Whether a container with this name exists (running or stopped).
pub fn container_exists(name: &str) -> bool {
    proc::run(
        "docker",
        &[
            "ps",
            "-a",
            "--filter",
            &format!("name=^{name}$"),
            "--format",
            "{{.Names}}",
        ],
    )
    .map(|o| o.lines().any(|n| n == name))
    .unwrap_or(false)
}

/// Whether a container is currently running.
pub fn is_running(name: &str) -> bool {
    proc::run(
        "docker",
        &[
            "ps",
            "--filter",
            &format!("name=^{name}$"),
            "--format",
            "{{.Names}}",
        ],
    )
    .map(|o| o.lines().any(|n| n == name))
    .unwrap_or(false)
}

/// Remove a container (forcefully), ignoring "no such container".
pub fn remove(name: &str) -> Result<()> {
    if container_exists(name) {
        proc::run("docker", &["rm", "-f", name])?;
    }
    Ok(())
}

/// Options for running an app container.
pub struct RunSpec<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub network: &'a str,
    pub host_port: u16,
    pub container_port: u16,
    pub env: Vec<(String, String)>,
    /// Host-backed bind mounts. The host paths are absolute and pre-created by
    /// HomeOps, so docker treats them as bind mounts (never named volumes).
    pub volumes: Vec<VolumeMount>,
}

/// A single bind mount for an app container.
pub struct VolumeMount {
    pub host: String,
    pub container: String,
    pub read_only: bool,
}

/// Start a detached app container bound to localhost only.
pub fn run_app(spec: &RunSpec) -> Result<()> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.name.into(),
        "--network".into(),
        spec.network.into(),
        "--restart".into(),
        "unless-stopped".into(),
        "-p".into(),
        format!("127.0.0.1:{}:{}", spec.host_port, spec.container_port),
        "-e".into(),
        format!("PORT={}", spec.container_port),
    ];
    for (k, v) in &spec.env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    for v in &spec.volumes {
        args.push("-v".into());
        let ro = if v.read_only { ":ro" } else { "" };
        args.push(format!("{}:{}{ro}", v.host, v.container));
    }
    args.push(spec.image.into());
    proc::run("docker", &args)?;
    Ok(())
}
