# HomeOps

**A single-server GitOps runner for personal / self-hosted servers.**

> Format the server. Run bootstrap. Get back to your life.

HomeOps is *not* trying to be a full PaaS like Coolify, CapRover, Dokku, Portainer
or Kubernetes. The goal is the opposite: simple, disposable and predictable. Keep
all your configuration in Git and rebuild a personal server from scratch with
minimal effort.

## Philosophy

HomeOps is for people who are done with clusters, giant dashboards, Kubernetes,
complex Compose files, "the UI is the source of truth", a thousand plugins, and
configuration hidden inside an internal database.

The server should be destroyable and rebuildable with ease. The truth lives in
Git. The UI can operate, but it does not configure.

> **The UI operates. Git configures.**

## What it does

1. Keep a private infra repo that declares everything that matters.
2. Declare which apps exist, which Git repos they come from, and which
   branch/tag/commit to track.
3. Declare domains, ports, environment variables and managed databases.
4. Clone each app, build it from a `Dockerfile` at the repo root, and run it as a
   local container.
5. Route traffic to apps by the `Host` header with a built-in HTTP proxy.
6. Optionally manage Postgres and MySQL, with simple logical backup/restore.
7. Install/uninstall its own systemd units and rebuild the whole server from the
   infra repo.

## What it is *not* (in v1)

No `docker-compose`, Kubernetes, Helm, sidecars, workers, per-app cron, managed
Redis/Elasticsearch/MinIO, app-defined infrastructure (volumes and databases are
declared in the infra repo, never by the app), a visual config editor, a
marketplace, a plugin system, multi-server, clustering, or HA.

If you need those, Coolify, Dokku, Kamal, CapRover, Kubernetes or Nomad probably
fits better.

## Mental model

There are two kinds of repository:

- **Infra repo** (private): the server's configuration — a single `homeops.ncl`
  (Nickel), with env vars declared inline. May contain secrets in plaintext if you
  accept that trade-off; the private repo effectively becomes the server's master key.
- **App repos** (public or private): just application code. Each must have a
  `Dockerfile` at the root and must not declare any infrastructure.

### The app contract

To be compatible with HomeOps v1, an app must:

1. Have a `Dockerfile` at the repo root and build with `docker build`.
2. Run as a single web process/container, listening on `0.0.0.0:$PORT`.
3. Take configuration from environment variables.
4. Log to `stdout`/`stderr`.
5. Not depend on persistent state inside the container, *unless* it is declared
   as a `volumes` mount (see [Persistent storage](#persistent-storage)).
6. Use `DATABASE_URL` for Postgres / `MYSQL_URL` for MySQL when needed.
7. Keep uploads and persistent files in a declared `volume` or outside the
   container (S3/R2/etc.).

### Host services (when an app can't be a container)

Some workloads genuinely need **native Docker access** — they shell out to
`docker`/`docker compose` themselves. A HomeOps *container* is never given that
(no socket mount, no `--privileged`, no host networking, no raw docker args), so
such a workload cannot be an app. The motivating case is
[StackBench](https://github.com/machado2/stackbench), whose web panel launches
`inspect eval`, which drives `docker compose` to build and run sandbox
containers.

For these, declare a **`host_service`** instead of an app. HomeOps still owns it
from git, but renders a **systemd unit on the host** rather than running a
container:

```nickel
host_services = {
  stackbench = {
    repo    = "git@github.com:machado2/stackbench.git",
    # `ref` optional — same master/main convention as apps.
    run_as  = "stackbench",          # system user the unit runs as (created, in the `docker` group)
    setup   = "deploy/setup.sh",     # idempotent provisioning, re-run on every source change
    exec    = "deploy/run.sh",       # the unit's ExecStart
    port    = 8077,                  # fixed loopback port the service binds
    domains = ["stackbench.example.com"],
    env     = { STACKBENCH_LLM_BASE_URL = "https://llm.example.com/v1" },
    pass    = "tomate1234",          # optional basic auth, like apps
  },
}
```

Each reconcile: sync the repo; ensure `run_as` exists and is in the `docker`
group; write a root-only `EnvironmentFile` from `env`; on a source change run
`setup` (as root, with `HOMEOPS_SERVICE_USER=<run_as>` set); render
`/etc/systemd/system/homeops-<name>.service`; then `daemon-reload`, `enable` and
`restart`. The service's domains are routed exactly like an app's (Caddy
generation + the HomeOps proxy), but the proxy upstream is the fixed
`127.0.0.1:<port>` instead of an allocated container port.

**Host-service contract.** The repo must provide an `exec` script that runs the
process in the foreground bound to `127.0.0.1:<port>`, and (optionally) an
idempotent `setup` script. Apps and host services share one name namespace and
one domain namespace (both validated unique).

**Privileges.** Rendering units and provisioning the host need root — the same
root the `homeops-reconcile` service already runs as. Host services do not
broaden reconcile's privileges; they use the root it already has. If you run
reconcile unprivileged (e.g. local testing), the units are written but won't
start, which is the honest outcome.

## Install

On a freshly formatted Linux server with Docker available:

```bash
curl -fsSL https://homeops.example/install.sh | sudo bash      # or grab a release binary
sudo homeops bootstrap --repo git@github.com:you/homeops-infra.git
```

`bootstrap` writes the local bootstrap file, clones the infra repo, installs the
systemd units, provisions databases, builds and runs every app, applies the proxy
and prints final status.

## Commands

| Command | Purpose |
| --- | --- |
| `homeops install` | Install systemd units, directories and timer. |
| `homeops uninstall [--purge]` | Remove what was installed (`--purge` also deletes data). |
| `homeops bootstrap --repo <url>` | Rebuild a server from scratch off the infra repo. |
| `homeops serve` | Run the long-lived web/proxy server (the `homeops-web.service`). |
| `homeops reconcile` | Converge running state to the desired state (the timer runs this). |
| `homeops plan` | Dry-run: show what reconcile would do. |
| `homeops status` | Show current state of all apps. |
| `homeops doctor` | Diagnose the environment (Docker, systemd, repos, DBs…). |
| `homeops backup <postgres\|mysql\|all> [db]` | Create a logical database backup. |
| `homeops restore <postgres\|mysql> <db> --file <dump>` / `restore latest` | Restore a database backup (`latest` is databases-only; always takes a safety backup first). |
| `homeops backup-volume <app> [name]` | Archive an app's volume(s) to the backup target. |
| `homeops restore-volume <app> <name> --file <tar>` | Restore an app volume (safety backup first; stops the app). |
| `homeops volume-prune <app> [--yes]` | List (or with `--yes`, delete) an app's orphaned volume dirs. |

## Configuration

### Local bootstrap (`/etc/homeops/bootstrap.toml`)

The only unavoidable local file — it just says where the infra repo is:

```toml
infra_repo = "git@github.com:you/homeops-infra.git"
infra_ref  = "main"
workdir    = "/var/lib/homeops"
```

### Infra repo (`homeops.ncl`)

Everything else lives in Git, written in [Nickel](https://nickel-lang.org). HomeOps
embeds the Nickel evaluator (the `nickel-lang-core` crate) and reads the config
in-process — no external binary required on the server. See
[`examples/homeops.ncl`](examples/homeops.ncl) for a complete example. In short:

```nickel
# Shared defaults; `| default` lets each app override without a merge conflict.
let webapp = fun cfg => { port | default = 3000 } & cfg in

{
  server.name = "vps-01",

  admin = { bind = "127.0.0.1:9090", username = "admin", password = "change-me" },

  reconcile.interval = "2m",

  proxy = { listen = "0.0.0.0:80", admin_domain = "ops.example.com" },

  databases = {
    postgres = { enabled = true, version = "16", admin_user = "admin", admin_password = "postgres-password" },
  },

  apps = {
    site = webapp {
      repo = "git@github.com:you/site.git",
      # ref omitted → convention: track `master` if it exists, else `main`.
      domains = ["example.com", "www.example.com"],
      env = { NODE_ENV = "production" },   # env vars live inline — no separate files
      pass = "tomate1234",                 # optional basic auth; username defaults to `admin`
    },

    api = webapp {
      repo = "git@github.com:you/api.git",
      ref = "main",
      domains = ["api.example.com"],
      env = { LOG_LEVEL = "info" },
      databases = { postgres = { database = "api", env_var = "DATABASE_URL" } },
      healthcheck = { path = "/health", timeout_seconds = 10, interval_seconds = 5, retries = 5 },
    },
  },
}
```

**Branch convention.** An app's `ref` is optional. Omit it and HomeOps tracks
`master` if the repo has it, otherwise `main`, otherwise it aborts with a clear
error. Set `ref` explicitly to pin a tag or commit.

**Per-app basic auth.** Add `pass: <plaintext>` to an app to put its domains
behind HTTP basic auth at the proxy. The username defaults to `admin`; override
it with `user:`. No-frills and plaintext, like the rest of the config — the
infra repo is private.

### Persistent storage

Apps are stateless by default, but some need to keep files on disk (uploads, a
SQLite file, a search index). Declare them per app with a `volumes` map of
`name → mount path`:

```nickel
api = webapp {
  repo = "git@github.com:you/api.git",
  domains = ["api.example.com"],
  volumes = { uploads = "/app/uploads", data = "/data" },
}
```

- **Host-backed.** Each volume is a plain directory under
  `<workdir>/data/<app>/<name>` (e.g. `/var/lib/homeops/data/api/uploads`),
  bind-mounted into the container. It is visible on the host, survives container
  rebuilds, restarts and rollbacks, and is removed by `uninstall --purge`.
- **The name is the identity.** Changing a volume's *mount path* keeps the data
  (same host directory). *Renaming* the volume points the app at a fresh, empty
  directory; the old one is left in place, never auto-deleted.
- **Permissions.** A freshly created volume directory is private (`0700`) and
  root-owned, so a container running as **root** (the default for most images)
  can write to it and nothing else on the host can. A container that runs as a
  **non-root** user must declare `uid` (see below) so HomeOps chowns the
  directory to it. HomeOps only sets permissions on first creation — tighten or
  adjust an existing directory yourself and it won't be re-stomped.
- **Back it up.** `homeops backup-volume <app>` (or `homeops backup all`, and
  the dashboard "Backup now" button) archives volumes as gzip tarballs into the
  backup target, with retention. Restore with
  `homeops restore-volume <app> <name> --file <tar>` — it takes a safety backup
  and stops the app first, then the next reconcile brings it back up. (Volume
  restore is always explicit; `restore latest` covers databases only.)
- **Prune.** Renaming or removing a volume leaves its old directory in place
  (never auto-deleted); `homeops doctor` flags it as an orphan. Delete orphans
  with `homeops volume-prune <app>` (a dry run by default; add `--yes` to
  actually remove).

For finer control, a volume value can be a record instead of a bare path:

```nickel
volumes = {
  cache  = "/cache",                                  # private 0700 dir, root-owned
  data   = { path = "/data", uid = 1000 },            # owned by uid 1000 (private 0700)
  assets = { path = "/assets", read_only = true },    # mounted read-only (:ro)
}
```

- `uid` chowns a *newly created* directory to that user — set it when the
  container runs as a fixed non-root UID, otherwise it cannot write to the
  root-owned `0700` directory.
- `read_only` mounts the volume `:ro` (handy for assets the app should not
  modify).

> The disposable-server promise still holds, but it shifts: with volumes, *all
> state is either in Git or in a backup target*. Set up a remote backup target if
> the data matters.

## How it works

- **State.** Observed state lives outside Git in `/var/lib/homeops/state/<app>.json`
  (deployed commit, config hash, image, assigned port). It exists so reconcile can
  skip unnecessary rebuilds and so rollback has something to fall back to.
- **Reconcile.** For each app: sync the repo, resolve the commit, hash the
  config (env vars included, since they live inline), and — only if something
  changed — rebuild the image, start a new container, run the healthcheck, and roll
  back to the previous image if it fails. A lock (`/run/homeops/reconcile.lock`)
  prevents concurrent runs.
- **Proxy.** Apps bind to `127.0.0.1` on a port in the `41000–41999` range. The
  built-in proxy routes by normalized `Host` header, sets `X-Forwarded-*`,
  streams bodies and tunnels WebSocket upgrades. TLS is expected to be terminated
  upstream (Caddy/Cloudflare/etc.).
- **Host services.** Workloads needing native Docker access run as systemd units
  on the host (`homeops-<name>.service`) instead of containers. Reconcile syncs
  the repo, runs an idempotent `setup` on source change, renders the unit
  (running as a dedicated user with a root-only `EnvironmentFile`) and
  restarts it; the proxy routes their domains to a fixed loopback port. See
  [Host services](#host-services-when-an-app-cant-be-a-container).
- **Caddy (optional).** `caddy.mode` can be `off` (default), `fragment` (writes a
  snippet you `import`), or `full` (only overwrites a file carrying the
  `# managed-by: homeops` marker). App and host-service domains are both emitted.
- **Databases.** Postgres/MySQL run as managed containers with a single admin
  user; HomeOps creates per-app databases and injects the connection string.

## Building

HomeOps is a single Rust binary. On the target Linux server:

```bash
cargo build --release
# binary at target/release/homeops
```

CI builds and checks the project on Linux on every push (see
[`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

## Roadmap

- **v0.1** core: config, bootstrap, install/uninstall, serve, reconcile timer,
  clone/build/run, Host-based proxy, status, lock, local state.
- **v0.2** safe ops: `plan`, `doctor`, env files, events, healthcheck, rollback,
  simple UI, basic auth.
- **v0.3 / v0.4** managed Postgres / MySQL with per-app databases and backup/restore.
- **v0.5** remote backup targets (SSH, S3-compatible), `restore latest`, retention.
- **v0.6** optional Caddy management with an ownership marker and reload.
- **v0.7** managed per-app volumes (host-backed) with `read_only`/`uid` options,
  tar backup/restore, orphan detection (`doctor`) and `volume-prune`.
- **v0.8** host services: GitOps-managed systemd units for workloads that need
  native Docker access (e.g. StackBench), routed like apps.
- **Later** workers, cron, Compose, Podman, webhooks, config editing via PR,
  multi-server.

## License

MIT — see [LICENSE](LICENSE).
