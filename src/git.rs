//! Thin wrappers over the `git` CLI.

use crate::proc;
use anyhow::Result;
use std::path::Path;

/// Clone `repo` into `dest` if missing, otherwise fetch updates.
/// Then check out `git_ref` and, when tracking a branch, fast-forward to origin.
pub fn sync(repo: &str, git_ref: &str, dest: &Path, track_branch: bool) -> Result<()> {
    if dest.join(".git").exists() {
        proc::run_in(Some(dest), "git", &["fetch", "--all", "--prune", "--tags"])?;
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        proc::run("git", &["clone", repo, dest.to_str().unwrap_or_default()])?;
    }

    proc::run_in(Some(dest), "git", &["checkout", "--force", git_ref])?;

    if track_branch {
        // Fast-forward the checked-out branch to its upstream tip.
        let _ = proc::run_in(
            Some(dest),
            "git",
            &["reset", "--hard", &format!("origin/{git_ref}")],
        );
    }
    Ok(())
}

/// Resolve the currently checked-out commit SHA.
pub fn head_commit(dir: &Path) -> Result<String> {
    proc::run_in(Some(dir), "git", &["rev-parse", "HEAD"])
}

/// Resolve the SHA a remote ref currently points at, without checking it out.
/// Returns `None` if the ref cannot be resolved.
pub fn remote_commit(repo: &str, git_ref: &str) -> Option<String> {
    let out = proc::run("git", &["ls-remote", repo, git_ref]).ok()?;
    out.split_whitespace().next().map(|s| s.to_string())
}
