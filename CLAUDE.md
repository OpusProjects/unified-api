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
├── main.rs              # Entrypoint: load config, start Axum
├── lib.rs               # App builder, shared types (AppState)
├── config.rs            # YAML configuration loading from config/ directory
├── api/                 # HTTP handlers (axum routes)
│   ├── mod.rs
│   ├── health.rs        # /healthz, /readyz
│   └── sources.rs       # /api/v1/sources, /api/v1/sources/{id}/dataset
├── domain/              # Core domain types (pure, no dependencies)
│   ├── dataset.rs       # Dataset, Group, HostVars
│   ├── source.rs        # Source, TtlOverrides
│   ├── cache_entry.rs   # CacheEntry with TTL logic
│   ├── credential.rs    # Credential, CredentialType, ResolvedCredential
│   ├── project.rs       # GitProject
│   └── endpoint.rs      # OutputEndpoint, InventoryScope
├── ports/               # Trait definitions (interfaces)
│   ├── cache.rs         # CachePort trait
│   └── connector.rs     # ConnectorPort trait
├── adapters/            # Concrete implementations
│   ├── memory_cache.rs  # CachePort → DashMap
│   └── process_connector.rs  # ConnectorPort → tokio::process
config/                  # Split YAML config (server, credentials, sources, etc.)
test-connectors/         # Fake connector scripts for testing
tests/                   # Integration tests
```

## Architecture

Hexagonal monolith — single binary, ports & adapters internally.
No external data dependencies (no Redis, no PostgreSQL). All cache in-memory with DashMap.
Configuration from YAML files, secrets from HashiCorp Vault.

## Conventions

- Private by default, `pub` only what needs to be exposed
- Comments only when the WHY is non-obvious
- **Teaching comments are intentional — do not strip them.** Many files carry
  explanatory comments in Spanish (e.g. "Un trait es como una interfaz en Java...")
  that explain Rust concepts to the maintainers. They are a deliberate exception to
  the comment rule above. When refactoring or moving code, keep them with the code
  they explain; when adding new non-obvious Rust constructs, comments in the same
  style are welcome.
