# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Per-source freshness gauges on `GET /metrics`: `unified_api_source_cached`,
  `unified_api_source_age_seconds`, `unified_api_source_ttl_seconds`,
  `unified_api_source_fresh`, `unified_api_source_hosts` and
  `unified_api_source_groups`, all labeled by source. Read from the cache on
  every scrape rather than pushed on sync, so age keeps growing while a source
  is not syncing — previously only counters and histograms existed, so "is any
  source stale?" could not be answered from Prometheus at all, and a source
  whose scheduler task stopped ticking produced no errors to alert on.
  `unified_api_source_cached` covers configured sources that have never
  synced, which would otherwise be an absent series.
- Per-source sync health on `GET /sources` and `GET /sources/{id}/status`: a
  `sync_health` block with `last_attempt_age_seconds`,
  `last_success_age_seconds`, `last_error` and `consecutive_failures`. A failed
  scheduled sync previously left nothing behind but a log line — the dataset
  just kept aging — so "the connector has been broken for six hours" and "this
  source syncs daily" were indistinguishable through the API. Recorded in
  `application::sync` so the scheduler and the HTTP route cannot drift, and
  kept in a registry outside the cache so a source with no entry still has
  somewhere to record its error. A success clears `last_error` and the failure
  count; the last success age survives failures, which is what makes "worked N
  hours ago, failing since" readable.

## [0.4.0] - 2026-07-26

### Changed

- **Breaking (consumers reading `total_hosts` from `/status`):** `total_hosts`
  is now the source's full host count, and the number of entries in the
  response is reported as the new `returned` field — mirroring the
  `total_hosts`/`returned` pair the `/dataset` envelope already uses. It
  previously counted hosts *after* filtering, so `?host=motoko` answered
  `total_hosts: 1`, which reads as "this source has one host". Unfiltered
  requests are unaffected: both fields equal the old value.

### Fixed

- `GET /status` no longer returns the same host twice. A group whose member
  list carries a host more than once (a connector emitting it under two
  nested groups that get merged) produced one entry per occurrence and
  counted each in `total_hosts`; `?host=a,a` did the same. The dataset
  endpoint has always deduplicated its selection — status now agrees.
- **Breaking (consumers branching on 404):** `?group=` on `/dataset` and
  `/status` now returns an empty result instead of `404` when the group
  matches nothing, completing the change 0.3.9 made for `?host=`. A filter
  that matches nothing is an empty collection, not a missing resource — and
  it matters more for groups, because auto-groups derive their names from
  fact keys: `?group=autofs` used to `404` until some host reported autofs
  data, then start working, with no change to the request. `404` is still
  returned when the source itself is not in cache.
- Both handlers now resolve the `?host=`/`?group=` filter through one shared
  helper, so `/dataset` and `/status` cannot answer the same filter
  differently again. They had already drifted twice: only the dataset path
  deduplicated its selection, and only after the change above do both treat
  an unmatched group the same way.

## [0.3.9] - 2026-07-25

### Added

- `?fields=` query parameter on `/dataset`: comma-separated list of top-level
  hostvars keys to include. Omitted keys are stripped from the response, so
  `?group=autofs&fields=autofs` returns only the autofs data per host (~1 MB)
  instead of every fact key (~11 MB). Without `fields` the full hostvars are
  returned as before.
- Declarative merge enrichers: a new enrichment mode that copies hostvars
  fields from one source into another by hostname, no script needed — set
  `source_id` and `fields` on an enricher instead of `script_path`.
  Script-based enrichers continue to work as before.
- SSH connector auto-groups: each top-level fact key becomes a group containing
  the hosts that have it (e.g. `?group=autofs` returns only hosts with autofs
  data). Mirrors Ansible's `keyed_groups` behaviour.

### Changed

- **Breaking (enrichers config):** `source_id` in `enrichers.yaml` is renamed
  to `target_id` (it always meant "the dataset being enriched"). `source_id`
  is now an optional field that specifies where to copy fields from in
  declarative merge enrichers. `script_path` is also optional — required only
  for script-based enrichers.

### Fixed

- `?host=` filter on `/dataset` and `/status` now returns an empty result
  instead of `404` when no hosts match. A filter that matches nothing is an
  empty collection, not a missing resource — consistent with standard REST
  conventions. `404` is still returned when the source itself is not in
  cache.

## [0.3.8] - 2026-07-24

### Added

- `ETag` / `If-None-Match` support on plain `GET /dataset`: responses carry a
  strong ETag derived from the serialized dataset; a matching `If-None-Match`
  answers `304 Not Modified` with no body. The ETag is stable across restarts
  for identical data and changes on any mutation (sync, enricher, host
  PUT/DELETE).
- Gzip response compression (tower-http `CompressionLayer`) when the client
  sends `Accept-Encoding: gzip`; clients that don't are unaffected.

### Fixed

- Reduced peak memory of full-dataset queries on large sources (e.g. SSH
  facts). The plain `/dataset` path built an intermediate `serde_json::Value`
  tree and then serialized it, holding up to three copies of the dataset at
  once; it now serializes directly to bytes. The paginated/filtered path got
  the same treatment (a borrowing serializer instead of a `Value` envelope).
- `CacheEntry` now holds its dataset and host timestamps behind `Arc`, so
  cache reads share the cached data instead of deep-copying it. Concurrent
  full-dataset pulls, output endpoint renders and disk snapshots no longer
  multiply memory by the dataset size; writers copy-on-write
  (`Arc::make_mut`), so readers keep an immutable snapshot while a sync
  mutates the entry.
- `GET /status` no longer scans every group's member list for every host when
  resolving group TTL overrides (quadratic on large sources) — overrides are
  resolved into a per-host map once per request.

### Changed

- Plain `/dataset` responses are served from a serialize-once cache: the JSON
  is built the first time a changed dataset is read and shared by every
  response until the next mutation.
- Plain `/dataset` responses are semantically identical JSON but no longer
  byte-identical: object keys are no longer sorted alphabetically (a side
  effect of the removed `Value` tree), so byte-level diffing/checksums of the
  response will see changes (use the new ETag instead).
- The periodic cache snapshot is skipped when nothing changed since the last
  save (tracked by a new `CachePort::generation` counter) — an idle instance
  no longer rewrites an identical file every interval.

## [0.3.7] - 2026-07-23

### Changed

- `?host=` query parameter on `/dataset` and `/status` endpoints now accepts
  comma-separated hostnames (e.g. `?host=host1,host2`). Unmatched names are
  silently skipped; 404 only when none match. Single-host queries are unchanged.

## [0.3.6] - 2026-07-22

### Added

- Runtime dependency `python3-pyvmomi` for VMware vCenter inventory connector scripts.

## [0.3.5] - 2026-07-11

### Added

- Federation: `connector_type: "remote"` makes another unified-api instance
  a source — the natural multi-datacenter topology (one instance per DC
  doing the local SSH/scripts, a central aggregating them for consumers).
  `script_path` names the source on the remote, `config.url` the remote base
  URL, and a `token` credential carries the remote API key (pair it with a
  restricted key on the edge). The origin's freshness travels along: the
  central reads the remote `/status` and builds its cache entry with the
  real dataset and per-host ages instead of resetting them on transfer.
  Clear 401/403/404 errors; a failed age lookup degrades to "fresh" with a
  warning. Centrals can be federated in turn (regions → global).

## [0.3.4] - 2026-07-11

### Added

- Dataset pagination and filtering: `GET /sources/{id}/dataset` accepts
  `limit`, `offset`, `host` and `group` query parameters and answers with a
  paginated envelope (`total_hosts`/`offset`/`limit`/`returned` + the sliced
  hostvars, sorted by hostname for stable pages). Without parameters the raw
  Dataset shape is returned unchanged, so existing consumers are unaffected —
  the envelope exists because a 1000-host dataset (~10MB of JSON) hangs
  browser UIs like Swagger when rendered whole.

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

### Security

- Update `crossbeam-epoch` 0.9.18 → 0.9.20, fixing RUSTSEC-2026-0204: an
  invalid pointer dereference in the `fmt::Pointer` impl for `Atomic`/`Shared`
  when the underlying pointer is invalid. Transitive via
  `metrics-exporter-prometheus` → `metrics-util`.

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

- YAML config parsing moved from the deprecated `serde_yaml` (archived by its
  author in March 2024) to `serde_yaml_ng`, a maintained drop-in fork with the
  same API. No config format change.

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
  consumers (AAP, AnsibleForms backends, server-to-server) are unaffected
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

[Unreleased]: https://github.com/OpusProjects/unified-api/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/OpusProjects/unified-api/compare/v0.3.9...v0.4.0
[0.3.9]: https://github.com/OpusProjects/unified-api/compare/v0.3.8...v0.3.9
[0.3.8]: https://github.com/OpusProjects/unified-api/compare/v0.3.7...v0.3.8
[0.3.7]: https://github.com/OpusProjects/unified-api/compare/v0.3.6...v0.3.7
[0.3.6]: https://github.com/OpusProjects/unified-api/compare/v0.3.5...v0.3.6
[0.3.5]: https://github.com/OpusProjects/unified-api/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/OpusProjects/unified-api/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/OpusProjects/unified-api/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/OpusProjects/unified-api/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/OpusProjects/unified-api/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/OpusProjects/unified-api/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/OpusProjects/unified-api/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/OpusProjects/unified-api/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/OpusProjects/unified-api/releases/tag/v0.1.0
