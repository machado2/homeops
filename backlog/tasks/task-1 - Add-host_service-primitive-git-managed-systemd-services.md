---
id: TASK-1
title: Add host_service primitive (git-managed systemd services)
status: Done
assignee:
  - '@claude'
created_date: '2026-06-26 07:22'
updated_date: '2026-06-26 07:35'
labels: []
dependencies: []
ordinal: 1000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
HomeOps runs apps as containers and, by design, never gives a container the Docker socket (no socket mount, no privileged, no host networking, no raw docker args). Some workloads must run on the host with native Docker access — the motivating case is StackBench, whose web panel launches 'inspect eval' that drives 'docker compose' to build/run per-stack sandboxes. Today such a service is a hand-built systemd unit on the VPS with a hand-managed Caddy route, sitting outside GitOps. Add a 'host_services' map to homeops.ncl so these are declared and reconciled from git like apps: clone the repo, run an idempotent setup script, render and manage a systemd unit running as a dedicated user, and register routing (Caddy generation + HomeOps proxy upstream pointing at 127.0.0.1:<port>) plus optional basic auth. This restores disposability without distorting the container app contract.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 homeops.ncl accepts a 'host_services' map (repo, ref, user, setup, exec, port, domains, env, pass/user) and the config deserializes + validates (domain uniqueness shared with apps; reject bad names/ports)
- [x] #2 reconcile clones/fetches the host_service repo, runs the setup script idempotently, renders /etc/systemd/system/<name>.service running 'exec' as the configured user with the env injected, and daemon-reload + enable/restart on change
- [x] #3 host_service domains participate in Caddy 'full' generation and the HomeOps proxy routes those domains to 127.0.0.1:<port> (with basic auth when 'pass' is set), so no hand-managed Caddy block is needed
- [x] #4 privilege model for the reconcile path is documented and implemented (root at install, or a scoped sudoers entry for restart) without broadening reconcile's privileges beyond what's required
- [x] #5 README documents the host_service contract and migrating StackBench onto it
- [x] #6 unit tests cover config parse/validate and unit/route rendering
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. config.rs: add `host_services: BTreeMap<String, HostServiceConfig>` to Config. New struct HostServiceConfig { repo, ref, tracking, run_as (system user), setup (opt script rel. path), exec (script rel. path), port, domains, env, pass, user (basic-auth, like apps) }. Methods tracks_branch() and basic_auth() mirroring AppConfig. Extend validate(): names valid + globally unique across apps+host_services; domains share the same uniqueness map as apps/admin; run_as valid; exec non-empty; port != 0 and not in app range 41000-41999.
2. New module host_service.rs: reconcile_one(cfg, paths, name, svc) -> Action. Steps: git::sync checkout; ensure run_as user (useradd --system + add to docker group, best-effort/root); render EnvironmentFile /etc/homeops/host-services/<name>.env (0600) from svc.env; if repo commit changed, run setup script as root with HOMEOPS_SERVICE_USER=<run_as>; render /etc/systemd/system/homeops-<name>.service (User=run_as, WorkingDirectory=checkout, ExecStart=checkout/exec, EnvironmentFile, After/Wants docker, Restart=always); daemon-reload; enable --now; restart when unit/env/commit changed. Track changes via reused AppState (last_deployed_commit/last_config_hash). Map to Action::{NoChange,Restart,RebuildRestart}.
3. reconcile.rs: after the app loop in reconcile_all, iterate cfg.host_services and call host_service::reconcile_one, pushing into the same results vec (error -> Action::Failed like apps).
4. caddy.rs render_sites: also emit reverse_proxy stanzas for host_service domains (→ same homeops proxy target).
5. serve.rs: resolve host by app first, then host_service; host_service upstream = 127.0.0.1:<svc.port> (fixed, from config, not AppState); apply basic_auth() challenge like apps.
6. systemd.rs uninstall: best-effort disable+rm of /etc/systemd/system/homeops-*.service host-service units; remove /etc/homeops/host-services on purge.
7. README.md: document the host_service contract + StackBench migration + privilege model (reconcile runs as root, same as today's reconcile.service; host services need root to write units/daemon.json — documented).
8. examples/homeops.ncl: add a host_services example. Tests in config.rs: parse/validate (domain clash across app/host_service, port-range reject, name uniqueness) + a unit/env render test in host_service.rs. Run cargo fmt + clippy + test.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented across config.rs (HostServiceConfig + Config.host_services + validate_host_service + shared name/domain uniqueness), new module host_service.rs (reconcile_one: git sync, ensure_user in docker group, root-only EnvironmentFile 0600, idempotent setup on commit change with HOMEOPS_SERVICE_USER, render+write systemd unit homeops-<name>.service, daemon-reload/enable/restart; pure render_unit/render_env; is_active/config_hash exposed for plan), reconcile.rs (host-service loop in reconcile_all + plan_host_service + shared record_failure), caddy.rs (host-service domains in render_sites), serve.rs (resolve_host_service + proxy upstream 127.0.0.1:<port> + basic auth), systemd.rs (uninstall glob-removes homeops-*.service units + /etc/homeops/host-services). Routing reuses the existing proxy/Caddy path; host-service upstream port is fixed from config (not allocated). Privilege model: reconcile already runs as root; host services use that root, do not broaden it; unprivileged reconcile writes units that won't start (honest outcome) — documented in README. Stackbench side: added deploy/ (setup.sh, run.sh, stackbench.service, apikeys.env.example, README.md); run.sh makes apikeys.env optional so HomeOps EnvironmentFile works; setup.sh honors HOMEOPS_SERVICE_USER. Infra repo: updated homeops.ncl comment + README host-service section. Validation: cargo test 34 passed (8 new), cargo fmt --check clean, cargo clippy clean; examples/homeops.ncl and infra/homeops.ncl both evaluate via the nickel binary.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Added a host_service primitive so workloads needing native Docker access (StackBench) are declared in homeops.ncl and reconciled from git as systemd units instead of containers: clone repo, idempotent setup, render unit running as a dedicated docker-group user with a root-only EnvironmentFile, and route domains via the existing Caddy generation + HomeOps proxy to a fixed loopback port (with optional basic auth). Verified: 34 unit tests pass (8 new for parse/validate/render), fmt+clippy clean, example configs evaluate. Companion deploy/ scaffolding added to the stackbench repo and docs updated in the infra repo.
<!-- SECTION:FINAL_SUMMARY:END -->
