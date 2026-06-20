//! `homeops serve`: the long-running process.
//!
//! Runs two HTTP listeners:
//!
//! * the **proxy** (e.g. `:80`), which routes by `Host` header to app containers
//!   on localhost, and forwards the admin domain to the admin listener;
//! * the **admin UI** (e.g. `127.0.0.1:9090`), which shows status and exposes a
//!   few operational actions (deploy now, backup now).

use crate::config::{AdminConfig, Config, Paths};
use crate::state::{self, AppState};
use crate::{backup, docker, reconcile};
use anyhow::{Context, Result};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

type ResBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;
type HttpClient = Client<HttpConnector, Incoming>;

struct Ctx {
    config: Config,
    paths: Paths,
    client: HttpClient,
    /// Lowercased admin domain, if routed through the proxy.
    admin_domain: Option<String>,
    /// Local address the admin listener is bound to.
    admin_upstream: String,
}

/// Entry point for `homeops serve`.
pub async fn serve(config: Config, paths: Paths) -> Result<()> {
    let proxy_addr: SocketAddr = config
        .proxy
        .listen
        .parse()
        .with_context(|| format!("invalid proxy.listen `{}`", config.proxy.listen))?;
    let admin_addr: SocketAddr = config
        .admin
        .bind
        .parse()
        .with_context(|| format!("invalid admin.bind `{}`", config.admin.bind))?;

    let admin_upstream = format!("127.0.0.1:{}", admin_addr.port());
    let admin_domain = config.proxy.admin_domain.as_ref().map(|d| d.to_lowercase());

    let ctx = Arc::new(Ctx {
        config,
        paths,
        client: Client::builder(TokioExecutor::new()).build_http(),
        admin_domain,
        admin_upstream,
    });

    tracing::info!("proxy listening on {proxy_addr}");
    tracing::info!("admin listening on {admin_addr}");

    let admin_ctx = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_listener(admin_ctx, admin_addr, true).await {
            tracing::error!("admin listener stopped: {e:#}");
        }
    });

    run_listener(ctx, proxy_addr, false).await
}

async fn run_listener(ctx: Arc<Ctx>, addr: SocketAddr, is_admin: bool) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    loop {
        let (stream, peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let ctx = ctx.clone();
        let peer_ip = peer.ip().to_string();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                let peer_ip = peer_ip.clone();
                async move {
                    if is_admin {
                        handle_admin(req, ctx).await
                    } else {
                        handle_proxy(req, ctx, peer_ip).await
                    }
                }
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                tracing::debug!("connection error: {e}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

fn normalize_host(req: &Request<Incoming>) -> Option<String> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())?;
    let host = host.split(':').next().unwrap_or(host);
    Some(host.to_ascii_lowercase())
}

/// Find the upstream port for a host by matching app domains and reading state.
fn resolve_app_port(ctx: &Ctx, host: &str) -> Option<u16> {
    for (name, app) in &ctx.config.apps {
        if app.domains.iter().any(|d| d.eq_ignore_ascii_case(host)) {
            return AppState::load(&ctx.paths, name)
                .ok()
                .and_then(|s| s.current_port);
        }
    }
    None
}

async fn handle_proxy(
    mut req: Request<Incoming>,
    ctx: Arc<Ctx>,
    peer_ip: String,
) -> Result<Response<ResBody>, Infallible> {
    let Some(host) = normalize_host(&req) else {
        return Ok(status(StatusCode::BAD_REQUEST, "missing Host header"));
    };

    // Admin domain → admin listener.
    let upstream = if ctx.admin_domain.as_deref() == Some(host.as_str()) {
        ctx.admin_upstream.clone()
    } else {
        match resolve_app_port(&ctx, &host) {
            Some(port) => format!("127.0.0.1:{port}"),
            None => {
                return Ok(status(
                    StatusCode::NOT_FOUND,
                    &format!("no app routed for host `{host}`"),
                ))
            }
        }
    };

    // WebSocket / other protocol upgrades → raw bidirectional tunnel.
    if is_upgrade(&req) {
        let client_on = hyper::upgrade::on(&mut req);
        let fwd = forward_request(req, &upstream, &host, &peer_ip);
        let mut res = match ctx.client.request(fwd).await {
            Ok(r) => r,
            Err(e) => return Ok(status(StatusCode::BAD_GATEWAY, &format!("upstream: {e}"))),
        };
        if res.status() == StatusCode::SWITCHING_PROTOCOLS {
            let upstream_on = hyper::upgrade::on(&mut res);
            tokio::spawn(async move {
                if let (Ok(c), Ok(u)) = tokio::join!(client_on, upstream_on) {
                    let mut c = TokioIo::new(c);
                    let mut u = TokioIo::new(u);
                    let _ = tokio::io::copy_bidirectional(&mut c, &mut u).await;
                }
            });
        }
        return Ok(box_response(res));
    }

    let fwd = forward_request(req, &upstream, &host, &peer_ip);
    match ctx.client.request(fwd).await {
        Ok(res) => Ok(box_response(res)),
        Err(e) => Ok(status(
            StatusCode::BAD_GATEWAY,
            &format!("upstream unavailable: {e}"),
        )),
    }
}

fn is_upgrade(req: &Request<Incoming>) -> bool {
    let has_conn_upgrade = req
        .headers()
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false);
    has_conn_upgrade && req.headers().contains_key(header::UPGRADE)
}

/// Rebuild an incoming request pointed at the upstream, adding `X-Forwarded-*`.
fn forward_request(
    req: Request<Incoming>,
    upstream: &str,
    host: &str,
    peer_ip: &str,
) -> Request<Incoming> {
    let (mut parts, body) = req.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .map(|x| x.as_str())
        .unwrap_or("/");
    if let Ok(uri) = format!("http://{upstream}{pq}").parse::<Uri>() {
        parts.uri = uri;
    }
    let h = &mut parts.headers;
    if let Ok(v) = HeaderValue::from_str(peer_ip) {
        h.insert("x-forwarded-for", v);
    }
    h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    if let Ok(v) = HeaderValue::from_str(host) {
        h.insert("x-forwarded-host", v);
    }
    Request::from_parts(parts, body)
}

fn box_response(res: Response<Incoming>) -> Response<ResBody> {
    let (parts, body) = res.into_parts();
    let body = body
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        .boxed();
    Response::from_parts(parts, body)
}

// ---------------------------------------------------------------------------
// Admin UI
// ---------------------------------------------------------------------------

async fn handle_admin(
    req: Request<Incoming>,
    ctx: Arc<Ctx>,
) -> Result<Response<ResBody>, Infallible> {
    if req.uri().path() == "/healthz" {
        return Ok(status(StatusCode::OK, "ok"));
    }
    if !authorized(&req, &ctx.config.admin) {
        let mut resp = status(StatusCode::UNAUTHORIZED, "authentication required");
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"homeops\""),
        );
        return Ok(resp);
    }

    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => Ok(html(dashboard_html(&ctx))),
        (&Method::GET, "/api/status") => Ok(json(&status_payload(&ctx))),
        (&Method::POST, "/api/deploy") => Ok(trigger_deploy(&ctx).await),
        (&Method::POST, "/api/backup") => Ok(trigger_backup(&ctx).await),
        _ => Ok(status(StatusCode::NOT_FOUND, "not found")),
    }
}

/// Authorize via bearer token or HTTP basic auth. If no credentials are
/// configured at all, access is allowed (admin defaults to a localhost bind).
/// Requiring the `Authorization` header — rather than a cookie — keeps mutating
/// actions safe from cross-site request forgery.
fn authorized(req: &Request<Incoming>, admin: &AdminConfig) -> bool {
    if admin.auth_token.is_none() && admin.username.is_none() {
        return true;
    }
    let Some(value) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    if let Some(token) = &admin.auth_token {
        if value == format!("Bearer {token}") {
            return true;
        }
    }
    if let (Some(user), Some(pass)) = (&admin.username, &admin.password) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        if value == format!("Basic {encoded}") {
            return true;
        }
    }
    false
}

async fn trigger_deploy(ctx: &Arc<Ctx>) -> Response<ResBody> {
    let cfg = ctx.config.clone();
    let paths = ctx.paths.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _lock = state::ReconcileLock::acquire(&paths)?;
        reconcile::reconcile_all(&cfg, &paths)
    })
    .await;
    match result {
        Ok(Ok(actions)) => {
            let summary: Vec<_> = actions
                .into_iter()
                .map(
                    |(name, action)| serde_json::json!({"app": name, "action": action.to_string()}),
                )
                .collect();
            json(&serde_json::json!({"ok": true, "results": summary}))
        }
        Ok(Err(e)) => json(&serde_json::json!({"ok": false, "error": e.to_string()})),
        Err(e) => json(&serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

async fn trigger_backup(ctx: &Arc<Ctx>) -> Response<ResBody> {
    let cfg = ctx.config.clone();
    let paths = ctx.paths.clone();
    let result = tokio::task::spawn_blocking(move || backup::backup_all(&cfg, &paths)).await;
    match result {
        Ok(Ok(files)) => {
            let files: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
            json(&serde_json::json!({"ok": true, "files": files}))
        }
        Ok(Err(e)) => json(&serde_json::json!({"ok": false, "error": e.to_string()})),
        Err(e) => json(&serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

fn status_payload(ctx: &Ctx) -> serde_json::Value {
    let apps: Vec<_> = ctx
        .config
        .apps
        .iter()
        .map(|(name, app)| {
            let st = AppState::load(&ctx.paths, name).unwrap_or_default();
            serde_json::json!({
                "name": name,
                "domains": app.domains,
                "port": st.current_port,
                "deployed_commit": st.last_deployed_commit,
                "image": st.last_image,
                "last_success_at": st.last_success_at,
                "last_error": st.last_error,
                "running": docker::is_running(&docker::container_name(name)),
                "events": state::recent_events(&ctx.paths, name, 5),
            })
        })
        .collect();
    serde_json::json!({
        "server": ctx.config.server.name,
        "apps": apps,
        "databases": {
            "postgres": backup::engine_running(backup::Engine::Postgres),
            "mysql": backup::engine_running(backup::Engine::Mysql),
        }
    })
}

fn dashboard_html(ctx: &Ctx) -> String {
    let mut rows = String::new();
    for (name, app) in &ctx.config.apps {
        let st = AppState::load(&ctx.paths, name).unwrap_or_default();
        let running = docker::is_running(&docker::container_name(name));
        let badge = if running { "🟢 up" } else { "🔴 down" };
        let commit = st
            .last_deployed_commit
            .as_deref()
            .map(|c| c.get(..7).unwrap_or(c).to_string())
            .unwrap_or_else(|| "—".into());
        let events = state::recent_events(&ctx.paths, name, 5).join("<br>");
        rows.push_str(&format!(
            "<tr><td>{name}</td><td>{}</td><td>{}</td><td>{commit}</td><td>{badge}</td>\
             <td>{}</td><td class=\"ev\">{events}</td></tr>",
            html_escape(&app.domains.join(", ")),
            st.current_port.map(|p| p.to_string()).unwrap_or_default(),
            html_escape(st.last_error.as_deref().unwrap_or("")),
        ));
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>HomeOps</title>\
         <style>body{{font-family:system-ui,sans-serif;margin:2rem;color:#222}}\
         table{{border-collapse:collapse;width:100%}}\
         th,td{{border:1px solid #ddd;padding:.5rem;text-align:left;vertical-align:top}}\
         th{{background:#f5f5f5}}.ev{{font-size:.8rem;color:#666}}\
         button{{padding:.5rem 1rem;margin-right:.5rem;cursor:pointer}}</style></head>\
         <body><h1>HomeOps — {server}</h1>\
         <p><button onclick=\"act('/api/deploy')\">Deploy now</button>\
         <button onclick=\"act('/api/backup')\">Backup now</button></p>\
         <table><tr><th>App</th><th>Domains</th><th>Port</th><th>Commit</th>\
         <th>Status</th><th>Last error</th><th>Recent events</th></tr>{rows}</table>\
         <script>async function act(u){{const r=await fetch(u,{{method:'POST'}});\
         alert(JSON.stringify(await r.json(),null,2));location.reload();}}</script>\
         </body></html>",
        server = html_escape(ctx.config.server.name.as_deref().unwrap_or("server")),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

fn full(b: impl Into<Bytes>) -> ResBody {
    Full::new(b.into()).map_err(|never| match never {}).boxed()
}

#[allow(dead_code)]
fn empty() -> ResBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn status(code: StatusCode, msg: &str) -> Response<ResBody> {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(msg.to_string()))
        .unwrap()
}

fn html(body: String) -> Response<ResBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(full(body))
        .unwrap()
}

fn json(value: &serde_json::Value) -> Response<ResBody> {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(body))
        .unwrap()
}
