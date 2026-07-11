# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [0.3.3] - 2026-07-11

### Fixed

- SSH connector with RSA keys against modern servers: the publickey signature
  was hardcoded to legacy `ssh-rsa` (SHA-1), which RHEL9-era crypto policies
  and OpenSSH ≥ 8.8 defaults reject — the same key worked with the OpenSSH
  client but failed through the API. The signature hash is now negotiated per
  host via the `server-sig-algs` extension; servers without the extension are
  tried with SHA-256 and fall back to SHA-1 if rejected. ed25519/ecdsa keys
  were never affected.

### Added

- `ssh_legacy_algorithms: "true"` (SSH source config): additionally offers
  SHA-1 KEX and MAC algorithms — appended after the modern ones — for
  OpenSSH 5.x-era hosts (EL6) that lack `hmac-sha2` entirely.

## [0.3.2] - 2026-07-11

### Added

- Dynamic host lists for SSH sources: `hosts_from_source` takes the hosts
  from another source's cached dataset (`source` + `match_pattern` as the
  union of groups and hosts + `connect_via`), chaining "the inventory source
  says what exists, SSH says how it is doing" with no glue scripts.
  `connect_via` picks the dial address per host — `hostname`, `ansible_host`,
  or fallback combos where a connection failure tries the next candidate
  (auth failures don't); results stay keyed by the inventory hostname.
- SSH observability: per-attempt WARNs with host/address/attempt, per-host
  duration at DEBUG, and an end-of-sync summary listing every unreachable
  host — the slow ones never delay the rest (continuous semaphore pipeline,
  not batches).

## [0.3.1] - 2026-07-11

### Added

- `script_args` on sources, enrichers and output endpoints: CLI arguments
  passed verbatim to the script (no shell), so scripts implementing the
  standard Ansible dynamic inventory interface (`--list`) work unmodified —
  no more wrapper scripts. SSH sources append them to the remote command in
  `script` gather mode.
- The Docker image now ships the Python libraries connector scripts most
  commonly import — `requests`, `PyYAML`, `jinja2` (via apt, so they track
  distro security updates) — plus a `python` → `python3` symlink
  (`python-is-python3`). Removes the need for init containers installing
  pip packages at pod start.
- New `connector_type: "static_inventory"`: parses classic Ansible static
  YAML inventories (`inventory.yaml` + `group_vars/` + `host_vars/`) natively
  from disk — no process, no `ansible-core` in the image. Host variables are
  flattened with documented precedence; groups keep hosts/children/vars.
  Pairs with a git project so the inventory repo's pull cycle refreshes the
  data. Vaulted files, host ranges and malformed YAML fail the sync with the
  file/group named.
- `output_format: "ansible"` on sources: converts standard Ansible dynamic
  inventory JSON (`_meta.hostvars` + top-level groups, including the legacy
  list form) into the internal Dataset, so existing inventory scripts plug in
  without changes. Malformed groups fail the sync with the group named; the
  implicit `all`/`ungrouped` meta-groups are skipped with a warning when they
  carry information. Sources left on the default `native` format now log a
  WARN when their output parses to 0 hosts but looks like Ansible JSON —
  previously that misconfiguration produced a silent empty inventory.

## [0.3.0] - 2026-07-08

### Added

- On-demand project sync: `POST /api/v1/projects/{id}/sync` (admin keys only)
  clones/updates a project checkout without restarting — made for pipelines in
  the scripts repository. `GET /api/v1/projects` lists projects with their
  checkout state. New per-project `sync_on_boot` (default `true`): set to
  `false` to start from an existing checkout as-is (no network at boot, pairs
  with a persistent volume) while a missing checkout is still cloned.

- Git project cloning: at boot the app shallow-clones every `projects.yaml`
  repository into `projects.dir` (config.yaml, default `./projects`) and
  re-pulls on `sync_interval_seconds` (fetch + hard reset). Relative script
  paths that exist inside a project's checkout run from there; anything else
  (absolute paths, image-baked scripts, SSH remote commands) keeps working
  unchanged. Private repos authenticate with a `token` credential (https,
  secret passed via environment, never argv) or an `ssh_key` credential
  (GIT_SSH_COMMAND). Enrichers and endpoints gain an optional `project_id`.
  The Docker image now ships `git` and a writable `/var/lib/unified-api`.

### Changed

- `projects.yaml`: `sync_interval` (a cron string that was never read) is now
  `sync_interval_seconds`, matching sources and enrichers.

- Scoped API keys: `api_keys.yaml` defines named keys whose secrets live in
  environment variables. A key is either `role: admin` (everything) or
  restricted to explicit `sources`/`endpoints` id lists — restricted keys see
  filtered list responses and get `403` elsewhere. The legacy
  `UNIFIED_API_KEY` env var keeps working as an extra admin key, and key
  rotation stays an external process (swap the env var value and restart).

- Optional cache persistence to disk: a `cache.persistence` block in
  `config.yaml` (snapshot `path` + `interval_seconds`, default 60) makes the
  app snapshot the in-memory cache atomically on an interval and on graceful
  shutdown, and reload it at boot — restarts serve the pre-restart data
  immediately (`/readyz` green from second zero) while the first syncs run.
  Without the block the cache stays purely in-memory as before.

## [0.2.1] - 2026-07-05

### Changed

- Reorganized the adapters into inbound/outbound (`in`/`out`) folders and moved
  the test fixtures under `tests/adapters/out/` to mirror them. The Docker
  image's bundled demo scripts moved from `/app/test-connectors/` to
  `/app/tests/adapters/out/` accordingly (only affects the zero-config demo;
  production deployments mount their own `config/`).

### Added

- Testing documentation (`docs/testing.md`), linked from the README and
  CONTRIBUTING.

## [0.2.0] - 2026-07-04

### Security

- Bump `russh` 0.48 → 0.62.1 (via 0.60.3), fixing two high-severity advisories:
  unbounded 32-bit allocation (RUSTSEC-2026-0154) and unchecked
  `CryptoVec` growth (RUSTSEC-2026-0153)

### Added

- Prometheus metrics at `GET /metrics`: counters and duration histograms for
  syncs, enricher runs and output endpoint runs
- `server.cors_allowed_origins` config to opt in to CORS for browser consumers
- HTTP request logging: method, path, status and latency at INFO per request
- Docker `HEALTHCHECK` querying `/healthz`
- Startup `WARN` when `UNIFIED_API_KEY` is unset (API running without auth)
- CI: `cargo audit` (RUSTSEC advisory scan) and Dockerfile build on PRs
- CI: version tags create a GitHub Release with the changelog section as notes
- Dependabot for Cargo dependencies (grouped weekly), alongside workflow actions

### Changed

- **Breaking (browser consumers only):** CORS is now disabled by default;
  the API previously sent allow-anything CORS headers. Server-to-server
  consumers (AWX, AnsibleForms backends) are unaffected
- **Breaking (SSH sources only):** the SSH connector's per-host timeout config
  key is renamed `timeout_seconds` → `ssh_connect_timeout_seconds` (it collided
  with the source-level `timeout_seconds`); an SSH source that set the old key
  falls back to the 30s default until renamed

### Fixed

- Connector/enricher/output serialization failures now fail the run with a clear
  error instead of silently sending the script empty stdin
- Invalid `cors_allowed_origins` entries are logged and skipped instead of
  silently dropped

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

[0.2.1]: https://github.com/OpusProjects/unified-api/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/OpusProjects/unified-api/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/OpusProjects/unified-api/releases/tag/v0.1.0
