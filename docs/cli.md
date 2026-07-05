# Command-line interface (CLI)

Unified API is a single binary with no subcommands — all runtime behaviour is
controlled through environment variables and YAML configuration files. This page
documents how to start, stop and operate the service from the command line.

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

## Environment variables

All behaviour that isn't in the YAML config is controlled through environment
variables. None are mandatory — the defaults are safe for local development.

| Variable | Default | Description |
|---|---|---|
| `CONFIG_DIR` | `config` (relative to CWD) | Path to the directory containing the YAML configuration files |
| `UNIFIED_API_KEY` | *(unset — auth disabled)* | Static API key for `/api/v1/*` routes. Compared in constant time. Health, metrics and Swagger remain public |
| `RUST_LOG` | `info` | Log level filter ([`tracing` / `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax) |

Credentials are resolved from additional env vars whose names come from
`credentials.yaml` — see [configuration](configuration.md) for the
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

| Code | Meaning |
|---|---|
| `0` | Clean shutdown (SIGTERM / Ctrl-C) |
| `1` | Fatal startup error — configuration file missing or invalid, bind failure, etc. The error is logged to stderr |

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
