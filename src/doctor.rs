//! Environment diagnostics.

use crate::config::{Bootstrap, Config, Paths};
use crate::{databases, docker, proc};

pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            ok: true,
            detail: detail.into(),
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            ok: false,
            detail: detail.into(),
        }
    }
}

/// Run all diagnostics. `cfg`/`bootstrap` are optional because `doctor` should
/// be useful even on a half-installed machine.
pub fn run(bootstrap: Option<&Bootstrap>, cfg: Option<&Config>, paths: &Paths) -> Vec<Check> {
    let mut checks = Vec::new();

    // Tooling.
    checks.push(if proc::exists("docker") {
        Check::ok("docker", "installed")
    } else {
        Check::fail("docker", "not found on PATH")
    });
    checks.push(if proc::exists("git") {
        Check::ok("git", "installed")
    } else {
        Check::fail("git", "not found on PATH")
    });
    checks.push(if proc::exists("systemctl") {
        Check::ok("systemd", "available")
    } else {
        Check::fail("systemd", "systemctl not found")
    });

    // Systemd units.
    for unit in ["homeops-web.service", "homeops-reconcile.timer"] {
        let active = proc::run("systemctl", &["is-enabled", unit]).is_ok();
        checks.push(if active {
            Check::ok(unit, "installed")
        } else {
            Check::fail(unit, "not installed/enabled")
        });
    }

    // Directories.
    checks.push(if paths.workdir.exists() {
        Check::ok("workdir", paths.workdir.display().to_string())
    } else {
        Check::fail("workdir", format!("missing: {}", paths.workdir.display()))
    });

    // Infra repo reachability.
    if let Some(b) = bootstrap {
        match crate::git::remote_commit(&b.infra_repo, &b.infra_ref) {
            Some(sha) => checks.push(Check::ok(
                "infra repo",
                format!("{} @ {}", b.infra_repo, &sha[..7.min(sha.len())]),
            )),
            None => checks.push(Check::fail(
                "infra repo",
                format!("cannot reach {} (SSH key / network?)", b.infra_repo),
            )),
        }
    }

    // App repos.
    if let Some(cfg) = cfg {
        for (name, app) in &cfg.apps {
            match crate::git::resolve_ref(&app.repo, app.r#ref.as_deref()) {
                Ok(git_ref) if crate::git::remote_commit(&app.repo, &git_ref).is_some() => {
                    checks.push(Check::ok(
                        &format!("app:{name}"),
                        format!("repo reachable ({git_ref})"),
                    ));
                }
                Ok(_) => checks.push(Check::fail(
                    &format!("app:{name}"),
                    format!("cannot reach {}", app.repo),
                )),
                Err(e) => checks.push(Check::fail(&format!("app:{name}"), e.to_string())),
            }
        }

        // Databases.
        if cfg.databases.postgres.as_ref().is_some_and(|p| p.enabled) {
            checks.push(if docker::is_running(databases::PG_CONTAINER) {
                Check::ok("postgres", "running")
            } else {
                Check::fail("postgres", "enabled but not running")
            });
        }
        if cfg.databases.mysql.as_ref().is_some_and(|m| m.enabled) {
            checks.push(if docker::is_running(databases::MYSQL_CONTAINER) {
                Check::ok("mysql", "running")
            } else {
                Check::fail("mysql", "enabled but not running")
            });
        }
    }

    checks
}
