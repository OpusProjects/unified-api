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
│   └── credentials.rs        # resolve_credentials
├── ports/                    # Trait definitions (interfaces)
│   ├── cache.rs              # CachePort (incl. atomic update/merge_or_insert)
│   ├── connector.rs          # ConnectorPort
│   ├── enricher.rs           # EnricherPort
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
│       ├── cache/            # memory.rs: CachePort → DashMap
│       ├── connectors/       # process.rs: ConnectorPort → tokio::process; ssh.rs → russh
│       ├── enrichers/        # process.rs: EnricherPort → tokio::process
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
- **CORS is off by default:** opt in with `server.cors_allowed_origins` (`["*"]`
  = any). No configured origins = no CORS layer at all.
- **Auth:** optional static key (`UNIFIED_API_KEY`); constant-time compare;
  `/healthz`, `/readyz`, `/metrics` and Swagger stay public.
- **OpenAPI version** comes from `CARGO_PKG_VERSION` — bump only `Cargo.toml`.

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
