# Unified API

[![unified-api CI](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml/badge.svg)](https://github.com/OpusProjects/unified-api/actions/workflows/build.yaml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg?logo=rust)](https://www.rust-lang.org/)

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
- **Gzip responses**: a client that accepts gzip transfers about a tenth of the bytes
- **Enrichers**: post-process cached data on a schedule or on demand
- **Output endpoints**: turn cached datasets into the format each consumer needs
- **Federation**: one instance per datacenter, one central aggregating them, real ages intact
- **Views**: one id over several sources, routing each host to the member that owns it
- **Scheduled + on-demand sync**: interval sync per source, plus scoped sync over the API
- **Refresh**: a read can bring the hosts it names up to date, bounded by the source's TTL
- **Swagger UI**: interactive OpenAPI docs served at `/swagger-ui/`
- **Single static binary**: axum + tokio, hexagonal architecture, ~11k lines

---

## 📚 Documentation

| Document | What it covers |
|---|---|
| [API](docs/api.md) | All routes with authentication, status code semantics and curl examples |
| [Architecture](docs/architecture.md) | The four layers, the dependency rule and the concurrency model |
| [CLI](docs/cli.md) | Environment variables, log tuning, health checks and common curl operations |
| [Configuration](docs/configuration.md) | Every YAML file field by field, env vars and startup validation |
| [Connectors](docs/connectors.md) | Script contracts for connectors, enrichers and output transformers |
| [Enrichers](docs/enrichers.md) | Post-processing cached data: modes, triggers, freshness rules, health |
| [Projects](docs/projects.md) | Git checkouts: sync styles, script resolution, virtualenvs, health |
| [Deployment](docs/deployment.md) | Container image, worked config example, CI/CD, Kubernetes and ArgoCD |
| [Federation](docs/federation.md) | One instance per datacenter, one central federating them, no WAN SSH |
| [Observability](docs/observability.md) | Scheduling, structured logs and the metrics worth alerting on |
| [Refresh](docs/on-demand-refresh.md) | Bringing a named host up to date at its origin, and what bounds the cost |
| [Testing](docs/testing.md) | Running the suite, what the tests cover, and where new ones belong |
| [Troubleshooting](docs/troubleshooting.md) | Symptom first: what to check when data is stale or a read refuses |
| [TTL](docs/caching.md) | The three-level freshness model, sync modes and TTL overrides |
| [Views](docs/views.md) | Several sources as one id, and how a host is routed to its owner |

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
