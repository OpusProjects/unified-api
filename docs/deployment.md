# Deployment

## Container image

CI publishes `ghcr.io/opusprojects/unified-api` on every push to `main` (tagged
`latest` and with the commit SHA) and on every `vX.Y.Z` git tag (tagged `X.Y.Z`).
Production deployments should pin a version tag; `latest` tracks `main`. The image is a two-stage build (Rust builder →
`debian:bookworm-slim`), runs as a non-root `unified` user, ships `python3` for
script connectors, declares a `HEALTHCHECK` (via python3) hitting `/healthz`, and
bakes in the repo's `config/` and the sample scripts under `tests/` as defaults.

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
- **Metrics:** add a `ServiceMonitor` (or scrape annotations) pointing at
  `/metrics` on the HTTP port — the endpoint is public, no API key needed.

## GitOps with ArgoCD

The reference deployment is plain manifests in a git repo watched by an ArgoCD
`Application` with automated sync (`prune` + `selfHeal`), typically as one entry
in an app-of-apps. A working layout:

```
apps/unified-api/manifests/
├── namespace.yaml
├── serviceaccount.yaml
├── configmap.yaml        # config/*.yaml files, mounted at CONFIG_DIR
├── <secret variant>      # see below
├── deployment.yaml       # pinned image tag, probes, non-root
├── service.yaml
├── servicemonitor.yaml   # Prometheus scrape of /metrics
└── ingress.yaml
```

Practical notes:

- **Pin the image** to a release tag (`ghcr.io/opusprojects/unified-api:X.Y.Z`)
  with `imagePullPolicy: IfNotPresent`. Upgrades are then a one-line git commit,
  and ArgoCD rolls the Deployment. `latest` + `Always` gives you unreviewed
  upgrades on every pod restart.
- **Sync waves** help ordering: namespace/serviceaccount first, ConfigMap and
  the secret before the Deployment, so the pod never starts against half the
  config.
- **Config changes**: editing the ConfigMap alone does not restart pods. Either
  bump a pod-template annotation (e.g. a config checksum) in the same commit or
  restart the rollout after sync.

### Secrets: three variants

The app never talks to a secrets backend itself — it only reads env vars
(matching each credential's `env_prefix`/`secret_keys`) and mounted files
(`file_keys`, and `UNIFIED_API_KEY` for the API itself). Any of these produces
the same `Secret` the Deployment references; they differ in where the actual
values live:

1. **Plain Secret** — create it out-of-band
   (`kubectl create secret generic unified-api-secrets --from-literal=api-key=…`)
   and keep it out of git. Simplest, but not GitOps: the secret is invisible to
   ArgoCD, must be documented somewhere, and has to be recreated by hand on
   cluster rebuild. Fine for dev clusters and first bring-up.
2. **Sealed Secrets** — encrypt the Secret with `kubeseal` against the cluster's
   sealed-secrets controller and commit the resulting `SealedSecret` manifest.
   Fully GitOps with no external dependency, but ciphertexts are bound to that
   controller's key pair: lose the key (cluster rebuild without backup) and every
   sealed secret must be re-sealed. Rotation means re-sealing and committing.
3. **External Secrets Operator** — commit an `ExternalSecret` that references
   paths in an external backend (Vault, AWS Secrets Manager, …) via a
   `(Cluster)SecretStore`; ESO materializes and refreshes the `Secret` on an
   interval. Git holds only references, values live and rotate in the backend.
   Costs you running ESO and the backend, and pods start only after ESO has
   synced the Secret at least once.

An ESO example wiring both the API key and the SSH connector credential:

```yaml
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata:
  name: unified-api-secrets
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: vault-unified-api
    kind: ClusterSecretStore
  target:
    name: unified-api-secrets
  data:
    - secretKey: api-key
      remoteRef: {key: unified-api/config, property: api_key}
    - secretKey: ssh-username
      remoteRef: {key: ssh/unified-api-connector, property: username}
    - secretKey: ssh-private-key
      remoteRef: {key: ssh/unified-api-connector, property: private_key}
```

The Deployment consumes it the same way under all three variants: `env` /
`envFrom` for `UNIFIED_API_KEY` and the `secret_keys` values, plus a volume
mounting the SSH private key at the path `file_keys` points to (mode `0400`).
If you use ESO with ArgoCD auto-sync, add `ignoreDifferences` for the
`conversionStrategy`/`decodingStrategy`/`metadataPolicy` defaults the operator
writes back, or the app never reports Synced.

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
