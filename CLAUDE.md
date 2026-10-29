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
- serde + serde_json + serde-saphyr (serialization; serde-saphyr is a pure-Rust
  YAML reader — we only ever read YAML — chosen over the libyaml ports, whose
  `unsafe-libyaml` core is unmaintained)
- utoipa (OpenAPI/Swagger docs)
- russh (native SSH connector)
- metrics + metrics-exporter-prometheus (`/metrics`)

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
├── config.rs                 # Loader: config.yaml + item files at any depth (SECTIONS)
├── domain/                   # Core domain types (pure; std, serde, the shared unified-auth types)
│   ├── dataset.rs            # Dataset, Group, HostVars
│   ├── source.rs             # SourceDef (file) → Source (resolved with its connector), TtlOverrides
│   ├── host.rs               # HostDef, GroupDef: per-host / per-group TTLs, folded into sources at load
│   ├── connector.rs          # Connector item (how to talk to a system); built-ins remote/static_inventory
│   ├── cache_entry.rs        # CacheEntry with TTL logic
│   ├── sync_health.rs        # SyncHealth + registry (one per kind: sync/enrich/project/snapshot)
│   ├── credential.rs         # Credential, CredentialType (from unified-secrets) + CredentialRules
│   ├── api_key.rs            # ApiKeyDef, ApiKeyRole (admin / restricted)
│   ├── enricher.rs           # Enricher
│   ├── sync_mode.rs          # SyncMode (replace/merge)
│   ├── static_inventory.rs   # Ansible YAML inventory parsing (native, no process)
│   ├── project.rs            # GitProject
│   ├── endpoint.rs           # OutputEndpoint
│   ├── view.rs               # View, ViewMember, Ownership (read-only composite)
│   ├── ldap.rs               # AccessRule, Grant; LdapConfig = unified_auth::LdapConfig<AccessRule>
│   └── topology.rs           # Topology, Peer, PeerSource (federation map)
├── application/              # Use cases (domain + ports only; shared by HTTP and scheduler)
│   ├── config.rs             # reload: swap the RuntimeConfig, diff what changed
│   ├── sync.rs               # sync_source, SyncScope, SyncOutcome
│   ├── enrich.rs             # run_enricher, EnrichOutcome
│   ├── projects.rs           # sync_project (git checkout up to date, bounded by timeout_seconds)
│   ├── scripts.rs            # per-execution script path resolution into project checkouts
│   ├── views.rs              # ViewSnapshot: owner resolution + merged reads
│   ├── login.rs              # LDAP sign-in: re-exports unified-auth's use case and throttle
│   ├── topology.rs           # the federation map, asking peers recursively
│   ├── host_search.rs        # find a host across sources and views from the cache
│   └── credentials.rs        # resolve_credentials
├── ports/                    # Trait definitions (interfaces)
│   ├── cache.rs              # CachePort (incl. atomic update/merge_or_insert)
│   ├── config_store.rs       # ConfigStorePort (read/validate/commit the config dir)
│   ├── connector.rs          # ConnectorPort
│   ├── enricher.rs           # EnricherPort
│   ├── git.rs                # GitPort (project checkouts), re-exported from unified-git
│   ├── output.rs             # OutputPort
│   ├── secrets.rs            # SecretsPort
│   ├── directory.rs          # DirectoryPort (LDAP)
│   ├── peer.rs               # PeerPort (another unified-api, for topology)
│   └── venv.rs               # VenvPort (per-project Python virtualenvs)
├── adapters/                 # Everything that touches the outside world
│   ├── in/                   # Driving adapters: the outside world drives the app
│   │   ├── http/             # axum handlers, auth, routes, OpenAPI spec
│   │   │   ├── routes.rs     # Router assembly (+ optional CORS layer)
│   │   │   ├── openapi.rs    # utoipa ApiDoc (register new handlers here)
│   │   │   ├── error.rs      # ApiError / ErrorBody — the JSON shape of every failure
│   │   │   ├── sources.rs    # Reads: list/dataset/status/groups/hosts
│   │   │   ├── views.rs      # Same reads for a view id (sources.rs dispatches here)
│   │   │   ├── cache.rs      # DELETE a source's cache entry (eviction)
│   │   │   ├── config.rs     # The config directory over HTTP (admin-only, opt-in)
│   │   │   ├── sync.rs       # POST sync
│   │   │   ├── enrichers.rs  # POST enricher run
│   │   │   ├── hosts.rs      # PUT/DELETE host
│   │   │   ├── endpoints.rs  # Output endpoints (GET and POST)
│   │   │   ├── projects.rs   # Git project routes (admin-only)
│   │   │   ├── connectors.rs # GET connectors + their users and health (admin-only)
│   │   │   ├── health.rs     # /healthz, /readyz
│   │   │   ├── metrics.rs    # /metrics (Prometheus exporter, installed once)
│   │   │   ├── login.rs      # login page, sign-in and sign-out routes
│   │   │   ├── ui_assets.rs  # the built web UI at "/" (server.ui.assets_dir)
│   │   │   ├── session.rs    # session cookie name + unified-auth's store, form layer
│   │   │   └── auth.rs       # Permissions, resolve_api_keys + unified-auth's middleware
│   │   └── scheduler/        # interval-based sync/enrich (calls application/)
│   └── out/                  # Driven adapters: the app drives the outside world
│       ├── cache/            # memory.rs: CachePort → DashMap; persistence.rs: disk snapshots
│       ├── config/           # fs.rs: ConfigStorePort → staged validation + atomic commit
│       ├── connectors/       # process.rs → tokio::process; ssh.rs → russh;
│       │                     #   static_inventory.rs → Ansible YAML on disk;
│       │                     #   remote.rs → another unified-api (federation)
│       ├── enrichers/        # process.rs: EnricherPort → tokio::process
│       ├── git/              # cli.rs: re-exports unified-git's CliGit (git binary)
│       ├── directory/        # ldap.rs: re-exports unified-auth's LdapDirectory
│       ├── peers/            # http.rs: PeerPort → reqwest (topology)
│       ├── output/           # process.rs: OutputPort → tokio::process
│       └── secrets/          # re-exports unified-secrets' env, vault, cache, reloadable, mock;
│                             #   build_chain(cfg) — the one place the chain is wired here
config/                       # Example config: config.yaml + item files (sources/, connectors/, ...)
tests/                        # Integration tests (*.rs), with sample scripts mirroring src/adapters/out/:
└── adapters/
    └── out/                  # sample scripts — stand-ins for the driven adapters
        ├── connectors/       #   sample source connectors (incl. slow.py for timeout tests)
        ├── enrichers/        #   sample enricher scripts
        └── output/           #   sample output transformer scripts
.cargo/audit.toml             # cargo-audit ignore list (documented advisories)
release-please-config.json    # release-please: version files, changelog sections
.release-please-manifest.json # the last released version
CHANGELOG.md                  # written by release-please from the squashed PR titles
```

## Architecture

Hexagonal monolith — single binary, ports & adapters internally.
Dependency direction: `adapters → application → ports → domain` (never the reverse).
Use-case logic lives in `application/` ONLY — HTTP handlers and the scheduler are thin
translators that call it; don't put orchestration logic in either.
No external data dependencies (no Redis, no PostgreSQL). All cache in-memory with DashMap.
Cache mutations must use the atomic `CachePort::update`/`merge_or_insert` operations —
never the get → modify → set pattern (it loses concurrent writes).
Configuration is item files (AWX style): `config.yaml` at the root, every other
`*.yaml`/`*.yml` at any depth holds items under a typed top-level key
(`config::SECTIONS`); `name` is the identity (what references, URLs and cache keys
use), `description` the label. Struct `name` fields hold that LABEL — `config::take`
moves `description` into them, which is what keeps API `name` fields unchanged.
References are written without `_id` (`credentials`, `project`, `target`, `sources`);
Rust fields kept their old names via `#[serde(rename)]`, because the API still
answers `target_id` etc. A source item (`SourceDef`) is resolved with its connector
at load into the flat runtime `Source` that everything downstream reads — never
read a connector at sync time. A new item type touches `SECTIONS`, the loader's
`take` calls and `AppConfig`.
Configuration from YAML files, parsed STRICTLY: every config struct carries
`deny_unknown_fields`, so a typo'd key fails startup naming the key instead of
silently applying the default (policy comment in `config.rs`). Free-form data
belongs in the `config:` maps, which stay arbitrary.
The reloadable part of it lives in `RuntimeConfig` behind one pointer on
`AppState`: handlers and tasks call `state.config()` for a snapshot (an Arc
clone, safe to hold across awaits) rather than reading fields directly, which
is what lets a reload be a single swap. Whether a setting can reload is decided
by where it is READ — per request/tick = reloadable; consumed once at
construction (the bound socket, router layers, the snapshot task) =
restart-only, and named in `config::RestartOnlySettings`. The refresh
limits sit in between: the coordinator holds a live semaphore and budget, and
a reload pushes new values onto it (`RefreshCoordinator::resize`).
Secrets resolved via `SecretsPort`: env vars / JSON files (read fresh on every
use), or native Vault (KV v2, token or Kubernetes auth) for a credential with
`vault:` + `vault_path`, where the Vault is itself a `type: vault` credential
behind its own short cache (`cache_ttl_seconds` — also the rotation latency).
`build_chain` routes each credential; the deprecated `secrets:` block in
config.yaml still works.

## Runtime behavior worth knowing

- **Execution timeouts:** every connector/enricher/output run is bounded by
  `timeout_seconds` (default 300); a hung script fails the run instead of blocking
  its scheduler task or HTTP request. Exceeding it KILLS the process (and aborts
  the SSH connector's per-host tasks) rather than abandoning it — otherwise a
  wedged script accumulated a live copy per interval. Scripts must therefore be
  interruptible: nothing cleans up a half-written file for them.
- **Shutdown drains before it snapshots:** SIGTERM first drains HTTP, then
  flips a watch channel every background task selects on; tasks finish their
  in-flight run (never cut mid-write) and main joins their handles — bounded
  by `server.shutdown_grace_seconds` (default 20) — BEFORE writing the final
  cache snapshot. That ordering is the point: no task is still mutating the
  cache while it serializes, and the periodic snapshot task has stopped, so it
  cannot race the final save on the same temp file.
- **The scheduler assumes things fail:** every periodic task (sync, enricher,
  project pull) starts with a deterministic per-id jitter (≤30s, capped at the
  interval) so nothing fires in lockstep (sources on a cron `schedule` skip
  the jitter — their times are deliberate — but back off identically); a failing task backs off by letting
  ticks pass — attempts at 1, 2, 4, then at most 8 intervals, reset on
  success — while `sync_health` carries the streak; and each task body runs
  under a supervisor that counts a panic
  (`unified_api_scheduler_task_panics_total`), logs it, and restarts the body
  instead of letting the task die silently.
- **Boot never blocks on git:** the listener binds and serves before the
  project clones — one unreachable remote used to mean no `/healthz` at all.
  Clones run concurrently in a background task, each bounded by the project's
  `timeout_seconds`; the sync schedulers start after they finish. This works
  because script paths resolve into checkouts PER EXECUTION
  (`application/scripts.rs`, via `AppState::source_for_sync` and the
  `Enrichment` bundle), not once at boot — which also means a script that
  first appears after startup is picked up on the next run, no restart.
- **Metrics:** `GET /metrics` (Prometheus; public by default, since a scrape
  config carries no API key — set `server.metrics_require_auth: true` to move
  it behind the key on a shared network, as the exposition labels every source
  id and host count) — per-route HTTP counters + latency histograms
  (`unified_api_http_*`, labeled by MATCHED route pattern, never the raw URL —
  cardinality), sync,
  enrich and endpoint counters + duration histograms (all histograms export
  real buckets, not summaries, so they aggregate across instances), plus
  per-source gauges
  (`unified_api_source_age_seconds`, `_fresh`, `_cached`, `_hosts`, `_groups`,
  `_ttl_seconds`) and health gauges from the registries: per-source
  `unified_api_source_sync_consecutive_failures` / `_last_attempt_age_seconds`
  / `_last_success_age_seconds`, and `_consecutive_failures` /
  `_last_success_age_seconds` per enricher, per project and for the snapshot
  task (`last_error` stays API-only — an error string as a label value is
  unbounded cardinality). All gauges are computed **on each scrape**,
  not pushed on sync: age grows with the clock, so a pushed value would read
  "0 seconds old" exactly when a source stops syncing. The recorder is a
  process global installed once via `OnceLock`, so tests building many apps
  share it.
- **Configuration is pushable, and reloads live:** with
  `config_api.enabled: true` the config directory is readable and writable
  over `/api/v1/config` (admin-only, reads included). A proposed change is
  STAGED in a temp directory and loaded there before anything moves, so a
  rejected push never touched the real directory; it is accepted or refused
  whole (cross-file references only make sense against the complete set) and
  each file lands via temp+rename. `?reload=true` or `POST /config/reload`
  then swaps `RuntimeConfig`, rebuilds the secrets chain, replaces the API key
  list and bumps a watch generation the scheduler supervisor listens on — it
  stops the old task generation (finishing in-flight gathers, never cut
  mid-write) and spawns a new one, overlapping safely because `SyncCoordinator`
  serialises syncs per source. Two refusals are deliberate and happen BEFORE
  the commit: a reload that would leave the API with no keys (silent auth
  removal) and one naming an api-key env var that is not set. See
  `docs/config-api.md`.

- **Views:** `views` items declare read-only composites over several sources.
  A view is served on the SOURCE routes and shares their id space (config
  validation rejects a collision) — that is what makes migrating a consumer a
  one-word change. It holds no cache entry: `application::views::snapshot`
  resolves it from its members on every read, per host, by DECLARED ownership
  (`owns.groups` resolved against another source's dataset) rather than by
  which member happens to have the host cached — see `docs/views.md` for why
  cache-membership routing is wrong. Writes (sync, evict, host PUT/DELETE)
  refuse a view id. A member with `owns.advertised: true` routes by what its
  SOURCE claims (`GET /sources/{id}/scope` — local config, or fetched per
  sync for remote members with last-known surviving outages); declared
  groups/hosts become the fallback, and no claim + no fallback claims
  NOTHING, never everything. A view's `ttl_seconds` is the on-demand refresh GATE, not a
  label, which is why `refresh_hosts` takes a `ttl_override`.
- **Sync health:** every sync goes through `application::sync`, which records
  last attempt / last success / last error / consecutive failures into the
  `SyncHealthRegistry` on `AppState`. `GET /sources` and `/status` expose it as
  `sync_health`. It lives outside the cache on purpose — a source that has
  never synced has no cache entry but still needs somewhere to record why.
  The same pattern covers the other periodic work, one registry per kind on
  `AppState` so id spaces cannot collide: enricher runs (recorded in
  `application::enrich`, exposed on `GET /enrichers`; a target missing from
  the cache counts as a failure), project pulls (`application::projects`,
  exposed on `GET /projects`; the project registry is created in `main`
  because boot clones run before `AppState` exists, and handed to the builder
  via `AppBuilder::project_health`), and the cache snapshot task (recorded
  under a well-known key in `adapters/out/cache/persistence.rs`).
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
- **Auth:** keys are declared as `api_keys` items, each `role: admin` (everything)
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

## Shared code with unified-gitops

Credential resolution, LDAP sign-in, API keys and browser sessions live in
[auth](https://github.com/OpusProjects/auth), two crates pinned
by tag in `Cargo.toml`: `unified-secrets` (Credential, SecretsPort, the env and
Vault adapters, the cache, `build_chain`, `SecretsConfig`) and `unified-auth`
(DirectoryPort, LdapDirectory, `LdapConfig<R>`, the login use case and
throttle; its `http` module: the API key registry and middleware, the session
store, CSRF and the browser-form layer). The modules that held them here
re-export them, the generic ones as type aliases over this service's
`Permissions`, so the rest of the service names them by their old paths. What
stays here is what only this service has: `CredentialRules` (the load-time
checks, in this service's words), the access rule type and its `grant_for`,
`Permissions`, `resolve_api_keys`, the cookie name (`uapi_session`), the
`ApiError` wording of a refusal, the login page and the Users page.

- **Fix a bug in that code in the auth repository**, release it, then move both
  services to the new tag in a `build:` PR (`cargo update -p unified-secrets
  -p unified-auth`). Never patch a copy here.
- To try a change before it is released, point both dependencies at a local
  checkout with an uncommitted `[patch."https://github.com/OpusProjects/auth"]`
  section (see the auth repository's `docs/development.md`).
- `deny.toml` allows exactly these git sources (auth and git).

Git checkouts (clone, pull, the commit a checkout is at) come the same way from
[git](https://github.com/OpusProjects/git)'s `unified-git` crate: `GitPort`,
`Remote` and `CliGit`, re-exported from `ports::git` and `adapters::out::git::cli`.
`GitProject::remote()` is what a project hands to git; when to pull, the
per-checkout lock and sync health stay in `application/projects.rs`. Move to a
new release with `cargo update -p unified-git` in a `build:` PR; try a change
first with an uncommitted `[patch."https://github.com/OpusProjects/git"]`.

## Releasing a new version

Releases are cut by release-please (`.github/workflows/release.yml`), as
`github-actions[bot]`; the full process is in `docs/releasing.md`. Never bump
the version, edit a released CHANGELOG section, push a tag or create a GitHub
Release by hand.

- **A PR's squashed title is its CHANGELOG line and decides the version.**
  `feat:` → MINOR, `fix:`/`perf:` → PATCH, `!` or a `BREAKING CHANGE:` footer →
  MINOR while pre-1.0 (`bump-minor-pre-major`). `refactor`, `docs`, `test`,
  `build`, `ci`, `chore` are hidden and release nothing on their own. So write
  titles for a release-notes reader: what changes for them, not
  `fix: review findings`.
- **New functionality is never a `fix:`**, however small the diff.
- After each merge into `main`, release-please updates the one open PR
  `chore: release vX.Y.Z` on `release-please--branches--main`. It bumps
  `Cargo.toml`, `Cargo.lock`, the `branch:` tag in
  `config/projects/prj-connectors-infra.yaml` (the line marked
  `x-release-please-version`) and `.release-please-manifest.json`, and writes
  the CHANGELOG section. Wording can be edited on that branch just before
  merging; a wrong title already merged is fixed with `BEGIN_COMMIT_OVERRIDE`
  in that PR's description.
- Its checks: a push made with `GITHUB_TOKEN` starts no workflow, so
  `release.yml` dispatches `build.yaml` on the release branch
  (`workflow_dispatch`). If they are missing:
  `gh workflow run build.yaml --ref release-please--branches--main`.
- **Merging the release PR is the release:** squash-merge it once green, and
  `release.yml` creates the tag `vX.Y.Z`, the GitHub Release (notes = the
  CHANGELOG section) and publishes `ghcr.io/opusprojects/unified-api` as
  `X.Y.Z`, `<sha>` and `latest`. Only releases publish; `build.yaml` builds
  the image on PRs and pushes and throws it away.
- `UI_REF` in `docker/Dockerfile` (the bundled ui) is not bumped by
  release-please: move it in a `build:` PR before the release that ships it.

## Conventions

- **CHANGELOG.md is written by release-please.** Do not add entries in feature
  PRs; the PR title is the entry.
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
