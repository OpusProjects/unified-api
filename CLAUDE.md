# CLAUDE.md — Unified API

## What is this

Lightweight infrastructure inventory aggregation and caching middleware, written in Rust.
Ingests data from multiple sources (Device42, VMware, Pure Storage, etc.), enriches and caches
it in-memory, and serves it via a fast REST API for consumers like AWX and AnsibleForms.

## Organization

- **GitHub Org:** [opus-automata](https://github.com/opus-automata)
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
├── main.rs              # Entrypoint: load config, build app state, start Axum
├── api/                 # HTTP handlers (axum routes)
│   ├── mod.rs
│   └── health.rs        # /healthz, /readyz
├── config.rs            # YAML configuration loading (planned)
└── domain/              # Core domain types (planned)
```

## Architecture

Hexagonal monolith — single binary, ports & adapters internally.
No external data dependencies (no Redis, no PostgreSQL). All cache in-memory with DashMap.
Configuration from YAML files, secrets from HashiCorp Vault.

## Conventions

- Private by default, `pub` only what needs to be exposed
- Comments only when the WHY is non-obvious
