# CLAUDE.md — Unified API

## What is this

Lightweight infrastructure inventory aggregation and caching middleware, written in Rust.
Ingests data from multiple sources (Device42, VMware, Pure Storage, etc.), enriches and caches
it in-memory, and serves it via a fast REST API for consumers like AWX and AnsibleForms.

## Organization

- **GitHub Org:** [OpusProjects](https://github.com/OpusProjects)
- **License:** Apache 2.0
- **Owners:** Fernando Roca and Blai Peidro

## Tech stack

- Rust (edition 2024)
- axum (HTTP framework)
- tokio (async runtime)
- dashmap (concurrent in-memory cache)
- serde + serde_json + serde_yaml (serialization)
- utoipa (OpenAPI/Swagger docs)
- russh (native SSH connector)
- metrics + metrics-exporter-prometheus (`/metrics`)
- subtle (constant-time API key comparison)

## Build & run

```bash
cargo build              # compile
cargo run                # compile + run (reads ./config, or $CONFIG_DIR)
cargo test               # run tests
CONFIG_DIR=/etc/unified-api cargo run   # config from a different directory
```

## Project structure

```
src/
├── main.rs                   # Entrypoint: load config, build app, start Axum
├── lib.rs                    # Module tree + AppBuilder (composition root)
├── state.rs                  # AppState (ports as Arc<dyn Trait> + static config)
├── config.rs                 # YAML configuration loading from config/ directory
├── domain/                   # Core domain types (pure, no dependencies)
│   ├── dataset.rs            # Dataset, Group, HostVars
│   ├── source.rs             # Source, TtlOverrides, ConnectorType
│   ├── cache_entry.rs        # CacheEntry with TTL logic
│   ├── credential.rs         # Credential, CredentialType
│   ├── enricher.rs           # Enricher
│   ├── sync_mode.rs          # SyncMode (replace/merge)
│   ├── project.rs            # GitProject
│   └── endpoint.rs           # OutputEndpoint
├── application/              # Use cases (domain + ports only; shared by HTTP and scheduler)
│   ├── sync.rs               # sync_source, SyncScope, SyncOutcome
│   ├── enrich.rs             # run_enricher, EnrichOutcome
│   ├── projects.rs           # sync_project (git checkout up to date)
│   └── credentials.rs        # resolve_credentials
├── ports/                    # Trait definitions (interfaces)
│   ├── cache.rs              # CachePort (incl. atomic update/merge_or_insert)
│   ├── connector.rs          # ConnectorPort
│   ├── enricher.rs           # EnricherPort
│   ├── git.rs                # GitPort (project checkouts)
│   ├── output.rs             # OutputPort
│   └── secrets.rs            # SecretsPort
├── adapters/                 # Everything that touches the outside world
│   ├── in/                   # Driving adapters: the outside world drives the app
│   │   ├── http/             # axum handlers, auth, routes, OpenAPI spec
│   │   │   ├── routes.rs     # Router assembly (+ optional CORS layer)
│   │   │   ├── openapi.rs    # utoipa ApiDoc (register new handlers here)
│   │   │   ├── sources.rs    # Read endpoints (list/dataset/status)
│   │   │   ├── sync.rs       # POST sync
│   │   │   ├── enrichers.rs  # POST enricher run
│   │   │   ├── hosts.rs      # PUT/DELETE host
│   │   │   ├── endpoints.rs  # Output endpoints
│   │   │   ├── health.rs     # /healthz, /readyz
│   │   │   ├── metrics.rs    # /metrics (Prometheus exporter, installed once)
│   │   │   └── auth.rs       # API key middleware
│   │   └── scheduler/        # interval-based sync/enrich (calls application/)
│   └── out/                  # Driven adapters: the app drives the outside world
│       ├── cache/            # memory.rs: CachePort → DashMap; persistence.rs: disk snapshots
│       ├── connectors/       # process.rs: ConnectorPort → tokio::process; ssh.rs → russh
│       ├── enrichers/        # process.rs: EnricherPort → tokio::process
│       ├── git/              # cli.rs: GitPort → git binary (clone/pull projects)
│       ├── output/           # process.rs: OutputPort → tokio::process
│       └── secrets/          # env.rs: SecretsPort → env/JSON files; mock.rs: test double
config/                       # Split YAML config (server, credentials, sources, etc.)
tests/                        # Integration tests (*.rs), with sample scripts mirroring src/adapters/out/:
└── adapters/
    └── out/                  # sample scripts — stand-ins for the driven adapters
        ├── connectors/       #   sample source connectors (incl. slow.py for timeout tests)
        ├── enrichers/        #   sample enricher scripts
        └── output/           #   sample output transformer scripts
.cargo/audit.toml             # cargo-audit ignore list (documented advisories)
CHANGELOG.md                  # Keep a Changelog; move Unreleased entries on release
```

## Architecture

Hexagonal monolith — single binary, ports & adapters internally.
Dependency direction: `adapters → application → ports → domain` (never the reverse).
Use-case logic lives in `application/` ONLY — HTTP handlers and the scheduler are thin
translators that call it; don't put orchestration logic in either.
No external data dependencies (no Redis, no PostgreSQL). All cache in-memory with DashMap.
Cache mutations must use the atomic `CachePort::update`/`merge_or_insert` operations —
never the get → modify → set pattern (it loses concurrent writes).
Configuration from YAML files; secrets resolved from env vars / JSON files via
`SecretsPort` (a Vault adapter is roadmap, not built).

## Runtime behavior worth knowing

- **Execution timeouts:** every connector/enricher/output run is bounded by
  `timeout_seconds` (default 300); a hung script fails the run instead of blocking
  its scheduler task or HTTP request.
- **Metrics:** `GET /metrics` (Prometheus, public like the health probes) — sync,
  enrich and endpoint counters + duration histograms. The recorder is a process
  global installed once via `OnceLock`, so tests building many apps share it.
- **Read path is shared, not copied:** `CacheEntry` holds its dataset (and
  host timestamps) behind `Arc`; reads bump a refcount, writers go through
  `Arc::make_mut` (copy-on-write). The entry also caches its serialized JSON +
  ETag (invalidated by every mutator), so plain `/dataset` responses reuse one
  buffer and support `If-None-Match` → `304`.
- **Responses gzip** when the client sends `Accept-Encoding: gzip`
  (tower-http `CompressionLayer`).
- **Snapshots skip when idle:** `CachePort::generation()` (bumped on every
  write) lets the persistence task skip disk writes when nothing changed.
- **CORS is off by default:** opt in with `server.cors_allowed_origins` (`["*"]`
  = any). No configured origins = no CORS layer at all.
- **Auth:** optional static key (`UNIFIED_API_KEY`); constant-time compare;
  `/healthz`, `/readyz`, `/metrics` and Swagger stay public.
- **OpenAPI version** comes from `CARGO_PKG_VERSION` — bump only `Cargo.toml`.

## Releasing a new version

`main` is protected: it takes no direct pushes and no force pushes, and every
change needs a PR whose `test`, `audit` and `build-image` checks pass. That
includes the release commit itself — there is no admin bypass.

### 1. Choose the number

Semantic Versioning, and the project is pre-1.0:

- **PATCH** (`0.6.1`) — bug fixes only. Nothing added.
- **MINOR** (`0.7.0`) — anything added (a route, a config key, a response
  field), *and* anything breaking, since 0.x puts breaking changes in MINOR.
- New functionality is never a PATCH, however small the diff.

Mark breaking entries in the CHANGELOG as
`**Breaking (who it affects):**` so a reader can see the risk without a diff.

### 2. Bump, on a branch

```bash
git checkout main && git pull --ff-only
git checkout -b release/x.y.z
```

Four edits, all required:

1. `Cargo.toml` — `version = "x.y.z"` (source of truth; the OpenAPI spec
   version comes from `CARGO_PKG_VERSION`)
2. `Cargo.lock` — `[[package]] name = "unified-api" version = "x.y.z"`
3. `CHANGELOG.md` — rename `## [Unreleased]` to `## [x.y.z] - YYYY-MM-DD` and
   leave a fresh empty `## [Unreleased]` above it
4. `CHANGELOG.md` link refs at the bottom — repoint `[Unreleased]` at
   `vx.y.z...HEAD` and add `[x.y.z]: …/compare/v<previous>...vx.y.z`

### 3. Check before pushing

```bash
cargo build                        # must compile as the new version
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git diff --stat Cargo.lock         # exactly 1 line — no dependency drift
```

Then dry-run the release notes with the same `awk` the workflow uses, because
a malformed heading silently degrades them to "See CHANGELOG.md for details":

```bash
awk -v s="## [x.y.z]" 'substr($0,1,length(s))==s{f=1;next} f && substr($0,1,4)=="## ["{exit} f' CHANGELOG.md
```

### 4. PR, merge, then tag

```bash
git commit -am "Release x.y.z"     # no type prefix; releases are the exception
git push -u origin release/x.y.z
gh pr create --base main --title "Release x.y.z"
# wait for test + audit + build-image
gh pr merge --squash --delete-branch --subject "Release x.y.z"

git checkout main && git pull --ff-only   # MUST pull: squashing made a new commit
git tag vx.y.z && git push origin vx.y.z
```

**Tag the squashed commit on `main`, not the branch commit.** Squash-merging
creates a different commit, so tagging before pulling points the tag at
something that is not on `main`. Tags are not covered by branch protection, so
the tag push itself needs no PR.

### What CI does with the tag

`.github/workflows/build.yaml` (workflow name "unified-api CI"):

- publishes `ghcr.io/opusprojects/unified-api` as `x.y.z`, `<sha>` and `latest`
- creates the GitHub Release with notes extracted from the CHANGELOG section

**Only tags publish.** PRs and pushes to `main` build the image and discard it,
so `latest` always means the newest release rather than the tip of `main`.

Do not create GitHub releases by hand — the workflow does it from the tag, and
a manual one is authored by a person rather than `github-actions[bot]`, which
is visible forever in the API and cannot be corrected without deleting and
recreating the release (losing its original date).

## Conventions

- Private by default, `pub` only what needs to be exposed
- Comments only when the WHY is non-obvious
- **All code comments must be written in English** — no exceptions, including
  teaching comments and test comments
- **Teaching comments are intentional — do not strip them.** Many files carry
  explanatory comments (e.g. "A trait is like an interface in Java...") that teach
  Rust concepts to the maintainers. They are a deliberate exception to the comment
  rule above. When refactoring or moving code, keep them with the code they
  explain; when adding new non-obvious Rust constructs, comments in the same style
  are welcome. They are written in English.
