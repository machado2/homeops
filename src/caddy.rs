//! Optional Caddy integration.
//!
//! HomeOps does not take over the server by default. When asked, it can emit a
//! Caddy fragment (recommended) or a full Caddyfile (only over a file it owns).

use crate::config::{Config, CADDY_OWNERSHIP_MARKER};
use anyhow::{Context, Result};
use std::path::Path;

/// Render the reverse-proxy stanzas pointing every app/admin domain at the
/// HomeOps proxy. `proxy_target` is where Caddy should forward (the proxy port).
fn render_sites(cfg: &Config, proxy_target: &str) -> String {
    let mut out = String::new();
    let mut domains: Vec<String> = cfg.apps.values().flat_map(|a| a.domains.clone()).collect();
    // Host services route through the same HomeOps proxy as apps; their domains
    // must appear here too or `full` mode would drop them.
    domains.extend(cfg.host_services.values().flat_map(|s| s.domains.clone()));
    if let Some(d) = &cfg.proxy.admin_domain {
        domains.push(d.clone());
    }
    for domain in domains {
        out.push_str(&format!(
            "{domain} {{\n  reverse_proxy {proxy_target}\n}}\n\n"
        ));
    }
    out
}

/// Apply the configured Caddy mode. Returns a human-readable description of what
/// happened (or that nothing did).
pub fn apply(cfg: &Config) -> Result<String> {
    let proxy_target = proxy_target(cfg);
    match cfg.caddy.mode.as_str() {
        "off" => Ok("caddy: off (not touched)".into()),
        "fragment" => {
            let output = cfg
                .caddy
                .output
                .as_deref()
                .unwrap_or("/etc/caddy/homeops.caddy");
            let body = render_sites(cfg, &proxy_target);
            write_fragment(Path::new(output), &body)?;
            Ok(format!(
                "caddy: wrote fragment {output} (add `import {output}` to your Caddyfile)"
            ))
        }
        "full" => {
            let output = cfg
                .caddy
                .output
                .as_deref()
                .unwrap_or("/etc/caddy/Caddyfile");
            let body = format!(
                "{CADDY_OWNERSHIP_MARKER}\n\n{}",
                render_sites(cfg, &proxy_target)
            );
            write_full(Path::new(output), &body)?;
            reload();
            Ok(format!("caddy: wrote full Caddyfile {output}"))
        }
        other => anyhow::bail!("unknown caddy.mode `{other}` (off|fragment|full)"),
    }
}

fn proxy_target(cfg: &Config) -> String {
    // Caddy forwards to the HomeOps proxy on the same host.
    let port = cfg.proxy.listen.rsplit(':').next().unwrap_or("80");
    format!("127.0.0.1:{port}")
}

fn write_fragment(path: &Path, body: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    reload();
    Ok(())
}

/// Refuse to overwrite a full Caddyfile we do not own (no ownership marker).
fn write_full(path: &Path, body: &str) -> Result<()> {
    if path.exists() {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        if !existing.contains(CADDY_OWNERSHIP_MARKER) {
            anyhow::bail!(
                "refusing to overwrite {} — it lacks the `{CADDY_OWNERSHIP_MARKER}` marker",
                path.display()
            );
        }
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn reload() {
    let _ = crate::proc::run("systemctl", &["reload", "caddy"]);
}
