//! HomeOps — a single-server GitOps runner.
//!
//! Format the server. Run bootstrap. Get back to your life.

mod backup;
mod caddy;
mod config;
mod databases;
mod docker;
mod doctor;
mod git;
mod host_service;
mod proc;
mod reconcile;
mod serve;
mod state;
mod storage;
mod systemd;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{Bootstrap, Config, Paths};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "homeops",
    version,
    about = "Single-server GitOps runner. Git is the source of truth; the UI only operates."
)]
struct Cli {
    /// Path to the local bootstrap file.
    #[arg(long, global = true, default_value = config::DEFAULT_BOOTSTRAP_PATH)]
    bootstrap: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install HomeOps into systemd (units, dirs, timer).
    Install {
        /// Infra repo URL (writes the bootstrap file if given).
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, default_value = "main")]
        r#ref: String,
        #[arg(long, default_value = config::DEFAULT_WORKDIR)]
        workdir: String,
        /// Reconcile timer interval.
        #[arg(long, default_value = "2m")]
        interval: String,
    },
    /// Remove what HomeOps installed. `--purge` also deletes data.
    Uninstall {
        #[arg(long)]
        purge: bool,
        #[arg(long, default_value = config::DEFAULT_WORKDIR)]
        workdir: String,
    },
    /// Rebuild a server from scratch off the infra repo.
    Bootstrap {
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = "main")]
        r#ref: String,
        #[arg(long, default_value = config::DEFAULT_WORKDIR)]
        workdir: String,
        #[arg(long, default_value = "2m")]
        interval: String,
    },
    /// Run the long-lived web/proxy server.
    Serve,
    /// Converge running state to the desired state (oneshot).
    Reconcile,
    /// Show what reconcile would do, without applying.
    Plan,
    /// Show current state of all apps.
    Status,
    /// Diagnose the environment.
    Doctor,
    /// Back up a managed database (or `all`).
    Backup {
        /// `postgres`, `mysql`, or `all`.
        engine: String,
        /// Database name (omit when using `all`).
        database: Option<String>,
    },
    /// Restore a managed database from a dump, or `restore latest`.
    Restore {
        /// `postgres`, `mysql`, or `latest`.
        engine: String,
        database: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Back up an app's persistent volume(s) into the backup target.
    BackupVolume {
        /// App name.
        app: String,
        /// Volume name (omit to back up all of the app's volumes).
        name: Option<String>,
    },
    /// Restore an app's persistent volume from a tarball.
    RestoreVolume {
        /// App name.
        app: String,
        /// Volume name.
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Remove an app's orphaned volume dirs (data no longer declared in config).
    VolumePrune {
        /// App name.
        app: String,
        /// Actually delete (default is a dry run that only lists orphans).
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Install {
            repo,
            r#ref,
            workdir,
            interval,
        } => {
            let paths = Paths::new(&workdir);
            let bootstrap = repo.map(|infra_repo| Bootstrap {
                infra_repo,
                infra_ref: r#ref,
                workdir: workdir.clone(),
            });
            systemd::install(&paths, bootstrap.as_ref(), &cli.bootstrap, &interval)?;
            println!(
                "HomeOps installed. Units enabled: homeops-web.service, homeops-reconcile.timer"
            );
            Ok(())
        }

        Command::Uninstall { purge, workdir } => {
            let paths = Paths::new(&workdir);
            systemd::uninstall(&paths, purge)?;
            println!(
                "HomeOps uninstalled{}.",
                if purge { " (purged data)" } else { "" }
            );
            Ok(())
        }

        Command::Bootstrap {
            repo,
            r#ref,
            workdir,
            interval,
        } => bootstrap_server(repo, r#ref, workdir, interval, &cli.bootstrap),

        Command::Serve => {
            let (bootstrap, paths) = load_local(&cli.bootstrap)?;
            let _ = bootstrap;
            let cfg = Config::load(&paths.config_file())
                .context("loading the infra config (has the infra repo been cloned?)")?;
            serve::serve(cfg, paths).await
        }

        Command::Reconcile => {
            let (bootstrap, paths) = load_local(&cli.bootstrap)?;
            paths.ensure_dirs()?;
            sync_infra(&bootstrap, &paths)?;
            let cfg = Config::load(&paths.config_file())?;
            let _lock = state::ReconcileLock::acquire(&paths)?;
            let results = reconcile::reconcile_all(&cfg, &paths)?;
            for (name, action) in &results {
                println!("{name}: {action}");
            }
            match caddy::apply(&cfg) {
                Ok(msg) => println!("{msg}"),
                Err(e) => eprintln!("caddy: {e:#}"),
            }
            Ok(())
        }

        Command::Plan => {
            let (_bootstrap, paths) = load_local(&cli.bootstrap)?;
            let cfg = Config::load(&paths.config_file())?;
            for (name, entry) in reconcile::plan(&cfg, &paths) {
                println!("{name}:");
                if let Some((old, new)) = &entry.commit_change {
                    println!("  app repo changed: {old} -> {new}");
                } else {
                    println!("  app repo changed: no");
                }
                println!(
                    "  config changed: {}",
                    if entry.config_changed { "yes" } else { "no" }
                );
                println!("  action: {}", entry.action);
                println!();
            }
            Ok(())
        }

        Command::Status => {
            let (_bootstrap, paths) = load_local(&cli.bootstrap)?;
            let cfg = Config::load(&paths.config_file())?;
            print_status(&cfg, &paths);
            Ok(())
        }

        Command::Doctor => {
            let (bootstrap, paths) = load_local(&cli.bootstrap)
                .map(|(b, p)| (Some(b), p))
                .unwrap_or_else(|_| (None, Paths::new(config::DEFAULT_WORKDIR)));
            let cfg = Config::load(&paths.config_file()).ok();
            let checks = doctor::run(bootstrap.as_ref(), cfg.as_ref(), &paths);
            let mut all_ok = true;
            for c in &checks {
                let mark = if c.ok { "ok  " } else { "FAIL" };
                if !c.ok {
                    all_ok = false;
                }
                println!("[{mark}] {:<20} {}", c.name, c.detail);
            }
            if !all_ok {
                std::process::exit(1);
            }
            Ok(())
        }

        Command::Backup { engine, database } => {
            let (_bootstrap, paths) = load_local(&cli.bootstrap)?;
            let cfg = Config::load(&paths.config_file())?;
            if engine == "all" {
                let files = backup::backup_all(&cfg, &paths)?;
                for f in files {
                    println!("backed up: {}", f.display());
                }
            } else {
                let eng = backup::Engine::parse(&engine)?;
                let db = database.context("a database name is required")?;
                let file = backup::backup(&cfg, &paths, eng, &db)?;
                println!("backed up: {}", file.display());
            }
            Ok(())
        }

        Command::Restore {
            engine,
            database,
            file,
        } => {
            let (_bootstrap, paths) = load_local(&cli.bootstrap)?;
            let cfg = Config::load(&paths.config_file())?;
            if engine == "latest" {
                let f = backup::latest(&paths).context("no backups found to restore")?;
                println!("restoring latest: {}", f.display());
                // Infer engine from the path (postgres/ or mysql/).
                let eng = if f.to_string_lossy().contains("mysql") {
                    backup::Engine::Mysql
                } else {
                    backup::Engine::Postgres
                };
                let db = infer_db_name(&f);
                backup::restore(&cfg, &paths, eng, &db, &f)?;
            } else {
                let eng = backup::Engine::parse(&engine)?;
                let db = database.context("a database name is required")?;
                let f = file.context("--file is required (or use `restore latest`)")?;
                backup::restore(&cfg, &paths, eng, &db, &f)?;
            }
            println!("restore complete");
            Ok(())
        }

        Command::BackupVolume { app, name } => {
            let (_bootstrap, paths) = load_local(&cli.bootstrap)?;
            let cfg = Config::load(&paths.config_file())?;
            let files = match name {
                Some(name) => vec![backup::backup_volume(&cfg, &paths, &app, &name)?],
                None => backup::backup_app_volumes(&cfg, &paths, &app)?,
            };
            for f in files {
                println!("backed up: {}", f.display());
            }
            Ok(())
        }

        Command::RestoreVolume { app, name, file } => {
            let (_bootstrap, paths) = load_local(&cli.bootstrap)?;
            let cfg = Config::load(&paths.config_file())?;
            // Hold the reconcile lock so the timer can't bring the app back up
            // mid-restore.
            let _lock = state::ReconcileLock::acquire(&paths)?;
            backup::restore_volume(&cfg, &paths, &app, &name, &file)?;
            println!("restore complete — run `homeops reconcile` (or wait for the timer) to bring `{app}` back up");
            Ok(())
        }

        Command::VolumePrune { app, yes } => {
            let (_bootstrap, paths) = load_local(&cli.bootstrap)?;
            let cfg = Config::load(&paths.config_file())?;
            let app_cfg = cfg
                .apps
                .get(&app)
                .with_context(|| format!("unknown app `{app}`"))?;
            let orphans = storage::prune(&paths, &app, app_cfg, yes)?;
            if orphans.is_empty() {
                println!("no orphaned volume dirs for `{app}`");
            } else {
                for dir in &orphans {
                    let verb = if yes { "removed" } else { "would remove" };
                    println!("{verb}: {}", dir.display());
                }
                if !yes {
                    println!("\nre-run with --yes to delete these directories");
                }
            }
            Ok(())
        }
    }
}

/// `homeops bootstrap`: full rebuild from the infra repo.
fn bootstrap_server(
    repo: String,
    git_ref: String,
    workdir: String,
    interval: String,
    bootstrap_path: &std::path::Path,
) -> Result<()> {
    let paths = Paths::new(&workdir);
    let bootstrap = Bootstrap {
        infra_repo: repo,
        infra_ref: git_ref,
        workdir: workdir.clone(),
    };
    println!("==> writing bootstrap file");
    bootstrap.save(bootstrap_path)?;
    paths.ensure_dirs()?;

    println!("==> cloning infra repo");
    sync_infra(&bootstrap, &paths)?;

    let cfg = Config::load(&paths.config_file())?;

    println!("==> installing systemd units");
    systemd::install(&paths, Some(&bootstrap), bootstrap_path, &interval)?;

    println!("==> reconciling apps");
    {
        let _lock = state::ReconcileLock::acquire(&paths)?;
        let results = reconcile::reconcile_all(&cfg, &paths)?;
        for (name, action) in &results {
            println!("  {name}: {action}");
        }
    }
    if let Err(e) = caddy::apply(&cfg) {
        eprintln!("caddy: {e:#}");
    }

    println!("\n==> done. Current status:");
    print_status(&cfg, &paths);
    Ok(())
}

fn load_local(bootstrap_path: &std::path::Path) -> Result<(Bootstrap, Paths)> {
    let bootstrap = Bootstrap::load(bootstrap_path).with_context(|| {
        format!(
            "reading {} — run `homeops bootstrap --repo ...` first",
            bootstrap_path.display()
        )
    })?;
    let paths = Paths::new(&bootstrap.workdir);
    Ok((bootstrap, paths))
}

/// Pull (or clone) the infra repo into the local infra directory.
fn sync_infra(bootstrap: &Bootstrap, paths: &Paths) -> Result<()> {
    git::sync(
        &bootstrap.infra_repo,
        &bootstrap.infra_ref,
        &paths.infra,
        true,
    )
    .context("syncing infra repo")
}

fn print_status(cfg: &Config, paths: &Paths) {
    if let Some(name) = &cfg.server.name {
        println!("server: {name}");
    }
    for (name, app) in &cfg.apps {
        let st = state::AppState::load(paths, name).unwrap_or_default();
        let running = docker::is_running(&docker::container_name(name));
        println!("\n{name}");
        println!("  domains:   {}", app.domains.join(", "));
        if !app.volumes.is_empty() {
            let vols: Vec<String> = app
                .volumes
                .iter()
                .map(|(n, s)| {
                    let ro = if s.read_only() { " (ro)" } else { "" };
                    format!("{n}→{}{ro}", s.path())
                })
                .collect();
            println!("  volumes:   {}", vols.join(", "));
        }
        println!(
            "  port:      {}",
            st.current_port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "—".into())
        );
        println!(
            "  deployed:  {}",
            st.last_deployed_commit
                .as_deref()
                .map(|c| c.get(..7).unwrap_or(c).to_string())
                .unwrap_or_else(|| "—".into())
        );
        println!("  running:   {}", if running { "yes" } else { "no" });
        if let Some(at) = &st.last_success_at {
            println!("  last ok:   {at}");
        }
        if let Some(err) = &st.last_error {
            println!("  last err:  {err}");
        }
    }
}

fn infer_db_name(path: &std::path::Path) -> String {
    // Files are named `<db>-<timestamp>.sql.gz`.
    path.file_name()
        .and_then(|f| f.to_str())
        .and_then(|f| f.rsplit_once('-').map(|x| x.0))
        .unwrap_or("postgres")
        .to_string()
}
