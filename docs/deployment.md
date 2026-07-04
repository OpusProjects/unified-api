# Deployment

## Container image

CI publishes `ghcr.io/opusprojects/unified-api` on every push to `main` (tagged
`latest` and with the commit SHA) and on every `vX.Y.Z` git tag (tagged `X.Y.Z`).
Production deployments should pin a version tag; `latest` tracks `main`. The image is a two-stage build (Rust builder →
`debian:bookworm-slim`), runs as a non-root `unified` user, ships `python3` for
script connectors, and bakes in the repo's `config/` and `test-connectors/` as
defaults.

```bash
docker run -p 8182:8182 \
  -v $(pwd)/config:/app/config:ro \
  -e UNIFIED_API_KEY=change-me \
  -e SECTION9_USERNAME=admin -e SECTION9_PASSWORD=... \
  ghcr.io/opusprojects/unified-api:latest
```

## CI/CD pipeline

`.github/workflows/build.yaml`, two jobs:

1. **test** — on every push and PR: `cargo fmt --check`, `cargo clippy --all-targets
   -- -D warnings`, `cargo test` (with `Swatinem/rust-cache`)
1. **audit** — scans `Cargo.lock` against the RUSTSEC advisory database for
   known-vulnerable dependency versions; ignored advisories are documented in
   `.cargo/audit.toml`
2. **build-image** — after tests pass: on PRs the image is **built but not
   pushed** (catches Dockerfile breakage before merge); pushes to `main` and
   `v*` tags also publish to GHCR (`sha` + `latest` from `main`, semver from tags)
1. **release** — on `v*` tags only, after the image is published: creates a
   GitHub Release whose notes are that version's section from `CHANGELOG.md`

The CI badge in the README tracks this workflow.

## Kubernetes notes

- **Probes:** liveness → `GET /healthz`; readiness → `GET /readyz` (503 until at
  least one configured source has synced, so pods join the Service only with data)
- **Config:** mount a ConfigMap at a path and point `CONFIG_DIR` at it
- **Secrets:** the app never talks to a secrets backend directly today. Inject
  values as env vars matching each credential's `env_prefix`/`secret_keys` (e.g.
  from a Secret via `envFrom`, populated by External Secrets Operator), or mount
  files and reference them via `secret_file` / `file_keys`. Native HashiCorp Vault
  resolution is on the roadmap — it will slot in as another `SecretsPort` adapter
- **API key:** set `UNIFIED_API_KEY` from a Secret; without it the API is open
- **Replicas:** the cache is per-process and not shared. Multiple replicas each
  sync independently — fine for read-scaling with scheduled syncs, but manual
  `PUT/DELETE /hosts` edits and on-demand syncs only touch the replica that served
  them. Run a single replica if consumers rely on those.

## Scheduling behavior

Background sync tasks start at boot for every source with
`sync_interval_seconds > 0` (tokio `interval`, first tick immediately). Enrichers
with an interval likewise. A failed run logs the error and waits for the next tick —
there is no retry/backoff beyond the interval itself. Every script execution is
bounded by its `timeout_seconds` (default 300), so a hung connector or enricher
cannot wedge its scheduler task. Shutdown is graceful for
in-flight HTTP requests (SIGTERM/Ctrl-C); scheduler tasks stop with the process.

## Observability

Structured logs via `tracing` to stdout; tune with `RUST_LOG` (e.g.
`unified_api=debug`). Every HTTP request is logged at INFO with method, path,
status and latency (a `tower-http` trace layer); set `tower_http=debug` for more
detail. Sync and enrich outcomes are logged with source ids, host counts and
durations.

**Prometheus metrics** are exposed at `GET /metrics` (public, like the health
probes — scrapers don't carry the API key):

| Metric | Labels | Meaning |
|---|---|---|
| `unified_api_sync_total` | `source`, `result` | Sync runs, success vs error |
| `unified_api_sync_duration_seconds` | `source` | Sync duration histogram |
| `unified_api_enrich_total` | `source`, `result` | Enricher runs |
| `unified_api_enrich_duration_seconds` | `source` | Enricher duration histogram |
| `unified_api_endpoint_total` | `endpoint`, `result` | Output endpoint runs |
| `unified_api_endpoint_duration_seconds` | `endpoint` | Endpoint duration histogram |

Timed-out and failed runs count as `result="error"`, so alerting on the error
rate catches hung connectors too.
