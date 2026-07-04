# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-07-04

First tagged release.

### Added

- Source connectors: script (any executable printing inventory JSON) and native
  parallel SSH facts gathering
- In-memory cache with three-level TTL freshness (dataset / host / group),
  per-host and per-group TTL overrides, and atomic merge operations that are
  safe under concurrent writers
- Sync modes (`replace` / `merge`), with full, host-scoped and group-scoped
  syncs over the API and scheduled interval syncs per source
- Enrichers: scheduled or on-demand post-processing of cached datasets
- Output endpoints: transform one or more cached datasets through a script
  (e.g. merged Ansible inventory), with dynamic per-request parameters
- Execution timeouts (`timeout_seconds`, default 300) on connectors, enrichers
  and output transformers — a hung script fails the run instead of blocking it
- REST API with OpenAPI spec and Swagger UI; optional static API key auth
  (`X-API-Key` / `Bearer`, constant-time comparison)
- Split YAML configuration with startup cross-reference validation; secrets
  resolved from environment variables or JSON files, never stored in config
- Health (`/healthz`) and readiness (`/readyz`) probes
- Docker image (multi-stage, non-root) published to GHCR; CI gates on
  rustfmt, clippy and the test suite; Dependabot for workflow actions

[Unreleased]: https://github.com/OpusProjects/unified-api/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/OpusProjects/unified-api/releases/tag/v0.1.0
