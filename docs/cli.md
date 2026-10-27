# Command-line interface (CLI)

Unified API is a single binary — all runtime behaviour is controlled through
environment variables and YAML configuration files, plus two flags:
`--check-config` validates configuration without starting anything, and
`--migrate-config` converts a 0.34 configuration directory to item files. This
page documents how to start, stop and operate the service from the command line.

- [Running the binary](#running-the-binary)
- [Validating configuration (`--check-config`)](#validating-configuration---check-config)
- [Converting a 0.34 directory (`--migrate-config`)](#converting-a-034-directory---migrate-config)
- [Environment variables](#environment-variables)
- [Log level tuning](#log-level-tuning)
- [Health checks](#health-checks)
- [Common operations via the API](#common-operations-via-the-api)
- [Graceful shutdown](#graceful-shutdown)
- [Exit codes](#exit-codes)
- [OpenAPI / Swagger UI](#openapi--swagger-ui)

---

## Running the binary

The binary is called `unified-api`. It reads configuration from `./config` by
default and starts the HTTP server.

**From a local build** (project root):

```bash
cargo run                          # compile + run (dev, reads ./config)
cargo run --release                # optimised build
./target/release/unified-api       # run the pre-built binary directly
```

**With Docker:**

```bash
docker run -p 8182:8182 ghcr.io/opusprojects/unified-api:latest
```

**With Docker Compose / Kubernetes:** see [deployment](deployment.md) for volume
mounts, probes, secrets and replica considerations.

---

## Validating configuration (`--check-config`)

The same strict validation startup runs, as a CI check: load the config
directory, report, exit — nothing binds, no scheduler starts, no secrets are
resolved (a runner validating YAML has no reason to hold the deployment's env
vars, so missing credentials are **not** an error here).

```bash
unified-api --check-config ./config        # or CONFIG_DIR=./config unified-api --check-config
# configuration OK (./config): 4 sources, 3 connectors, 1 views, 2 credentials, ...   → exit 0
# configuration INVALID (./config): sources/a.yaml: source 'src-a': unknown field `ttl_second` ...   → exit 1
```

Exit `0` means this directory would start; exit `1` prints every validation
error at once (a typo'd key, a broken cross-reference, an invalid cron
expression) to stderr. Teams keeping `config/` in its own git repository can
run this in that repo's CI, so a typo fails the pull request instead of the
deploy:

```yaml
# e.g. GitHub Actions, using the published image
- run: docker run --rm -v $PWD:/config -e CONFIG_DIR=/config \
         ghcr.io/opusprojects/unified-api:latest --check-config
```

On a directory still in the **0.34 layout** (`sources.yaml` and the other fixed
files, keyed by id), `--check-config` does not fail and does not write: it
converts the directory in a scratch copy, validates THAT, and says so —

```
configuration OK (./config) in the 0.34 layout: startup would convert it (converted
credentials.yaml, sources.yaml, ... into 15 item files; connector 'd42' extracted from:
src-d42-a, src-d42-b; ...); run --migrate-config to do it now
```

— because the question CI asks is whether this version will start on the
directory, and since startup migrates, it will exactly when the conversion
loads. The checkout may well be mounted read-only; nothing is written to it.

---

## Converting a 0.34 directory (`--migrate-config`)

```bash
unified-api --migrate-config ./config     # or the directory in CONFIG_DIR, or ./config
```

Converts a directory in the 0.34 layout to item files in place, prints what it
did, and exits — the same code startup runs on its own (see
[configuration → upgrading](configuration.md#upgrading-from-034)), offered as a
command so a configuration **repository** can be converted once and committed.
A repository and a pod upgraded in place end up with the same files.

```
converted credentials.yaml, endpoints.yaml, enrichers.yaml, projects.yaml, sources.yaml into 15 item files
connector 'd42' extracted from: src-d42-a, src-d42-b
connector 'ssh-gather-facts' extracted from: src-ssh-dc1
source 'src-ssh-dc1': its project_id was dropped — an ssh connector runs nothing from a checkout
migrated ./config; the old files are in ./config/.legacy-config-20261025T185536Z
```

- The old files move to `.legacy-config-<UTC timestamp>/`, hidden so the loader
  skips it. Comments are not carried into the new files (they are not data);
  they are in the backup. Commit the new files and delete the backup when done.
- Nothing moves unless the converted directory loads: a failure prints
  `migration of ./config FAILED, nothing was changed: ...` and exits `1`.
- A directory already in the item-file layout prints `... is already in the
  item-file layout; nothing to do` and exits `0`, so the command is safe to
  run twice.

---

## Environment variables

All behaviour that isn't in the YAML config is controlled through environment
variables. None are mandatory — the defaults are safe for local development.

| Variable | Default | Description |
|---|---|---|
| `CONFIG_DIR` | `config` (relative to CWD) | Path to the configuration directory (`config.yaml` plus item files) |
| `UNIFIED_API_KEY` | *(unset — auth disabled)* | Static API key for `/api/v1/*` routes. Compared in constant time. Health, metrics and Swagger remain public |
| `RUST_LOG` | `info` | Log level filter ([`tracing` / `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax) |

Credentials are resolved from additional env vars whose names come from the
`credentials` items — see [configuration](configuration.md) for the
`env_prefix`/`secret_keys` pattern.

---

## Log level tuning

Structured logs go to stdout via `tracing`. Tune verbosity with `RUST_LOG`:

```bash
RUST_LOG=debug cargo run                      # everything at debug
RUST_LOG=unified_api=debug cargo run          # debug only for this crate
RUST_LOG=unified_api=debug,tower_http=debug cargo run  # + HTTP request detail
RUST_LOG=warn cargo run                       # quiet — warnings and errors only
```

---

## Health checks

Three public endpoints (no API key needed) let you verify the process is running:

```bash
curl localhost:8182/healthz          # liveness — 200 while the process runs
curl localhost:8182/readyz           # readiness — 200 once at least one source has synced (503 otherwise)
curl localhost:8182/metrics          # Prometheus metrics (sync/enrich/endpoint counters + durations)
```

---

## Common operations via the API

Once the server is running, all management happens through the REST API. These
are the most common operations from the CLI — see [api](api.md) for the full
reference.

### Syncing a source

```bash
# Full sync
curl -X POST localhost:8182/api/v1/sources/src-section9/sync

# Scoped sync — single host or group
curl -X POST 'localhost:8182/api/v1/sources/src-section9/sync?host=motoko'
curl -X POST 'localhost:8182/api/v1/sources/src-section9/sync?group=production'
```

### Reading cached data

```bash
# List all sources with freshness info
curl localhost:8182/api/v1/sources

# Full dataset (hostvars + groups) for one source
curl localhost:8182/api/v1/sources/src-section9/dataset

# Per-host freshness status, optionally filtered
curl localhost:8182/api/v1/sources/src-section9/status
curl 'localhost:8182/api/v1/sources/src-section9/status?host=motoko'
curl 'localhost:8182/api/v1/sources/src-section9/status?group=production'
```

### Modifying hosts

```bash
# Upsert a host's vars
curl -X PUT localhost:8182/api/v1/sources/src-section9/hosts/motoko \
  -H 'Content-Type: application/json' \
  -d '{"os": "linux", "role": "gateway"}'

# Remove a host
curl -X DELETE localhost:8182/api/v1/sources/src-section9/hosts/motoko
```

### Running enrichers and output endpoints

```bash
# Run an enricher
curl -X POST localhost:8182/api/v1/enrichers/enrich-section9

# Run an output endpoint (e.g. merged Ansible inventory)
curl -X POST localhost:8182/api/v1/endpoints/ep-ansible-full
```

### Authenticating

When `UNIFIED_API_KEY` is set, pass it on every `/api/v1/*` request:

```bash
curl -H 'X-API-Key: my-secret-key' localhost:8182/api/v1/sources
# or
curl -H 'Authorization: Bearer my-secret-key' localhost:8182/api/v1/sources
```

---

## Graceful shutdown

The process handles `SIGTERM` and `Ctrl-C`: in-flight HTTP requests are allowed
to complete before the server exits. Background scheduler tasks stop with the
process — there is no drain beyond what axum's graceful shutdown provides.

```bash
kill -TERM $(pgrep unified-api)    # graceful stop
# or simply Ctrl-C in the foreground terminal
```

---

## Exit codes

The process exits with one of two codes, so a supervisor can tell a clean shutdown from a failed start.

| Code | Meaning |
|---|---|
| `0` | Clean shutdown (SIGTERM / Ctrl-C), `--check-config` found the configuration valid, or `--migrate-config` converted the directory (or had nothing to do) |
| `1` | Fatal startup error — configuration file missing or invalid, a 0.34 directory whose conversion would not load, bind failure, etc. The error is logged to stderr. Also `--check-config` with an invalid configuration, and a failed `--migrate-config` |

---

## OpenAPI / Swagger UI

The interactive API documentation is served at `/swagger-ui/` (the root `/`
redirects there). The raw OpenAPI spec is at `/api-docs/openapi.json`. The
version in the spec comes from `Cargo.toml`.

```bash
# Open in a browser
xdg-open http://localhost:8182/swagger-ui/

# Fetch the raw spec
curl localhost:8182/api-docs/openapi.json
```

### Large responses in the browser

Two things keep the UI usable against an enterprise inventory, where a single
dataset is megabytes of JSON:

- **Syntax highlighting is off.** Swagger colours a response by wrapping every
  token in its own DOM element; on a 2000-host dataset that is millions of
  elements and the tab locks up long after the server has answered. The body
  is rendered as plain text instead, which paints. Nothing is truncated, and
  the `Download` link is unaffected.
- **`limit` is prefilled with 50** on `GET /sources/{id}/dataset`, so pressing
  *Execute* out of curiosity asks for a page rather than the whole
  datacenter. It is an example, not a server-side default: clear the field in
  the UI (or just omit `limit` from your own client) and you get the raw,
  unpaginated Dataset as before.

For dumping a whole inventory, use `curl` and redirect to a file — a browser
is the wrong tool for 10MB of JSON no matter how it is rendered.
