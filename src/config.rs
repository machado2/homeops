//! Configuration model.
//!
//! There are two layers of configuration:
//!
//! * [`Bootstrap`] — the tiny, unavoidable local file (`/etc/homeops/bootstrap.toml`)
//!   that only tells the server where the infra repo lives.
//! * [`Config`] — the full desired state, read from `homeops.ncl` (Nickel) inside the infra repo.
//!   Git is the source of truth; everything operational comes from here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Default working directory for state, checkouts and backups.
pub const DEFAULT_WORKDIR: &str = "/var/lib/homeops";
/// Default location of the local bootstrap file.
pub const DEFAULT_BOOTSTRAP_PATH: &str = "/etc/homeops/bootstrap.toml";
/// Lowest port HomeOps will allocate for app upstreams.
pub const PORT_RANGE_START: u16 = 41000;
/// Highest port HomeOps will allocate for app upstreams.
pub const PORT_RANGE_END: u16 = 41999;
/// Docker network all managed containers join so they can reach each other by name.
pub const DOCKER_NETWORK: &str = "homeops";
/// Marker that must be present before HomeOps overwrites a full Caddyfile.
pub const CADDY_OWNERSHIP_MARKER: &str = "# managed-by: homeops";

// ---------------------------------------------------------------------------
// Bootstrap (local)
// ---------------------------------------------------------------------------

/// The only configuration that must live on the machine itself.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Bootstrap {
    /// Git URL of the private infra repo.
    pub infra_repo: String,
    /// Branch/tag/commit of the infra repo to track.
    #[serde(default = "default_infra_ref")]
    pub infra_ref: String,
    /// Local working directory.
    #[serde(default = "default_workdir")]
    pub workdir: String,
}

fn default_infra_ref() -> String {
    "main".to_string()
}
fn default_workdir() -> String {
    DEFAULT_WORKDIR.to_string()
}

impl Bootstrap {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading bootstrap file {}", path.display()))?;
        let cfg: Bootstrap =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(path, raw)
            .with_context(|| format!("writing bootstrap file {}", path.display()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Resolved local directory layout
// ---------------------------------------------------------------------------

/// Concrete paths derived from the working directory.
#[derive(Debug, Clone)]
pub struct Paths {
    pub workdir: PathBuf,
    pub infra: PathBuf,
    pub checkouts: PathBuf,
    pub state: PathBuf,
    pub backups: PathBuf,
    pub data: PathBuf,
    pub run: PathBuf,
    pub install_manifest: PathBuf,
}

impl Paths {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        let workdir = workdir.into();
        Paths {
            infra: workdir.join("infra"),
            checkouts: workdir.join("checkouts"),
            state: workdir.join("state"),
            backups: workdir.join("backups"),
            data: workdir.join("data"),
            run: PathBuf::from("/run/homeops"),
            install_manifest: workdir.join("install-manifest.json"),
            workdir,
        }
    }

    /// Path to the Nickel config (`homeops.ncl`) inside the infra checkout.
    pub fn config_file(&self) -> PathBuf {
        self.infra.join("homeops.ncl")
    }

    pub fn checkout(&self, app: &str) -> PathBuf {
        self.checkouts.join(app)
    }

    pub fn state_file(&self, app: &str) -> PathBuf {
        self.state.join(format!("{app}.json"))
    }

    /// Directory holding all of an app's persistent volumes.
    pub fn app_data_dir(&self, app: &str) -> PathBuf {
        self.data.join(app)
    }

    /// Host directory backing a single named volume of an app.
    pub fn volume_dir(&self, app: &str, name: &str) -> PathBuf {
        self.data.join(app).join(name)
    }

    pub fn events_file(&self, app: &str) -> PathBuf {
        self.state.join(format!("{app}.events.log"))
    }

    pub fn lock_file(&self) -> PathBuf {
        self.run.join("reconcile.lock")
    }

    /// Create every directory HomeOps expects to exist.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.workdir,
            &self.infra,
            &self.checkouts,
            &self.state,
            &self.backups,
            &self.backups.join("postgres"),
            &self.backups.join("mysql"),
            &self.data,
            &self.run,
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory {}", dir.display()))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Config (homeops.ncl)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub reconcile: ReconcileConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub backups: BackupConfig,
    #[serde(default)]
    pub databases: DatabasesConfig,
    #[serde(default)]
    pub caddy: CaddyConfig,
    #[serde(default)]
    pub apps: BTreeMap<String, AppConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        // Evaluate the Nickel config in-process (the `nickel-lang-core` evaluator
        // is embedded) and deserialize straight into `Config` — no external
        // `nickel` binary on the server.
        let cfg: Config = nickel_lang_core::deserialize::from_path(path.to_path_buf())
            .map_err(|e| anyhow::anyhow!("evaluating Nickel config {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject configurations that would route the same domain to two places.
    pub fn validate(&self) -> Result<()> {
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let admin_domain = self.proxy.admin_domain.as_ref().map(|d| d.to_lowercase());
        if let Some(d) = &admin_domain {
            seen.insert(d.clone(), "<admin>".to_string());
        }
        for (name, app) in &self.apps {
            validate_app_name(name)?;
            for domain in &app.domains {
                let key = domain.to_lowercase();
                if let Some(prev) = seen.get(&key) {
                    anyhow::bail!("domain `{domain}` is claimed by both `{prev}` and `{name}`");
                }
                seen.insert(key, name.clone());
            }
            validate_volumes(name, app)?;
        }
        Ok(())
    }
}

/// An app name is used verbatim as a host path component (checkouts, state,
/// `<workdir>/data/<app>/…`) and as a Docker container name, and it is the
/// target of destructive operations like `volume-prune`. Reject anything that
/// could escape those paths or confuse Docker.
fn validate_app_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        anyhow::bail!("invalid app name `{name}` (use letters, digits, `-`, `_`)");
    }
    Ok(())
}

/// Reject volume declarations that would be ambiguous or unsafe. Volume names
/// become host path components, and the mount paths are passed to `docker -v`,
/// so both need to be well-formed before reconcile ever runs.
fn validate_volumes(app: &str, cfg: &AppConfig) -> Result<()> {
    let mut mounts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (name, spec) in &cfg.volumes {
        let path = spec.path();
        // The name is used as a directory component under `<workdir>/data/<app>/`.
        let valid_name = !name.is_empty()
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid_name {
            anyhow::bail!(
                "app `{app}`: invalid volume name `{name}` (use letters, digits, `-`, `_`)"
            );
        }
        // A relative path makes docker treat it as a *named* volume, silently
        // diverging from the host-backed model.
        if !Path::new(path).is_absolute() {
            anyhow::bail!("app `{app}`: volume `{name}` mount path `{path}` must be absolute");
        }
        // `:` is docker's `-v host:container[:mode]` separator; a colon in the
        // container path would be misread as a mount option.
        if path.contains(':') {
            anyhow::bail!("app `{app}`: volume `{name}` mount path `{path}` must not contain `:`");
        }
        let normalized = path.trim_end_matches('/');
        if normalized.is_empty() {
            anyhow::bail!("app `{app}`: volume `{name}` cannot mount at `/`");
        }
        for danger in [
            "/etc", "/usr", "/bin", "/sbin", "/lib", "/boot", "/dev", "/proc", "/sys",
        ] {
            if normalized == danger {
                anyhow::bail!("app `{app}`: volume `{name}` refuses to mount over `{danger}`");
            }
        }
        if !mounts.insert(normalized.to_string()) {
            anyhow::bail!("app `{app}`: two volumes mount at the same path `{normalized}`");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ServerConfig {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminConfig {
    /// Address the admin UI listens on, e.g. `127.0.0.1:9090`.
    #[serde(default = "default_admin_bind")]
    pub bind: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Alternative to username/password: a static bearer token.
    #[serde(default)]
    pub auth_token: Option<String>,
}

fn default_admin_bind() -> String {
    "127.0.0.1:9090".to_string()
}

impl Default for AdminConfig {
    fn default() -> Self {
        AdminConfig {
            bind: default_admin_bind(),
            username: None,
            password: None,
            auth_token: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReconcileConfig {
    /// Reconcile interval as a systemd-style duration, e.g. `2m`.
    #[serde(default = "default_interval")]
    pub interval: String,
}

fn default_interval() -> String {
    "2m".to_string()
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        ReconcileConfig {
            interval: default_interval(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyConfig {
    /// `homeops` (built-in proxy) is the only supported mode in v1.
    #[serde(default = "default_proxy_mode")]
    pub mode: String,
    #[serde(default = "default_proxy_listen")]
    pub listen: String,
    /// Domain that routes to the admin UI.
    #[serde(default)]
    pub admin_domain: Option<String>,
    /// Max request body size in bytes (0 = unlimited).
    #[serde(default)]
    pub max_body_bytes: u64,
}

fn default_proxy_mode() -> String {
    "homeops".to_string()
}
fn default_proxy_listen() -> String {
    "0.0.0.0:80".to_string()
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            mode: default_proxy_mode(),
            listen: default_proxy_listen(),
            admin_domain: None,
            max_body_bytes: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BackupConfig {
    #[serde(default)]
    pub target: BackupTarget,
    #[serde(default)]
    pub retention: Option<u32>,
    #[serde(default)]
    pub compression: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackupTarget {
    Local {
        #[serde(default = "default_backup_path")]
        path: String,
    },
    Ssh {
        host: String,
        user: String,
        path: String,
    },
    S3 {
        bucket: String,
        endpoint: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
    },
}

fn default_backup_path() -> String {
    format!("{DEFAULT_WORKDIR}/backups")
}

impl Default for BackupTarget {
    fn default() -> Self {
        BackupTarget::Local {
            path: default_backup_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DatabasesConfig {
    #[serde(default)]
    pub postgres: Option<EngineConfig>,
    #[serde(default)]
    pub mysql: Option<EngineConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub enabled: bool,
    pub version: String,
    #[serde(default = "default_admin_user")]
    pub admin_user: String,
    pub admin_password: String,
}

fn default_admin_user() -> String {
    "admin".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CaddyConfig {
    /// `off` (default), `fragment`, or `full`.
    #[serde(default = "default_caddy_mode")]
    pub mode: String,
    #[serde(default)]
    pub output: Option<String>,
}

fn default_caddy_mode() -> String {
    "off".to_string()
}

impl Default for CaddyConfig {
    fn default() -> Self {
        CaddyConfig {
            mode: default_caddy_mode(),
            output: None,
        }
    }
}

// ---------------------------------------------------------------------------
// App config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    /// Git URL of the app repo.
    pub repo: String,
    /// Branch, tag, or commit. Omit to follow the convention: track `master`
    /// if the repo has it, otherwise `main`, otherwise abort.
    #[serde(default)]
    pub r#ref: Option<String>,
    /// Optional explicit tracking mode: `branch` or `fixed`. Inferred when absent.
    #[serde(default)]
    pub tracking: Option<String>,
    /// Domains routed to this app.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Port the app listens on inside the container.
    #[serde(default = "default_app_port")]
    pub port: u16,
    /// Environment variables passed to the container, declared inline.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Plaintext HTTP basic-auth password protecting this app's domains. When
    /// set, the proxy challenges every request. No-frills: the repo is private,
    /// so the password lives here in the clear.
    #[serde(default)]
    pub pass: Option<String>,
    /// Basic-auth username. Defaults to `admin` when a `pass` is set.
    #[serde(default)]
    pub user: Option<String>,
    /// Managed databases used by this app.
    #[serde(default)]
    pub databases: AppDatabases,
    /// Persistent host-backed storage: volume name → mount spec. The name is
    /// stable identity; HomeOps backs each volume with a host directory under
    /// `<workdir>/data/<app>/<name>` and re-attaches it on every (re)start, so
    /// data survives rebuilds, restarts and rollbacks.
    #[serde(default)]
    pub volumes: BTreeMap<String, VolumeSpec>,
    /// Optional HTTP healthcheck.
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
}

fn default_app_port() -> u16 {
    3000
}

impl AppConfig {
    /// Whether HomeOps should follow new commits on the ref.
    pub fn tracks_branch(&self) -> bool {
        match self.tracking.as_deref() {
            Some("branch") => true,
            Some("fixed") => false,
            _ => {
                // No explicit ref → convention resolves to master/main, both branches.
                let Some(r) = &self.r#ref else {
                    return true;
                };
                // Infer: a 40-char hex string is a commit (fixed); a name that
                // looks like a version tag is fixed; anything else is a branch.
                let is_sha =
                    r.len() >= 7 && r.len() <= 40 && r.chars().all(|c| c.is_ascii_hexdigit());
                let is_tag =
                    r.starts_with('v') && r[1..].chars().next().is_some_and(|c| c.is_ascii_digit());
                !(is_sha || is_tag)
            }
        }
    }

    /// Basic-auth credentials guarding this app's domains, if a password is set.
    /// The username defaults to `admin` by convention.
    pub fn basic_auth(&self) -> Option<(String, String)> {
        let pass = self.pass.as_ref()?;
        let user = self.user.as_deref().unwrap_or("admin");
        Some((user.to_string(), pass.clone()))
    }
}

/// How a single volume mounts. Accepts either a bare mount path
/// (`data = "/data"`) or a record with options
/// (`data = { path = "/data", read_only = true, uid = 1000 }`). The bare-string
/// form is the common case and stays valid forever.
///
/// Deserialization is hand-written rather than `#[serde(untagged)]` because the
/// embedded Nickel deserializer does not support the buffering untagged enums
/// rely on; a `deserialize_any` visitor that accepts a string *or* a map works.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeSpec {
    /// Container mount path.
    pub path: String,
    /// Mount the volume read-only (`:ro`).
    pub read_only: bool,
    /// Own a *newly created* host directory by this UID so a non-root container
    /// can write to it without the world-writable default.
    pub uid: Option<u32>,
}

impl VolumeSpec {
    /// The container mount path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the mount is read-only.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The UID that should own a freshly created host directory, if set.
    pub fn uid(&self) -> Option<u32> {
        self.uid
    }
}

impl<'de> Deserialize<'de> for VolumeSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SpecVisitor;

        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = VolumeSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a mount path string or a { path, read_only, uid } record")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<VolumeSpec, E> {
                Ok(VolumeSpec {
                    path: v.to_string(),
                    read_only: false,
                    uid: None,
                })
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<VolumeSpec, A::Error> {
                let mut path: Option<String> = None;
                let mut read_only: Option<bool> = None;
                let mut uid: Option<u32> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "path" => path = Some(map.next_value()?),
                        "read_only" => read_only = Some(map.next_value()?),
                        "uid" => uid = map.next_value()?,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(VolumeSpec {
                    path: path.ok_or_else(|| serde::de::Error::missing_field("path"))?,
                    read_only: read_only.unwrap_or(false),
                    uid,
                })
            }
        }

        deserializer.deserialize_any(SpecVisitor)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AppDatabases {
    #[serde(default)]
    pub postgres: Option<AppDatabase>,
    #[serde(default)]
    pub mysql: Option<AppDatabase>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppDatabase {
    pub database: String,
    /// Env var the connection string is injected as (e.g. `DATABASE_URL`).
    pub env_var: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Healthcheck {
    pub path: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_hc_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
}

fn default_timeout() -> u64 {
    10
}
fn default_hc_interval() -> u64 {
    5
}
fn default_retries() -> u32 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a single app from Nickel source.
    fn app(src: &str) -> AppConfig {
        nickel_lang_core::deserialize::from_str(src).expect("valid app config")
    }

    /// Parse a whole config from Nickel source.
    fn config(src: &str) -> Config {
        nickel_lang_core::deserialize::from_str(src).expect("valid config")
    }

    #[test]
    fn branch_ref_is_tracked() {
        assert!(app(r#"{ repo = "x", ref = "main" }"#).tracks_branch());
        assert!(app(r#"{ repo = "x", ref = "develop" }"#).tracks_branch());
    }

    #[test]
    fn omitted_ref_tracks_branch_by_convention() {
        // No `ref` → resolved to master/main at sync time, always a branch.
        let a = app(r#"{ repo = "x" }"#);
        assert!(a.r#ref.is_none());
        assert!(a.tracks_branch());
    }

    #[test]
    fn pass_yields_basic_auth_with_admin_default() {
        let a = app(r#"{ repo = "x", pass = "tomate1234" }"#);
        assert_eq!(a.basic_auth(), Some(("admin".into(), "tomate1234".into())));
    }

    #[test]
    fn user_overrides_basic_auth_default() {
        let a = app(r#"{ repo = "x", pass = "s3cr3t", user = "fabio" }"#);
        assert_eq!(a.basic_auth(), Some(("fabio".into(), "s3cr3t".into())));
    }

    #[test]
    fn no_pass_means_no_auth() {
        assert_eq!(app(r#"{ repo = "x" }"#).basic_auth(), None);
    }

    #[test]
    fn commit_and_tag_refs_are_fixed() {
        assert!(!app(r#"{ repo = "x", ref = "a1b2c3d4e5f6" }"#).tracks_branch());
        assert!(!app(r#"{ repo = "x", ref = "v1.2.3" }"#).tracks_branch());
    }

    #[test]
    fn explicit_tracking_overrides_inference() {
        assert!(app(r#"{ repo = "x", ref = "v1.2.3", tracking = "branch" }"#).tracks_branch());
        assert!(!app(r#"{ repo = "x", ref = "main", tracking = "fixed" }"#).tracks_branch());
    }

    #[test]
    fn duplicate_domain_is_rejected() {
        let cfg = config(
            r#"{ apps = {
                   a = { repo = "x", domains = ["dup.example.com"] },
                   b = { repo = "y", domains = ["dup.example.com"] },
                 } }"#,
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn admin_domain_conflict_is_rejected() {
        let cfg = config(
            r#"{ proxy = { admin_domain = "ops.example.com" },
                 apps = { a = { repo = "x", domains = ["ops.example.com"] } } }"#,
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn relative_volume_path_is_rejected() {
        let cfg = config(r#"{ apps = { a = { repo = "x", volumes = { data = "data" } } } }"#);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn duplicate_mount_path_is_rejected() {
        let cfg = config(
            r#"{ apps = { a = { repo = "x", volumes = { one = "/data", two = "/data/" } } } }"#,
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn invalid_volume_name_is_rejected() {
        let cfg =
            config(r#"{ apps = { a = { repo = "x", volumes = { "../escape" = "/data" } } } }"#);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn mounting_over_system_path_is_rejected() {
        let cfg = config(r#"{ apps = { a = { repo = "x", volumes = { etc = "/etc" } } } }"#);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn valid_volumes_are_accepted() {
        let cfg = config(
            r#"{ apps = { a = { repo = "x", volumes = { data = "/data", uploads = "/app/uploads" } } } }"#,
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn bare_string_volume_is_writable_root() {
        let a = app(r#"{ repo = "x", volumes = { data = "/data" } }"#);
        let v = &a.volumes["data"];
        assert_eq!(v.path(), "/data");
        assert!(!v.read_only());
        assert_eq!(v.uid(), None);
    }

    #[test]
    fn record_volume_parses_options() {
        let a = app(
            r#"{ repo = "x", volumes = { data = { path = "/data", read_only = true, uid = 1000 } } }"#,
        );
        let v = &a.volumes["data"];
        assert_eq!(v.path(), "/data");
        assert!(v.read_only());
        assert_eq!(v.uid(), Some(1000));
    }

    #[test]
    fn record_volume_is_validated_too() {
        let cfg =
            config(r#"{ apps = { a = { repo = "x", volumes = { data = { path = "rel" } } } } }"#);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn colon_in_mount_path_is_rejected() {
        let cfg =
            config(r#"{ apps = { a = { repo = "x", volumes = { data = "/data:cached" } } } }"#);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn traversal_app_name_is_rejected() {
        let cfg = config(r#"{ apps = { "../escape" = { repo = "x" } } }"#);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn normal_app_name_is_accepted() {
        let cfg = config(r#"{ apps = { "my-api_2" = { repo = "x" } } }"#);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn unique_domains_are_accepted() {
        let cfg = config(
            r#"{ apps = {
                   a = { repo = "x", domains = ["a.example.com"] },
                   b = { repo = "y", domains = ["b.example.com"] },
                 } }"#,
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn evaluates_full_nickel_config() {
        // End-to-end through the embedded evaluator, exercising the bits most
        // likely to break: the `webapp` helper + `| default`, inline `env`, the
        // internally-tagged `backups.target`, the `ref` field, and an app that
        // omits the optional healthcheck. No external binary involved.
        let src = r#"
            let webapp = fun cfg => { port | default = 3000 } & cfg in
            {
              proxy = { admin_domain = "ops.example.com" },
              backups = { target = { "type" = "local", path = "/var/lib/homeops/backups" }, retention = 7 },
              apps = {
                site = webapp {
                  repo = "git@github.com:you/site.git",
                  ref = "main",
                  domains = ["example.com"],
                  env = { API_KEY = "secret" },
                  pass = "tomate",
                  volumes = { data = "/data", uploads = "/app/uploads" },
                  healthcheck = { path = "/", timeout_seconds = 10, interval_seconds = 5, retries = 5 },
                },
                api = webapp {
                  repo = "git@github.com:you/api.git",
                  domains = ["api.example.com"],
                },
              },
            }
        "#;
        let cfg = config(src);
        cfg.validate().expect("valid config");

        let site = &cfg.apps["site"];
        assert_eq!(site.port, 3000); // from the `| default`
        assert_eq!(site.r#ref.as_deref(), Some("main"));
        assert_eq!(site.env.get("API_KEY").map(String::as_str), Some("secret"));
        assert_eq!(site.basic_auth(), Some(("admin".into(), "tomate".into())));
        assert_eq!(site.volumes.get("data").map(|v| v.path()), Some("/data"));
        assert_eq!(
            site.volumes.get("uploads").map(|v| v.path()),
            Some("/app/uploads")
        );
        assert!(site.healthcheck.is_some());

        let api = &cfg.apps["api"];
        assert!(api.healthcheck.is_none());

        match &cfg.backups.target {
            BackupTarget::Local { path } => assert_eq!(path, "/var/lib/homeops/backups"),
            other => panic!("expected local backup target, got {other:?}"),
        }
    }
}
