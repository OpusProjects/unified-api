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
- serde + serde_json + serde_yaml_ng (serialization; `_ng` is the maintained
  fork of the deprecated `serde_yaml`)
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
│   ├── sync_health.rs        # SyncHealth + registry (last attempt/success/error)
│   ├── credential.rs         # Credential, CredentialType
│   ├── api_key.rs            # ApiKeyDef, ApiKeyRole (admin / restricted)
│   ├── enricher.rs           # Enricher
│   ├── sync_mode.rs          # SyncMode (replace/merge)
│   ├── static_inventory.rs   # Ansible YAML inventory parsing (native, no process)
│   ├── project.rs            # GitProject
│   ├── endpoint.rs           # OutputEndpoint
│   └── view.rs               # View, ViewMember, Ownership (read-only composite)
├── application/              # Use cases (domain + ports only; shared by HTTP and scheduler)
│   ├── sync.rs               # sync_source, SyncScope, SyncOutcome
│   ├── enrich.rs             # run_enricher, EnrichOutcome
│   ├── projects.rs           # sync_project (git checkout up to date)
│   ├── views.rs              # ViewSnapshot: owner resolution + merged reads
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
│   │   │   ├── error.rs      # ApiError / ErrorBody — the JSON shape of every failure
│   │   │   ├── sources.rs    # Reads: list/dataset/status/groups/hosts
│   │   │   ├── views.rs      # Same reads for a view id (sources.rs dispatches here)
│   │   │   ├── cache.rs      # DELETE a source's cache entry (eviction)
│   │   │   ├── sync.rs       # POST sync
│   │   │   ├── enrichers.rs  # POST enricher run
│   │   │   ├── hosts.rs      # PUT/DELETE host
│   │   │   ├── endpoints.rs  # Output endpoints (GET and POST)
│   │   │   ├── projects.rs   # Git project routes (admin-only)
│   │   │   ├── health.rs     # /healthz, /readyz
│   │   │   ├── metrics.rs    # /metrics (Prometheus exporter, installed once)
│   │   │   └── auth.rs       # API key middleware
│   │   └── scheduler/        # interval-based sync/enrich (calls application/)
│   └── out/                  # Driven adapters: the app drives the outside world
│       ├── cache/            # memory.rs: CachePort → DashMap; persistence.rs: disk snapshots
│       ├── connectors/       # process.rs → tokio::process; ssh.rs → russh;
│       │                     #   static_inventory.rs → Ansible YAML on disk;
│       │                     #   remote.rs → another unified-api (federation)
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
.github/scripts/              # helpers the workflow runs (check-changelog.sh)
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
- **Metrics:** `GET /metrics` (Prometheus; public by default, since a scrape
  config carries no API key — set `server.metrics_require_auth: true` to move
  it behind the key on a shared network, as the exposition labels every source
  id and host count) — sync,
  enrich and endpoint counters + duration histograms, plus per-source gauges
  (`unified_api_source_age_seconds`, `_fresh`, `_cached`, `_hosts`, `_groups`,
  `_ttl_seconds`). The gauges are computed from the cache **on each scrape**,
  not pushed on sync: age grows with the clock, so a pushed value would read
  "0 seconds old" exactly when a source stops syncing. The recorder is a
  process global installed once via `OnceLock`, so tests building many apps
  share it.
- **Views:** `views.yaml` declares read-only composites over several sources.
  A view is served on the SOURCE routes and shares their id space (config
  validation rejects a collision) — that is what makes migrating a consumer a
  one-word change. It holds no cache entry: `application::views::snapshot`
  resolves it from its members on every read, per host, by DECLARED ownership
  (`owns.groups` resolved against another source's dataset) rather than by
  which member happens to have the host cached — see `docs/views.md` for why
  cache-membership routing is wrong. Writes (sync, evict, host PUT/DELETE)
  refuse a view id. A view's `ttl_seconds` is the on-demand refresh GATE, not a
  label, which is why `refresh_hosts` takes a `ttl_override`.
- **Sync health:** every sync goes through `application::sync`, which records
  last attempt / last success / last error / consecutive failures into the
  `SyncHealthRegistry` on `AppState`. `GET /sources` and `/status` expose it as
  `sync_health`. It lives outside the cache on purpose — a source that has
  never synced has no cache entry but still needs somewhere to record why.
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
- **Readiness:** `/readyz` is green when no sources are configured or at least
  one has synced — a pod serving part of the inventory beats one serving
  nothing while it waits on the slowest source. Set
  `server.readyz_require_all_sources: true` where a partial inventory is worse
  than none (a job template that would run against half a datacenter); it then
  waits for every configured source. Either way the body carries
  `sources_synced` and `sources_pending`, so a failing probe names what it is
  waiting for.
- **Auth:** keys are declared in `api_keys.yaml`, each `role: admin` (everything)
  or restricted to explicit `sources`/`endpoints` id lists; secrets come from the
  env var each definition names. The legacy `UNIFIED_API_KEY` still works as one
  extra admin key. Constant-time compare; `/healthz`, `/readyz` and Swagger
  stay public, and so does `/metrics` unless
  `server.metrics_require_auth: true`. No keys configured at all = auth
  disabled, logged loudly at startup — and the flag has no effect in that
  state, since the middleware treats every caller as admin.
- **Errors carry a body:** handlers return `ApiError`, which renders as
  `{"error": "..."}` — never a bare `StatusCode`, which axum sends with an empty
  body. New handlers should use `ApiError::source_forbidden` /
  `source_not_cached` / `source_not_configured` so the wording stays identical
  across routes, and name `body = ErrorBody` in their `#[utoipa::path]`.
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
git checkout -b release/x.y.z          # the release/ prefix matters, see below
```

The branch **must** be named `release/*`. CI fails any other PR that edits an
already-released CHANGELOG section, and cutting a version is precisely that
edit — renaming `## [Unreleased]` to `## [x.y.z]`. The prefix is how the check
knows to stand aside.

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

- **Only `## [Unreleased]` is editable in CHANGELOG.md.** Released sections
  record what shipped. `.github/scripts/check-changelog.sh` runs in CI on every PR and
  fails if they change — the mistake is otherwise silent, because rebasing
  across a release makes git apply your entry into the newly released section
  without a conflict. If that happens, move the entry back up to Unreleased.
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
