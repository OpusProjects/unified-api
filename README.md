# Unified API

[![unified-api CI](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml/badge.svg)](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Container](https://img.shields.io/badge/ghcr.io-OpusProjects%2Funified--api-2496ED?logo=docker&logoColor=white)](https://github.com/OpusProjects/unified-api/pkgs/container/unified-api)

Lightweight infrastructure inventory aggregation and caching middleware, written in Rust.

Unified API ingests inventory from sources of truth like Device42, VMware,
Pure Storage, ad-hoc scripts or SSH facts, caches and enriches it in memory,
and serves it over a fast REST API.

Consumers like AWX and AnsibleForms query that cache, never the sources. A
hundred job runs cost Device42, VMware or Pure Storage exactly what one does,
and every one of them sees the same inventory.

---

## ✨ Features

- **Pluggable sources**: any executable that prints inventory JSON is a connector
- **SSH connector**: gathers Ansible facts from whole fleets in parallel
- **In-memory cache with TTLs**: per-dataset, per-host and per-group freshness, no database
- **Gzip responses**: inventory JSON compresses ~10× for `Accept-Encoding: gzip` clients
- **Enrichers**: post-process cached data on a schedule or on demand
- **Output endpoints**: turn cached datasets into the format each consumer needs
- **Federation**: one instance per datacenter, one central aggregating them, real ages intact
- **Views**: one id over several sources, routing each host to the member that owns it
- **Scheduled + on-demand sync**: interval sync per source, plus scoped sync over the API
- **Refresh**: a read can bring the hosts it names up to date, bounded by the source's TTL
- **Swagger UI**: interactive OpenAPI docs served at `/swagger-ui/`
- **Single static binary**: axum + tokio, hexagonal architecture, ~3k lines

---

## 📚 Documentation

| Document | What it covers |
|---|---|
| [API](docs/api.md) | All routes with authentication, status code semantics and curl examples |
| [Architecture](docs/architecture.md) | The four layers, the dependency rule, request flows and the concurrency model |
| [Caching & TTLs](docs/caching.md) | The three-level freshness model, sync modes, TTL overrides and atomicity rules |
| [CLI](docs/cli.md) | Environment variables, log tuning, health checks, common curl operations and shutdown |
| [Configuration](docs/configuration.md) | Every YAML file field by field, environment variables and startup validation |
| [Connectors](docs/connectors.md) | The script contracts for source connectors, enrichers and output transformers |
| [Deployment](docs/deployment.md) | Container image, the worked config example, CI/CD jobs, Kubernetes and ArgoCD |
| [Federation](docs/federation.md) | One instance per datacenter and one central federating them, and why not to SSH across the WAN |
| [Observability](docs/observability.md) | Scheduling behaviour, structured logs, and the Prometheus metrics worth alerting on |
| [Refresh](docs/on-demand-refresh.md) | Getting current facts for a named host across a federated mesh, and the limits that stop consumers overloading a datacenter |
| [Testing](docs/testing.md) | How to run the suite, what the unit and integration tests cover, and how to add more |
| [Views](docs/views.md) | Presenting several sources as one id, how a host is routed to its owner, and why ownership is declared rather than inferred |

---

## 🤝 Contributing

Contributions are welcome: [CONTRIBUTING.md](CONTRIBUTING.md) covers the PR workflow, commit style, CI gates and architecture rules.

Security issues: see [SECURITY.md](SECURITY.md) for private reporting.

---

## 👥 Authors

- [Fernando Roca](https://github.com/fernandorocagonzalez)
- [Blai Peidro](https://github.com/blaipr)

---

## ⚖️ License

[Apache 2.0](LICENSE)
