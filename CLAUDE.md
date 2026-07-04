# CLAUDE.md — Unified API

## What is this

Lightweight infrastructure inventory aggregation and caching middleware, written in Rust.
Ingests data from multiple sources (Device42, VMware, Pure Storage, etc.), enriches and caches
it in-memory, and serves it via a fast REST API for consumers like AWX and AnsibleForms.

## Organization

- **GitHub Org:** [OpusProjects](https://github.com/OpusProjects)
- **License:** Apache 2.0
- **Owners:** Fernando Roca and Blai

## Tech stack

- Rust (edition 2024)
- axum (HTTP framework)
- tokio (async runtime)
- dashmap (concurrent in-memory cache)
- serde + serde_json + serde_yaml (serialization)
- utoipa (OpenAPI/Swagger docs)

## Build & run

```bash
cargo build              # compile
cargo run                # compile + run
cargo test               # run tests
cargo run -- --config config/config.yaml  # run with config (future)
```

## Project structure

```
src/
├── main.rs              # Entrypoint: load config, build app, start Axum
├── lib.rs               # Module tree + AppBuilder (composition root)
├── state.rs             # AppState (ports as Arc<dyn Trait> + static config)
├── config.rs            # YAML configuration loading from config/ directory
├── domain/              # Core domain types (pure, no dependencies)
│   ├── dataset.rs       # Dataset, Group, HostVars
│   ├── source.rs        # Source, TtlOverrides, ConnectorType
│   ├── cache_entry.rs   # CacheEntry with TTL logic
│   ├── credential.rs    # Credential, CredentialType
│   ├── enricher.rs      # Enricher
│   ├── sync_mode.rs     # SyncMode (replace/merge)
│   ├── project.rs       # GitProject
│   └── endpoint.rs      # OutputEndpoint
├── application/         # Use cases (domain + ports only; shared by HTTP and scheduler)
│   ├── sync.rs          # sync_source, SyncScope, SyncOutcome
│   ├── enrich.rs        # run_enricher, EnrichOutcome
│   └── credentials.rs   # resolve_credentials
├── ports/               # Trait definitions (interfaces)
│   ├── cache.rs         # CachePort (incl. atomic update/merge_or_insert)
│   ├── connector.rs     # ConnectorPort
│   ├── enricher.rs      # EnricherPort
│   ├── output.rs        # OutputPort
│   └── secrets.rs       # SecretsPort
├── adapters/            # Everything that touches the outside world
│   ├── http/            # Driving: axum handlers, auth, routes, OpenAPI spec
│   │   ├── routes.rs    # Router assembly
│   │   ├── openapi.rs   # utoipa ApiDoc (register new handlers here)
│   │   ├── sources.rs   # Read endpoints (list/dataset/status)
│   │   ├── sync.rs      # POST sync
│   │   ├── enrichers.rs # POST enricher run
│   │   ├── hosts.rs     # PUT/DELETE host
│   │   ├── endpoints.rs # Output endpoints
│   │   ├── health.rs    # /healthz, /readyz
│   │   └── auth.rs      # API key middleware
│   ├── scheduler.rs     # Driving: interval-based sync/enrich (calls application/)
│   ├── memory_cache.rs  # CachePort → DashMap
│   ├── process_connector.rs  # ConnectorPort → tokio::process
│   ├── ssh_connector.rs # ConnectorPort → russh
│   ├── process_enricher.rs   # EnricherPort → tokio::process
│   ├── process_output.rs     # OutputPort → tokio::process
│   ├── env_secrets.rs   # SecretsPort → env vars / JSON files
│   └── mock_secrets.rs  # SecretsPort test double (AppBuilder default)
config/                  # Split YAML config (server, credentials, sources, etc.)
test-connectors/         # Fake connector scripts for testing
tests/                   # Integration tests
```

## Architecture

Hexagonal monolith — single binary, ports & adapters internally.
Dependency direction: `adapters → application → ports → domain` (never the reverse).
Use-case logic lives in `application/` ONLY — HTTP handlers and the scheduler are thin
translators that call it; don't put orchestration logic in either.
No external data dependencies (no Redis, no PostgreSQL). All cache in-memory with DashMap.
Cache mutations must use the atomic `CachePort::update`/`merge_or_insert` operations —
never the get → modify → set pattern (it loses concurrent writes).
Configuration from YAML files, secrets from HashiCorp Vault.

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
