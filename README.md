# Unified API

[![CI](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml/badge.svg)](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Container](https://img.shields.io/badge/ghcr.io-OpusProjects%2Funified--api-2496ED?logo=docker&logoColor=white)](https://github.com/OpusProjects/unified-api/pkgs/container/unified-api)

Lightweight infrastructure inventory aggregation and caching middleware, written in Rust.

Unified API ingests inventory data from multiple sources of truth (Device42, VMware,
Pure Storage, ad-hoc scripts, SSH facts...), caches and enriches it in memory, and serves
it through a fast REST API to consumers like AWX and AnsibleForms — so every automation
tool sees the same, fresh view of the infrastructure without hammering the backends.

## Features

- **Pluggable sources** — any executable that prints inventory JSON is a connector;
  a native SSH connector gathers Ansible facts from fleets in parallel
- **In-memory cache with TTLs** — per-dataset, per-host and per-group freshness,
  no Redis or database required
- **Enrichers** — post-process cached data on a schedule or on demand
- **Output endpoints** — transform one or more cached datasets into whatever format
  a consumer needs (e.g. a merged Ansible inventory)
- **Scheduled + on-demand sync** — background interval sync per source, plus
  full/host/group-scoped sync over the API
- **Swagger UI** — interactive OpenAPI docs served at `/swagger-ui/`
- **Single static binary** — axum + tokio, hexagonal architecture, ~3k lines

## Quick start

```bash
cargo run                      # uses ./config; demo sources run against test-connectors/
# open http://localhost:8182/  → redirects to Swagger UI
```

Sync a source and read it back:

```bash
curl -X POST localhost:8182/api/v1/sources/src-section9/sync
curl localhost:8182/api/v1/sources/src-section9/dataset
curl -X POST localhost:8182/api/v1/endpoints/ep-ansible-full   # merged Ansible inventory
```

Or with Docker:

```bash
docker run -p 8182:8182 ghcr.io/opusprojects/unified-api:latest
```

## Documentation

| Document | What it covers |
|---|---|
| [Architecture](docs/architecture.md) | The hexagon: domain / application / ports / adapters, dependency rules, request flows, concurrency model |
| [Configuration](docs/configuration.md) | Every YAML file and field, environment variables, startup validation |
| [REST API](docs/api.md) | Endpoints, authentication, request/response examples |
| [Connectors, enrichers & outputs](docs/connectors.md) | The script contracts: how to write a source connector, an enricher, or an output transformer |
| [Caching & TTLs](docs/caching.md) | The three-level freshness model, sync modes, TTL overrides, atomicity guarantees |
| [Deployment](docs/deployment.md) | Docker image, CI/CD pipeline, Kubernetes notes, secrets injection |

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Project status

Actively developed. The API surface under `/api/v1` is functional but not yet frozen;
HashiCorp Vault secrets resolution and git-cloned connector projects are on the roadmap.

## Authors

- [Fernando Roca](https://github.com/fernandorocagonzalez)
- [Blai Peidro](https://github.com/blaipr)

Part of [OpusProjects](https://github.com/OpusProjects).

## License

[Apache 2.0](LICENSE)
