# Example HomeOps infra configuration (Nickel).
#
# This file lives at the root of your PRIVATE infra repo and is the single
# source of truth for the server. HomeOps evaluates it with `nickel export`,
# so you can use let-bindings, functions and `| default` to kill repetition.
# Env vars live inline under each app — no separate env files.

# Shared defaults for web apps. `| default` lets each app override a field
# (e.g. `port`) without a merge conflict.
let webapp = fun cfg =>
  { port | default = 3000 } & cfg
in

{
  server.name = "vps-01",

  admin = {
    # Keep the admin UI on localhost and reach it through the proxy's
    # admin_domain, or expose it directly and protect it with a token.
    bind = "127.0.0.1:9090",
    username = "admin",
    password = "change-me",          # plaintext is accepted; the repo is private
    # auth_token = "a-long-random-token",  # alternative to username/password
  },

  reconcile.interval = "2m",

  proxy = {
    mode = "homeops",
    listen = "0.0.0.0:80",
    admin_domain = "ops.example.com",
  },

  backups = {
    target = { "type" = "local", path = "/var/lib/homeops/backups" },
    retention = 7,
    compression = "gzip",
  },

  databases = {
    postgres = { enabled = true, version = "16", admin_user = "admin", admin_password = "postgres-password" },
    # mysql = { enabled = false, version = "8", admin_user = "admin", admin_password = "mysql-password" },
  },

  # Optional Caddy integration. Default is off so HomeOps never takes over the box.
  # mode: "off" (default) | "fragment" (writes a snippet you import) | "full" (owns the file).
  caddy = { mode = "off" },
  # caddy = { mode = "full", output = "/etc/caddy/Caddyfile" },

  apps = {
    site = webapp {
      repo = "git@github.com:you/site.git",
      # `ref` is optional. Omit it to follow the convention: track `master` if the
      # repo has it, otherwise `main`. Pin it for a tag/commit.
      domains = ["example.com", "www.example.com"],
      env = { NODE_ENV = "production" },
      # Optional plaintext HTTP basic auth guarding this app's domains. Setting a
      # `pass` is enough — the username defaults to `admin`. Override with `user`.
      pass = "tomate1234",
      # user = "someone",
    },

    api = webapp {
      repo = "git@github.com:you/api.git",
      ref = "main",
      domains = ["api.example.com"],
      env = { LOG_LEVEL = "info" },
      databases = { postgres = { database = "api", env_var = "DATABASE_URL" } },
      # Persistent host-backed storage: volume name -> mount path. HomeOps backs
      # each with a directory under <workdir>/data/<app>/<name>, re-attached on
      # every restart so data survives rebuilds. Back it up with
      # `homeops backup-volume api` (or `backup all`). A value can also be a
      # record for options: `{ path = "/data", uid = 1000, read_only = true }`.
      volumes = { uploads = "/app/uploads" },
      healthcheck = { path = "/health", timeout_seconds = 10, interval_seconds = 5, retries = 5 },
    },
  },

  # Host services: workloads that need native Docker access (they shell out to
  # `docker`/`docker compose` themselves), which a HomeOps *container* never
  # gets. HomeOps still manages them from git, but renders a systemd unit on the
  # host instead of running a container. Use this sparingly — apps are the norm.
  host_services = {
    # Example: StackBench's panel launches `inspect eval`, which drives
    # `docker compose` to build/run sandbox containers.
    stackbench = {
      repo    = "git@github.com:machado2/stackbench.git",
      # `ref` optional — same master/main convention as apps.
      run_as  = "stackbench",          # system user the unit runs as (created, added to `docker`)
      setup   = "deploy/setup.sh",     # idempotent provisioning; re-run on every source change
      exec    = "deploy/run.sh",       # the unit's ExecStart (must bind 127.0.0.1:<port>)
      port    = 8077,                  # fixed loopback port the proxy/Caddy route to
      domains = ["stackbench.example.com"],
      env     = { STACKBENCH_LLM_BASE_URL = "https://llm.example.com/v1" },
      pass    = "tomate1234",          # optional basic auth, like apps
    },
  },
}
