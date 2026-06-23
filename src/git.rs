//! Thin wrappers over the `git` CLI.

use crate::proc;
use anyhow::{Context, Result};
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

/// The convention for an app that does not pin a `ref`: track `master` if the
/// repo has it, otherwise `main`, otherwise abort. Master wins even when both
/// exist (a single `ls-remote --heads` lists every branch, so this is one
/// round-trip).
pub fn default_branch(repo: &str) -> Result<String> {
    let heads = proc::run("git", &["ls-remote", "--heads", repo])
        .with_context(|| format!("listing branches of {repo}"))?;
    let has = |branch: &str| {
        heads
            .lines()
            .any(|l| l.ends_with(&format!("refs/heads/{branch}")))
    };
    if has("master") {
        Ok("master".to_string())
    } else if has("main") {
        Ok("main".to_string())
    } else {
        anyhow::bail!("{repo} has neither a `master` nor a `main` branch — pin an explicit `ref`")
    }
}

/// Resolve the ref an app should track: its explicit `ref`, or the master/main
/// convention when omitted.
pub fn resolve_ref(repo: &str, explicit: Option<&str>) -> Result<String> {
    match explicit {
        Some(r) => Ok(r.to_string()),
        None => default_branch(repo),
    }
}
