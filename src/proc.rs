//! Small helpers around running external processes (git, docker, systemctl…).

use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

/// Run a command, capturing stdout. Returns an error if it exits non-zero.
pub fn run<S: AsRef<OsStr>>(program: &str, args: &[S]) -> Result<String> {
    run_in(None, program, args)
}

/// Like [`run`], but with an optional working directory.
pub fn run_in<S: AsRef<OsStr>>(dir: Option<&Path>, program: &str, args: &[S]) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .with_context(|| format!("failed to spawn `{program}` (is it installed?)"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`{program}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a command, inheriting stdout/stderr (for long output like `docker build`).
pub fn run_inherit<S: AsRef<OsStr>>(dir: Option<&Path>, program: &str, args: &[S]) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn `{program}`"))?;
    if !status.success() {
        anyhow::bail!("`{program}` exited with {status}");
    }
    Ok(())
}

/// Run `sh -c <script>` — used where a real shell pipeline is convenient
/// (e.g. `pg_dump | gzip > file`). The target host is Linux.
pub fn shell(script: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(script)
        .status()
        .context("failed to spawn `sh`")?;
    if !status.success() {
        anyhow::bail!("shell command failed: {script}");
    }
    Ok(())
}

/// Whether a program is available on `PATH`.
pub fn exists(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
