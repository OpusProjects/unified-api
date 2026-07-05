# Unified API

[![CI](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml/badge.svg)](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Container](https://img.shields.io/badge/ghcr.io-OpusProjects%2Funified--api-2496ED?logo=docker&logoColor=white)](https://github.com/OpusProjects/unified-api/pkgs/container/unified-api)

Lightweight infrastructure inventory aggregation and caching middleware, written in Rust.

Unified API ingests inventory data from sources of truth like Device42, VMware,
Pure Storage, ad-hoc scripts or SSH facts, caches and enriches it in memory, and
serves it through a fast REST API to consumers like AWX and AnsibleForms — so every
automation tool works from the same fresh, consistent view of the infrastructure
without each run hammering the upstream systems of record behind it.

---

## ✨ Features

- **Pluggable sources**: any executable that prints inventory JSON is a connector
- **SSH connector**: gathers Ansible facts from whole fleets in parallel
- **In-memory cache with TTLs**: per-dataset, per-host and per-group freshness, no database
- **Enrichers**: post-process cached data on a schedule or on demand
- **Output endpoints**: turn cached datasets into the format each consumer needs
- **Scheduled + on-demand sync**: interval sync per source, plus scoped sync over the API
- **Swagger UI**: interactive OpenAPI docs served at `/swagger-ui/`
- **Single static binary**: axum + tokio, hexagonal architecture, ~3k lines

---

## 🚀 Quick start

```bash
cargo run                      # uses ./config; demo sources run the sample scripts under tests/
# open http://localhost:8182/  -> redirects to Swagger UI
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

---

## 📚 Documentation

| Document | What it covers |
|---|---|
| [Architecture](docs/architecture.md) | The four layers, the dependency rule, request flows and the concurrency model |
| [Configuration](docs/configuration.md) | Every YAML file field by field, environment variables and startup validation |
| [REST API](docs/api.md) | All routes with authentication, status code semantics and curl examples |
| [Connectors](docs/connectors.md) | The script contracts for source connectors, enrichers and output transformers |
| [Caching & TTLs](docs/caching.md) | The three-level freshness model, sync modes, TTL overrides and atomicity rules |
| [Deployment](docs/deployment.md) | Container image, CI/CD jobs, Kubernetes probes, secrets and scheduling notes |

---

## 🤝 Contributing

Contributions are welcome: [CONTRIBUTING.md](CONTRIBUTING.md) covers the PR workflow, commit style, CI gates and architecture rules.

Security issues: see [SECURITY.md](SECURITY.md) for private reporting.

---

## 👥 Authors

- [Fernando Roca](https://github.com/fernandorocagonzalez)
- [Blai Peidro](https://github.com/blaipr)

Part of [OpusProjects](https://github.com/OpusProjects).

---

## ⚖️ License

[Apache 2.0](LICENSE)
