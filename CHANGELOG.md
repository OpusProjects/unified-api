# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.29.0] - 2026-09-02

### Added

- **`limit:` on an output endpoint — a constructed inventory.** An endpoint
  merges everything its sources carry and then returns only the hosts one of
  them has:

  ```yaml
  ep-awx-managed:
    source_ids: ["src-cmdb", "src-vmware", "src-facts"]
    output: ansible
    limit:
      by_hosts_from_inventory: "src-cmdb"
  ```

  One source decides who is in, the others decide what is known. A host the
  limit keeps arrives with every variable, group and membership the other
  sources gave it; a VM that exists in vCenter and not in the CMDB does not
  appear at all. Until now the only way to get that was a source list that
  merged less, which also lost the enrichment.

  The limit runs on the datasets before a transformer is chosen, so a script
  gets the same inventory a builtin does — an endpoint's scope should not
  depend on how it is rendered. It is deliberately not overridable per request:
  the `config:` filters are transformer settings a caller may override, while a
  limit is the endpoint's scope, and an endpoint is granted to keys that may
  not read its sources raw.

  A group the limit empties keeps its vars, unlike one a `filter_*` empties: a
  limit says which hosts the inventory has, not which groups stopped meaning
  anything. Rendering still never touches the cache — the trimming is
  copy-on-write.

  `limit:` holds one rule per field, so further kinds arrive under the same
  key. Config validation refuses a limit naming a source outside the endpoint's
  `source_ids`, a limit with no rule at all, and (as everywhere) a misspelled
  key. See [docs/endpoints.md](docs/endpoints.md#limits-a-constructed-inventory).

## [0.28.0] - 2026-09-01

### Added

- **`groups`, `groups_excluded` and `fields_excluded` on a declarative
  enricher.** With the existing `fields` that is two axes and two directions,
  under one rule: an absent list selects everything, a present one selects only
  what it names, and an exclusion beats an inclusion.

  The group axis is the one that was missing, and it is where the real boundary
  usually runs. A tenancy's local accounts — password hashes, SSH keys — sit on
  that tenancy's own group, beside the login name every play needs. Both are
  called the same thing on every tenancy's group, so no list of variable *names*
  separates them; the group name does.

  `all` is exempt from needing a matching group in the target, not from being
  selected against: an explicit `groups` list is the whole list, `all` included.
  Excluding a group stops the source's vars reaching it and does not remove a
  group the target owns.

### Changed

- **A declarative enricher creates a group the target does not have, instead of
  skipping it — and an endpoint renders a group that has vars but no hosts.**
  Two halves of one thing: publishing what a group *means* for members that are
  settled somewhere else.

  A source declares a group's variables; who is in it is often not its to know.
  Device42 decides membership on its next sync, or Ansible's `group_by` does at
  play time — it puts a host into an existing group of the same name and picks
  up the vars it finds there. Skipping the group lost every variable declared
  for one the target had not heard of yet, which for an inventory whose groups
  come from a different system is most of them.

  The render side matters as much: a group with vars and no hosts was dropped
  as empty, so even a created group would have been thrown away between the
  enricher writing it and the endpoint answering — silently, with the enricher
  reporting success.

  A group that *did* name hosts and lost them all to a filter is still pruned:
  the filter's answer for it is nothing, vars or no vars. The two empty groups
  are opposite cases and are treated as such.

  No host is added by any of this. An enricher still moves variables only.

## [0.27.0] - 2026-09-01

### Changed

- **A declarative enricher with no `fields` now takes every var the source
  declares, instead of none.** Being in a group carries all of that group's vars
  in Ansible — there is no per-name permission — so `fields` is the narrowing,
  and its absence should mean the ordinary thing rather than the empty one. It
  meant the empty one: an enricher with a `source_id`, a `target_id` and no
  `fields` ran, copied nothing and reported success, which is a config that
  looks active and is not.

  `fields: []` is unchanged and still takes nothing: an explicitly empty list
  names nothing, which is a different statement from not naming any.

  Omitting `fields` hands the target everything the matching groups hold,
  including whatever sits beside the variable that was wanted. An endpoint's
  `exclude_vars` can drop a name on the way out, but it does not narrow what was
  written into the target source itself.

## [0.26.0] - 2026-09-01

### Changed

- **A declarative enricher carries the source's group vars onto the target's
  groups, instead of resolving them onto each host.** 0.25.0 taught the merge to
  find a field declared on a group; it wrote the result onto every member, so a
  value shared by 780 machines cost 780 entries — the duplication the same
  release had just taken out of the static-inventory connector. It is now merged
  onto the target's group of the same name, one copy, and the consumer resolves
  it with its own precedence: `all`, then more specific groups, then host vars,
  then `extra_vars`.

  `all` needs no matching name, because in Ansible it means every host. The
  merged `all` carries the target's hostnames, since an endpoint drops a group
  with neither hosts nor children and the variables would otherwise be rendered
  away without a word.

  A field the source declares **on a host** is still copied onto that host —
  that data is genuinely per host. `fields` filters both paths, and a group the
  target does not have is skipped. No host ever crosses between sources.

  Consumers reading `_meta.hostvars` for a value that is declared on a group now
  find it on the group instead. Through Ansible, or anything that resolves an
  inventory, the answer per host is unchanged.

## [0.25.0] - 2026-09-01

### Added

- **A declarative enricher resolves the source's group vars, not just its
  hostvars.** For each host of the target it now looks the field up in the
  source's groups — every group the *target* places the host in, ancestors
  included — before falling back to, and being overridden by, the source's own
  hostvars for that host. Membership comes from the target and values from the
  source, which is what lets a source describe a group it holds no members of:
  the usual shape for a variable that describes a whole tenancy rather than a
  machine, and the only shape available once group vars are no longer flattened
  onto every host. Where two of a host's groups declare the same field the more
  deeply nested one wins, ties at one depth broken alphabetically. Only
  variables cross — a shared group name cannot pull one source's hosts into
  another.

### Changed

- **A static inventory emits variables where they are declared, instead of
  resolving them onto every host.** A group's vars stay on the group and a
  host's on the host; Ansible applies its own precedence when it reads the
  inventory, which it is the authority on. `all` is now emitted as a group,
  because that is where `group_vars/all` lands. The resolved result on each
  host is unchanged for an Ansible consumer.

  It was resolved in the connector until now, and the cost is why it is not
  any more: copying a group's vars onto each member meant a 1097-host
  inventory whose `group_vars/all` is 55 KB carried that 55 KB a thousand
  times over — a 53 MB dataset, ~94% of it the same bytes — and the server was
  OOMKilled on every sync. Anything reading `hostvars` directly rather than
  through Ansible (`output: json`, a declarative enricher) must now resolve
  group vars itself.

### Fixed

- **`exclude_vars` drops the name from groups as well as hosts.** It stripped
  `hostvars` only, so a variable declared on a group reached every member
  through the group door — with group vars no longer flattened, that is the
  usual place for one to live. An endpoint's exclusion list is how something
  is kept out of it, and a filter with a way around it is not one.

## [0.24.1] - 2026-09-01

### Fixed

- **A static inventory reads `group_vars/<name>/` directories, not only
  `group_vars/<name>.yaml`.** Ansible accepts either layout; only the flat one
  was read, so an inventory using directories — one file per concern, which is
  how a large one stays readable — lost every variable it declared. Silently:
  the sync reported every host and every group, each with no vars at all, and
  the "no matching group" warning never fired because the map was empty rather
  than mismatched. Files within a directory merge alphabetically, matching
  Ansible, and a name defined both as a file and as a directory now fails the
  sync instead of one of them quietly winning.

## [0.24.0] - 2026-08-30

### Added

- **`server.metrics_require_auth` now applies on a configuration reload.**
  The `/metrics` route is always registered public and the handler checks the
  flag (and the API key, when required) on every scrape, instead of the flag
  deciding router placement at startup — it was the one *security* setting on
  the restart-only list, where "we changed it and it didn't take" is the worst
  failure mode. Behavior is otherwise unchanged: public by default, and with
  no keys configured the flag keeps having no effect. A 401 from `/metrics`
  carries the standard error body.

- **`server.cors_allowed_origins` now applies on a configuration reload.**
  The CORS middleware reads the origin list from the current snapshot on
  every request instead of baking it into the router at startup, so adding a
  new dashboard's origin (or revoking one) is a config push, not a fleet
  restart. Behavior is otherwise unchanged — an empty list still means no
  CORS headers at all, and preflight handling is identical.

- **`server.max_body_bytes` now applies on a configuration reload.** The
  body-limit middleware reads the limit from the current snapshot per
  request, so raising it for a configuration push that outgrew the old value
  is itself just a push — the awkward case where fixing the limit used to
  require the restart the limit was blocking you into. With this, the
  restart-only list is down to the genuinely structural: `server.host`,
  `server.port`, `cache.persistence`, `projects.dir` and
  `config_api.enabled`.

### Changed

- **SSH host certificates are refused when `ssh_known_hosts` verification is
  on.** The SSH library (russh 0.63) can now present an OpenSSH *certificate*
  as a server's host key. `known_hosts` verification is defined over plain
  host keys — trusting a certificate needs a CA model the connector does not
  have — so with verification enabled such a server is refused whole, with a
  warning naming the host, rather than half-checked. The accept-any default
  (no `ssh_known_hosts` configured) is unchanged.

## [0.23.0] - 2026-08-26

### Added

- **Live reload now covers the refresh limits and the shutdown grace.**
  `server.refresh_timeout_seconds`, `server.refresh_max_concurrent` and
  `server.shutdown_grace_seconds` apply on a configuration reload instead of
  being reported as `restart_required`. The refresh budget and a grown
  concurrency cap take effect on the next refresh; a shrunk cap lets in-flight
  refreshes finish under the old limit and reclaims their permits as they
  land; the shutdown grace is read when the drain starts, so the last reloaded
  value governs it.

- **`server.max_body_bytes` makes the request body limit explicit.** The limit
  was always enforced — axum ships a 2 MB default — but silently: nothing
  declared it, and an oversized push (a whole-directory config `PUT` is one
  body) got a bare 413 with no explanation. The limit is now a named
  `config.yaml` setting with the same 2 MiB default, and exceeding it answers
  `413` with the standard `{"error": ...}` body naming the key and the
  configured limit.

### Fixed

- **Endpoint failures render through the standard error shape, and timed-out
  runs are counted.** The output endpoint's 500 and 504 bodies were hand-built
  JSON; they now come from the same `ApiError` as every other failure, the 503
  keeps its `missing_sources` list under a declared schema, and all three plus
  the 504 appear in the OpenAPI spec. A timed-out run also used to return
  before the counters, so `unified_api_endpoint_total` never saw it — despite
  the docs saying timed-out runs count as `result="error"`. They do now.
- **`GET /metrics` appears in the OpenAPI spec**, including that
  `server.metrics_require_auth: true` moves it behind the API key.

## [0.22.0] - 2026-08-22

### Added

- **`output: json` and `output: csv` builtin transformers.** Two more formats
  render in-process alongside `output: ansible`, sharing its merge pipeline and
  its `filter_datacenter` / `filter_os` / `filter_group` / `exclude_vars`
  filters (each overridable per request): `json` serves the merged, filtered
  inventory in the raw source shape (`hostvars` + `groups`) as
  `application/json`, and `csv` serves one row per host as `text/csv`, with
  columns picked and ordered by a `columns` setting (default: every hostvar
  name seen, sorted). Renders are deterministic — identical inventory renders
  byte-for-byte identically.

- **Reload and build observability on `/metrics`.** Three gauges, computed on
  each scrape: `unified_api_config_restart_required` counts the restart-only
  keys the last applied reload changed — non-zero means the pod runs on a
  configuration it could only partially adopt, a state that until now was
  visible only via `GET /api/v1/config` on each pod;
  `unified_api_config_generation` is the number of applied reloads since the
  process started, so a pod lagging a fleet-wide configuration push is one
  query away; and `unified_api_build_info{version}` is the classic constant-1
  info metric carrying the running version as a label.

### Changed

- **`endpoints.yaml`: `timeout_seconds` on a builtin endpoint is now a config
  error named at load**, like `project_id` and `script_args` since 0.21.0 —
  a builtin runs in-process, so the script timeout it names never applied.

### Fixed

- **`401` responses now carry the standard `{"error": ...}` body.** The auth
  middleware answered a missing or invalid API key with an empty body — the
  one error every new consumer hits first, and the last one in the API that
  said nothing. The message also distinguishes the two states, because their
  fixes differ: `missing API key` names the `X-API-Key` header to pass,
  `invalid API key` means one arrived and matched no configured key.

## [0.21.0] - 2026-08-20

### Added

- **Builtin output transformers.** An output endpoint can now render a format
  in-process with `output: ansible` instead of shelling out to a script:
  `GET/POST /api/v1/endpoints/{id}` merges the configured sources and renders
  Ansible dynamic inventory (`_meta.hostvars` plus one key per group), with the
  same `filter_datacenter` / `filter_os` / `filter_group` / `exclude_vars`
  filters the shipped script offered. No per-request interpreter spawn, no
  project checkout, and the logic is tested in the binary. The `script_path`
  form stays for bespoke formats.

### Changed

- **`endpoints.yaml`: `script_path` is now optional.** An endpoint sets exactly
  one of `output` (a builtin) or `script_path` (a script); setting neither or
  both is a config error named at load, and `project_id` / `script_args` on a
  builtin endpoint are rejected rather than silently ignored.

## [0.20.0] - 2026-08-19

### Added

- **Configuration API.** The configuration directory is now readable and
  writable over HTTP, so a configuration-as-code pipeline can push a change to
  an instance instead of publishing an artifact the instance has to pull:
  `GET/PUT /api/v1/config` (the whole directory in one transaction, with
  `prune` for image-like semantics), `GET/PUT/DELETE /api/v1/config/{file}`,
  `POST /api/v1/config/validate` (dry run — the same checks as
  `--check-config`, nothing written) and `POST /api/v1/config/reload`. A
  proposed change is staged and validated as a whole directory before anything
  moves, is rejected whole with every error at once, and each file is written
  atomically. ETags (sha256, per file and per directory) with `If-Match` make
  a concurrent overwrite a `412` instead of a silent win. Admin-only, reads
  included. **Off by default** — `config_api.enabled: true` in `config.yaml`
  opts in, and doing so lets an admin key rewrite every file the loader reads,
  `api_keys.yaml` included. See `docs/config-api.md`.

- **Live configuration reload.** `POST /api/v1/config/reload` (or `?reload=true`
  on a write) applies the directory to the running process: sources, views,
  enrichers, endpoints, projects, credentials, the secrets settings, API keys
  and `server.readyz_require_all_sources` all take effect with no restart, and
  the scheduler replaces its task generation — new sources start syncing,
  removed ones stop, and a project that arrived with the reload is cloned in
  the background. Settings a running process cannot adopt (`server.host`,
  `server.port`, `server.cors_allowed_origins`, `server.metrics_require_auth`,
  the refresh settings, `server.shutdown_grace_seconds`, `cache.persistence`,
  `projects.dir`, `config_api.enabled`) are reported as `restart_required`
  rather than silently ignored, and keep being reported by `GET /api/v1/config`
  until a restart adopts them. A reload that would leave the API with no keys
  at all is refused (`409`), as is one naming an API key env var that is not
  set — before anything is committed.

- New counters `unified_api_config_writes_total{outcome}` and
  `unified_api_config_reloads_total{outcome}`, and audit events
  (`config_write`, `config_write_reload`, `config_reload`) alongside the
  existing write-route trail.

### Changed

- **Configuration errors are a list, not a blob.** `AppConfig::validate_errors`
  and `load_config_detailed` expose every problem as an item, which the
  configuration API returns as an `errors` array alongside the usual `error`
  field. A YAML parse error now names the file it came from — a line and a
  column in a directory of eight files was half an answer. `--check-config`
  output is unchanged.

## [0.19.0] - 2026-08-18

### Added

- **Audit trail for write operations.** Every mutating route that actually
  runs — sync, cache evict, host put/delete, enricher run, project sync —
  emits one structured log event under the dedicated `audit` tracing target:
  `actor` (API key name, never the secret), `action`, `resource`,
  `request_id` and `outcome`. Filter or route it independently of the rest of
  the logs (`RUST_LOG=warn,audit=info`).

- **Validate-only config check.** `unified-api --check-config` loads and
  validates the configuration directory exactly as startup would — strict
  keys, cross-references, cron expressions — prints every error found, and
  exits 0/1 without binding, scheduling or resolving secrets. Run it in the
  CI of a config repository so a typo fails the pull request instead of the
  deploy.

### Changed

- **Federation pulls are conditional.** The remote connector now revalidates
  full pulls with the edge's `ETag` (`If-None-Match`); a `304` skips the
  transfer and the re-parse while the sync still refreshes ages, scope and
  health as before. A central polling an unchanged edge pays header bytes per
  tick instead of the full dataset. New counter
  `unified_api_remote_not_modified_total{url, source}` counts the skips.

## [0.18.0] - 2026-08-17

### Added

- **Enricher runs carry a `trigger`.** Script enrichers now receive the
  reserved `trigger` key inside `SOURCE_CONFIG`, exactly like connectors: the
  request id for `POST /enrichers/{id}/run`, `scheduled` for background runs,
  and — when enrichment is re-applied after a sync — the trigger of that sync
  itself, so a script's logs join the same trace end to end.

- **Cron schedules for enrichers and project pulls.** The `schedule` field
  sources gained in 0.17 now works on enrichers and git projects with
  identical semantics: standard 5-field cron (optional leading seconds)
  evaluated in UTC, validated at startup, mutually exclusive with a non-zero
  `sync_interval_seconds`, exact times with no startup jitter, and failure
  backoff by letting occurrences pass. A project's boot clone is unchanged —
  cron only paces the re-pulls.

- **View ownership can resolve from what a member advertises** — the second
  half, ending federation's duplicated truth. `advertised: true` on a view
  member routes by the member source's own claim: read from local config for
  local members, fetched from the edge's `GET /scope` with every sync for
  remote members (best effort — an edge too old for the route degrades
  cleanly). The rules, in the order they apply: live claim, else last-known
  claim (an unreachable edge keeps routing — stale routing beats no routing),
  else the declared `groups`/`hosts` as fallback, else the member claims
  **nothing** — an unknown advertisement never widens into a catch-all.
  `GET /status` reports each member's `ownership_mode`
  (`declared`/`advertised`/`fallback`/`unknown`), and startup validation
  refuses an advertised local member that could never route. Declared
  ownership is untouched: keep the fallback during a mixed-version rollout,
  drop it once every edge serves `/scope`.

- **Sources advertise their ownership scope.** `GET /api/v1/sources/{id}/scope`
  answers "what does this source claim to own" from **configuration, never
  cache contents**: an explicit new `advertise_scope` block (`groups` +
  `hosts`), else the `hosts_from_source` match pattern an SSH source already
  gathers by, else `declared: false`. A claims-everything pattern is stated as
  `catch_all: true` rather than bare empty lists, and an explicitly empty
  `advertise_scope` fails validation at startup. Views answer too, with the
  union of their members' declared ownership.

  This is the first half of removing federation's duplicated truth — the edge
  says "I am datacenter_dc2" in its own config and the central's view repeats
  it, driftably. The consumer side (view ownership resolved from what a
  member advertises) follows.

## [0.17.0] - 2026-08-16

### Added

- **The triggering request follows the work into the scripts.** Every sync
  hands the connector a `trigger` key inside `SOURCE_CONFIG`: the HTTP
  request id for a manual `POST /sync` (the same id the access log and the
  `x-request-id` response header carry), `scheduled` for timer-driven syncs,
  `refresh` for on-demand reads. Output endpoints get the same key in
  `ENDPOINT_CONFIG`. A connector's own log lines can now join the exact trace
  that caused them — the missing last hop of the request-id work from 0.15.

- **Cron schedules for sources.** The `schedule` field has existed since the
  beginning — "reserved for future", silently ignored, shipped in the sample
  config doing nothing. It now works: standard 5-field cron (optional leading
  seconds field), evaluated in **UTC**, as the alternative to
  `sync_interval_seconds` — "sync at 02:30" instead of "sync every N
  seconds". Cron sources fire at their exact times (no startup jitter — the
  times are deliberate) and back off on failure by letting occurrences pass,
  1, 2, 4, up to 8 apart, exactly like interval sources; supervision and
  shutdown drain apply unchanged.

  **Breaking (configs carrying a `schedule` value):** the field is now
  validated — an expression that does not parse fails startup naming the
  source, and `schedule` together with a non-zero `sync_interval_seconds` is
  refused ("pick one"). Both were silently ignored before; a config that
  relied on that silence needs the junk removed or the interval dropped.

- **Per-project Python virtualenvs.** A project with `python_venv: true` and
  a `requirements.txt` in its checkout gets a real virtualenv: built after
  the clone, refreshed after any pull that changed `requirements.txt`
  (unchanged pulls cost two file reads, not a pip run), stored OUTSIDE the
  checkout (`<projects.dir>/.venvs/<project>`) so the hard reset cannot wipe
  it. When that project's scripts run — connectors, enrichers, output
  endpoints — the venv's `bin/` is prepended to their PATH, so a
  `#!/usr/bin/env python3` shebang resolves to the venv's interpreter and
  pip-installed imports work, while non-Python scripts stay untouched.

  A failing install (a typo'd package, an unreachable index) fails the
  project sync — visible in its `sync_health` and bounded by the project's
  `timeout_seconds` — instead of surfacing later as one confusing import
  error per source. Until now a connector needing a single PyPI package
  meant baking a derived image; that remains necessary only for non-Python
  tooling. The image ships `python3-venv`/`python3-pip` to support this.

## [0.16.0] - 2026-08-15

### Added

- **Ready-to-apply deployment manifests under `deploy/`.** The docker compose
  file and complete Kubernetes manifests (Deployment with probes, non-root
  and resources, Service, PVC, kustomization) that `docs/deployment.md` used
  to embed as prose — now real files, linted in CI on every PR
  (`docker compose config` and kubeconform `-strict`), so what users apply
  can no longer silently drift from what the docs describe.

- **Native HashiCorp Vault resolution** — the adapter the docs have promised
  as roadmap since the `SecretsPort` existed. Give a credential a
  `vault_path` (KV v2, under the configured mount) and configure the new
  `secrets.vault:` block (`address`, `mount`, and either `token_env` or
  `kubernetes_role` + `jwt_path`); `secret_keys` then maps our names to
  fields of the Vault secret, or takes every field verbatim when omitted.

  Adoption is per credential: anything without a `vault_path` keeps resolving
  from env vars and files exactly as before, so the three mechanisms coexist
  during a migration. A `vault_path` with no `secrets.vault:` block fails
  validation at startup. With Kubernetes auth the client token is cached and
  renewed at 80% of its lease; with token auth the env var is re-read per
  resolution, so token rotation needs no restart. Every Vault request is
  bounded by `secrets.vault.timeout_seconds` (default 10), failures fail the
  sync naming the credential and land in `sync_health` like any other
  resolution error, and the credential cache above keeps the sync schedule
  from becoming a request storm against Vault.

- **Credential resolution is cached for a short TTL.** Credentials were
  re-resolved — an env read, a JSON secret file re-parsed — on every sync of
  every source, which was free right up until the backend is a network call
  away. Successful resolutions are now reused for
  `secrets.cache_ttl_seconds` (new `config.yaml` section; default 60,
  0 disables the cache). Errors are never cached — a transient backend blip
  is retried on the next resolution instead of being remembered for the TTL.

  The TTL is also the rotation latency, stated plainly: a secret rotated in
  the environment or on disk is picked up within `cache_ttl_seconds` rather
  than on the very next sync. Set it to 0 to keep the old behavior.

## [0.15.0] - 2026-08-14

### Added

- **HTTP request metrics.** `/metrics` now carries
  `unified_api_http_requests_total{method, path, status}` and a
  `unified_api_http_request_duration_seconds{method, path}` latency histogram
  — API-side SLOs were unmeasurable while syncs, enrichers and endpoints each
  had counters and the HTTP surface serving the consumers had none. The
  `path` label is always the **matched route pattern**
  (`/api/v1/sources/{id}/dataset`), never the raw URL, so cardinality stays
  one series per route rather than one per host; requests matching no route
  share `path="unmatched"`.

- **Requests are identifiable, in logs and in responses.** Every response now
  carries an `x-request-id` header: a client-provided id is kept (stitch the
  service's log lines into your own trace), otherwise a per-process counter
  assigns one. The access-log span carries the same `request_id`, and
  authenticated requests log the `key_name` of the API key that made the call
  — so an access-log line answers who did what, and an error report quoting
  an id finds its exact lines.

### Changed

- **Concurrent full syncs of one source coalesce onto a single gather.** The
  0.11.0 per-source lock serialized them but did not deduplicate: N
  simultaneous `POST /sync` requests were N sequential complete datacenter
  gathers. A full sync that finds another full sync completed while it queued
  now answers from that result — a sync that *started after the request
  began* is everything the request could have asked for, the same reasoning
  the read path applies when it re-checks staleness under its lock. The
  response says so: `coalesced: true`, `sync_duration_ms: 0`, and the counts
  report what the winning sync left in the cache
  (`unified_api_sync_total` gains `result="coalesced"`). Scoped syncs and
  `refresh_origin` requests never coalesce — they ask for something a plain
  bulk gather does not deliver — and a failed sync satisfies nobody: the next
  request in the queue gathers for real.

- **Duration histograms export real buckets instead of summaries.** Every
  `_duration_seconds` metric now renders as `_bucket`/`_sum`/`_count` series
  (edges from 5 ms up to the 300-second script timeout) rather than
  client-side `quantile` summaries. Bucket sums aggregate across instances —
  `histogram_quantile` over the fleet — which quantile summaries
  mathematically cannot.

  **Breaking (dashboards reading `quantile=` series):** the summary series
  for `unified_api_sync_duration_seconds`, `_enrich_`, and `_endpoint_`
  disappear; switch panels to `histogram_quantile` over the new `_bucket`
  series.

## [0.14.0] - 2026-08-13

### Fixed

- **Shutdown now drains the background tasks before the final snapshot.** The
  final cache snapshot could serialize the cache while a detached sync task
  was still mutating it, and the periodic snapshot task could race the final
  save on the same temp file — renaming a half-written snapshot over a
  complete one. On SIGTERM the service now drains in-flight HTTP requests,
  signals every background task (syncs, enricher runs, project pulls, the
  snapshot task) through a watch channel, waits — bounded by the new
  `server.shutdown_grace_seconds` (default 20) — for their in-flight runs to
  finish, and only then writes the final snapshot. A run mid-gather completes
  and lands in the cache instead of being cut; a task that outlives the grace
  is logged and the snapshot proceeds anyway, since best effort still beats a
  SIGKILL with no snapshot at all. A shutdown arriving during the boot clones
  aborts the remaining clones (killing their git children) instead of
  starting schedulers nobody wants anymore.

### Added

- **The scheduler now survives and softens failure.** Three changes to every
  periodic task (source syncs, enricher runs, project pulls):

  - **Backoff:** a failing task no longer hammers its struggling target at
    exactly `sync_interval_seconds` forever. After a failure the next attempt
    comes 1 interval later, then 2, 4, and at most 8, resetting on the first
    success. Attempts stay aligned to the configured cadence (the backoff lets
    ticks pass rather than shifting the clock), and `sync_health` carries the
    failure streak throughout.
  - **Startup jitter:** each task's schedule is shifted by a deterministic
    per-id offset (≤30 seconds, capped at the interval), so every source no
    longer gathers at the same instant at boot — nor at every common multiple
    of the intervals forever after, since tokio intervals keep their phase.
  - **Panic supervision:** a panic in a task body used to kill the task
    silently — that source simply stopped syncing until someone noticed stale
    data. Task bodies now run under a supervisor that logs the panic, counts
    it in a new `unified_api_scheduler_task_panics_total{task}` metric, and
    restarts the body after one interval.

### Changed

- **Boot no longer waits for git.** The listener now binds and serves before
  the project clones: one unreachable git remote used to mean no `/healthz` at
  all — a failed Kubernetes startup probe for a service whose HTTP layer was
  perfectly able to answer. Clones run in a background task, concurrently
  rather than one after the other, and each is bounded by the project's new
  `timeout_seconds` (default 300; a timed-out git child is killed, not
  abandoned). The sync schedulers start once the clones have had their bounded
  chance, so a source's first sync does not race its own script's clone.
  `/readyz` semantics are unchanged: it stays red until the first sync lands.

- **Script paths resolve into project checkouts at every execution, not once
  at boot.** Same conservative rules as before (absolute paths untouched, the
  checkout wins only when the file exists in it, otherwise the configured path
  stays). Two visible improvements: a script that first appears after startup
  — a slow clone, a pipeline's first push to a new project — is used on the
  very next run instead of after the next restart, and boot no longer needs
  the checkouts before it can build the router.

### Added

- **`timeout_seconds` on projects** (`projects.yaml`, default 300): a
  clone/pull that runs longer is aborted and recorded as a failure in the
  project's `sync_health`, the same convention sources, enrichers and
  endpoints already follow. Before this, a git remote that never answered hung
  its caller forever.

## [0.13.0] - 2026-08-12

### Added

- **Enricher runs, project pulls and cache snapshots record health, like syncs
  always did.** A permanently failing enricher, a project checkout stuck on a
  stale commit because every `git pull` fails, or a full disk killing cache
  persistence were `warn!`/`error!` lines per interval and nothing else —
  nothing an operator could query or alert on. All three now record last
  attempt / last success / last error / consecutive failures into health
  registries, the same pattern (and the same shape) as source sync health:

  - `GET /api/v1/enrichers` carries a `sync_health` block per enricher (a
    target that is not in the cache counts as a failure — the enricher is not
    doing its job either way);
  - `GET /api/v1/projects` carries one per project, which is where "the
    checkout exists but every pull fails" becomes visible, since
    `checkout_present` stays `true` the whole time;
  - `GET /metrics` gains `unified_api_enricher_consecutive_failures` /
    `_last_success_age_seconds` (per enricher),
    `unified_api_project_sync_consecutive_failures` /
    `_last_success_age_seconds` (per project) and
    `unified_api_snapshot_consecutive_failures` /
    `_last_success_age_seconds` (one snapshot task per process). Alert on the
    snapshot task via `consecutive_failures`, not the success age — an idle
    cache skips its snapshots on purpose.

- **Source sync health is exported to Prometheus.** The registry behind the
  `sync_health` block on `GET /sources` and `/status` — consecutive failures,
  last attempt, last success — was invisible to `/metrics`, so alerting had to
  infer a failing connector from the `unified_api_source_fresh == 0` proxy,
  which only fires once the whole TTL has run out: a source failing for two
  hours on a six-hour TTL still read as healthy. Three new per-source gauges,
  computed at scrape time like the freshness ones:

  - `unified_api_source_sync_consecutive_failures` — the streak to alert on
    directly;
  - `unified_api_source_sync_last_attempt_age_seconds` — grows past the sync
    interval when the scheduler task is not running at all, the one failure
    mode that produces no errors anywhere;
  - `unified_api_source_sync_last_success_age_seconds` — absent until a sync
    has ever succeeded, so "never worked" and "stopped working" read
    differently.

  `last_error` deliberately stays API-only: an error string as a label value
  is unbounded cardinality. `docs/observability.md` now carries alert examples
  for the failure streak and the silent-scheduler case.

### Changed

- **Unknown configuration keys are now startup errors.** Only the view structs
  rejected unknown keys; everywhere else a typo'd key was silently dropped and
  the field's default silently applied — `sync_interval_second:` (no `s`) meant
  a source that never syncs on its own, and a misspelled
  `metrics_require_auth:` meant a security setting failing open with no
  indication anywhere. Every configuration struct (`config.yaml`,
  `sources.yaml`, `credentials.yaml`, `enrichers.yaml`, `projects.yaml`,
  `endpoints.yaml`, `api_keys.yaml`) now carries `deny_unknown_fields`, so a
  key the schema does not define refuses to start and names the key — the
  same guarantee the view ownership patterns have had since they existed.

  **Breaking (configs carrying stray keys):** a config file with a key the
  schema does not define — a typo, or a leftover from an older version — now
  fails startup instead of being ignored. The error names the key; fix or
  remove it. Free-form data is unaffected: it belongs under the `config:` maps,
  which remain arbitrary key/value pairs.

## [0.12.0] - 2026-08-11

### Added

- **The SSH connector can verify host keys.** It accepted any server key: the
  handler's `check_server_key` returned true unconditionally, so the connector
  would open sessions — and authenticate — against whatever answered on the
  port. On a spoofed network path that means handing a signature from the
  fleet key to an impostor and ingesting whatever inventory it invents.

  A new per-source config key, `ssh_known_hosts`, points at an OpenSSH
  `known_hosts` file (plain, `[host]:port` and hashed entries are all
  understood — `ssh-keyscan` output works as-is). When set, every server key
  is checked **before authentication**: an unknown or changed key refuses the
  connection with both fingerprints in the log, and the host is reported
  `unreachable` — so, as with any unreachable host, its last known data is
  kept rather than replaced. The file is re-read on every sync, so rotating
  a host key needs no restart; startup validation fails fast on a missing
  file.

  Without `ssh_known_hosts` the behaviour is unchanged — any key is accepted —
  but every such sync now logs a warning instead of staying silent about it.

### Security

- **Scripts no longer inherit the service's environment.** Every spawned
  connector, enricher and output script received the full parent environment —
  every API-key secret and every other source's credential variables, alongside
  the scoped `CREDENTIAL_*` set it was actually granted. A single careless (or
  compromised) connector script could read the admin API key and another
  source's password even though it was only ever given its own.

  The environment is now cleared before the adapter injects the script's own
  variables (`SOURCE_CONFIG`, `CREDENTIAL_*`, `ENDPOINT_CONFIG`,
  `ENDPOINT_PARAMS`). The only variables that pass through from the service are
  the ones a script legitimately needs from the host: `PATH`, `HOME`, `TMPDIR`,
  `LANG`, `LC_ALL`, `TZ`, `PYTHONPATH`, the proxy variables
  (`HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`/`ALL_PROXY`, upper- and lowercase) and
  the CA-bundle variables (`SSL_CERT_FILE`, `SSL_CERT_DIR`,
  `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`).

  **Breaking (scripts reading undocumented environment variables):** a script
  that read arbitrary variables from the service's environment no longer sees
  them. Put such values in the source's/endpoint's `config` map (delivered as
  `SOURCE_CONFIG`/`ENDPOINT_CONFIG`) or in a credential definition (delivered
  as `CREDENTIAL_*`) instead.

## [0.11.0] - 2026-08-01

### Fixed

- **A slow sync can no longer overwrite a gather that started after it.**
  Nothing serialised two syncs of the same source: the scheduler and
  `POST /sync` call the same use case with nothing between them, and an
  on-demand refresh is a third writer. So a full sync of a large source could
  still be gathering when a consumer's `refresh=true` fetched one host, and the
  full sync would then write the dataset it had collected minutes earlier over
  the top of it.

  The stale value was also stamped freshly gathered — a replace rebuilds the
  entry and marks every host "now" — so the next `refresh=true` saw a fresh host
  and declined to correct it. The consumer asked for current facts, got older
  ones, and the freshness data agreed with the wrong answer.

  Syncs of one source now run one after another. A refresh that arrives while a
  full sync is running waits, hits its own `refresh_timeout_seconds`, and serves
  the cached data with `x-unified-api-refreshed: false` — the right outcome for
  a read that may improve its data but must never overtake a gather already in
  flight. Different sources are unaffected and still sync in parallel.

### Added

- **Views are visible in Prometheus.** A view holds no cache entry — it is
  resolved from its members on every read — so it appeared in neither
  `cache.keys()` nor `sources`, and had no metric series at all. The one address
  consumers are pointed at was the one thing impossible to alert on.

  Seven gauges, labelled `view`: `unified_api_view_fresh`, `_age_seconds`,
  `_ttl_seconds`, `_hosts`, `_members_total`, `_members_cached` and
  `_members_routable`. Separate names rather than reusing `unified_api_source_*`
  with a view id, because a view's hosts *are* its members' hosts — one series
  would double-count every host in any sum across the label.

  `_members_routable` is the one with no equivalent elsewhere: every member can
  be cached and fresh while the inventory source their ownership resolves
  against has never synced. The view then claims nothing and serves an empty
  dataset while looking healthy in every other number.

## [0.10.3] - 2026-07-31

### Fixed

- **An endpoint's 403 and 404 now say what went wrong.** `POST`/`GET
  /api/v1/endpoints/{id}` answered both with a bare status code, which axum
  renders with an **empty body** — so a consumer refused an endpoint got a 403
  with nothing in it, while the same refusal on a source route explains itself.
  Its other failures (a script that exited non-zero, sources not yet synced)
  already carried `{"error": ...}`, so the inconsistency sat inside one handler.

  Endpoints are the consumer-facing route — AWX and AnsibleForms call these —
  which makes it the worst place to answer "no" without a reason. Both now use
  `ApiError` like everything else, naming the endpoint id, and the OpenAPI
  responses declare `body = ErrorBody` so the published spec stops describing
  the empty shape.

- **Listing projects no longer blocks the runtime.** `GET /api/v1/projects`
  reported `checkout_present` with a blocking `Path::exists` per configured
  project, from inside an async handler — the same shape as the `secret_file`
  read fixed in 0.10.2, and small for the same reason and only on local disk. A
  checkout on a network or overlay volume is the case that parks a worker thread
  with unrelated requests queued behind it. It now uses `tokio::fs`.

- **A hostname the caller chose can no longer panic the request.** The
  `x-unified-api-refreshed-hosts` header is built from the hosts a successful
  refresh was asked for — and a host the connector did not return is still among
  them, so the value is the caller's own `?host=` text. A percent-encoded control
  byte in it produced an invalid header value, which `Builder::header` defers to
  `body()`, where the handler unwraps: the request panicked and the connection
  dropped instead of answering.

  Reaching it needs a source with `allow_on_demand_refresh` and a connector that
  exits 0 without returning the host it was asked for — which is an ordinary
  connector, since honouring the scope is an optimisation rather than a duty.
  Both refresh headers are now skipped when their value cannot be sent: they are
  metadata about the response, and failing to describe a response is no reason
  to refuse to send it.

- **A static inventory no longer drops hosts from a group declared twice.**
  Declaring one group under two parents is ordinary Ansible, and the parser
  replaced the earlier declaration with the later one — so every host the first
  carried disappeared from that group. Silently: the host stayed in `hostvars`,
  so nothing looked wrong until an inventory rendered from `groups` failed to
  target it, or a play matching the group skipped a machine that was plainly in
  the file.

  Declarations are now merged — hosts, children and group vars alike — and a
  group's ancestry is a graph rather than a chain, so a host inherits the vars
  of *every* parent its group is declared under instead of whichever was walked
  last. A host or child named in both declarations is listed once.

- **A timed-out run is now stopped, not just abandoned.** `timeout_seconds`
  bounded how long unified-api *waited*, never how long the script *ran*: the
  timeout dropped the future, and a dropped child process keeps executing. A
  connector wedged on an unresponsive API therefore got a fresh copy spawned on
  every `sync_interval_seconds` and none of them ever exited — at a 300-second
  interval, twelve new live processes an hour, indefinitely, each still querying
  the source of truth this service exists to shield from exactly that.

  Connector, enricher and output processes are now killed when their run is
  abandoned. The SSH connector had the same problem in its own shape — one
  detached task per host, which a timeout could not reach — so a source-level
  timeout on a large fleet left thousands of SSH sessions still being opened for
  a result nobody would read; those are now aborted with the run.

  **Write connector scripts to be interruptible.** A partially written file or a
  half-finished remote change will not be cleaned up for you.

## [0.10.2] - 2026-07-31

### Fixed

- **Reading a file-backed credential no longer blocks the runtime.**
  `EnvSecrets` read `secret_file` with a blocking `std::fs` call from inside an
  async function, which parks a whole worker thread — with unrelated HTTP
  requests and sync tasks queued behind it — on every sync of every source using
  one. Small on local disk, less so on a mounted secrets volume. It now reads
  through `tokio::fs`.

- **A conditional request can no longer talk a view out of refusing.** A view
  minted its ETag and could answer `304 Not Modified` *before* routing the hosts
  the request named, so `If-None-Match: *` turned a host no member claims into
  "nothing changed" instead of the `404` that says the request cannot be routed
  at all. That is the silent-nothing the refusal exists to prevent, and the
  source routes never had it — they check the entry exists first. A view now
  routes before any validator is minted.

- **Changing a source's `ttl_seconds` now takes effect on an entry that already
  exists.** The TTL was applied only when a cache entry was *created*, so a
  source whose entry is patched in place rather than replaced — every
  `sync_mode: merge` source, and any entry seeded by a scoped sync — kept the
  TTL it was born with for the life of the process. Editing `ttl_seconds` did
  nothing, and with disk persistence the old value came back on every restart,
  so it never took effect at all. The TTL is configuration, and the configured
  value now wins on every sync.

- **A failed SSH command is no longer stored as the host's facts.** The channel
  read loop stopped at EOF, but the exit status arrives as a message of its own
  and the protocol fixes no order between the two — EOF means "no more output",
  the exit status means "the process finished", and a command that closes stdout
  before exiting produces them the other way round. The loop could therefore
  return before ever seeing a non-zero status.

  A command that failed was then treated as a successful gather whose output
  happens to be an error message, and `parse_host_output` stored that under
  `raw_output` / `parse_error` as the host's variables. The inventory gained a
  host described by a shell error instead of a host marked unreachable — which
  since 0.10.1 would have kept its last known good data. The loop now reads to
  channel close and judges the status afterwards. A server that reports no exit
  status at all is still treated as success, as before.

- **A talkative script no longer wedges its own run.** Enrichers and output
  endpoints were handed their input by writing the whole payload to the script's
  stdin *before* reading anything back. Pipe buffers hold 64 KiB, so any script
  that wrote past that to stdout or stderr before draining its stdin deadlocked:
  it blocked on a full output pipe, we blocked on a full input pipe, and nothing
  moved until `timeout_seconds` expired — 300 seconds by default, answering an
  endpoint caller with a 504 for a script that was working perfectly. A verbose
  script logging its progress was enough to trigger it, and the payloads are the
  largest in the process (a whole dataset for an enricher, every configured
  source for an endpoint).

  The input is now written on a task of its own, so the script's output is
  drained while it is being fed. A script that stops reading early is no longer
  an error either — it is a legitimate thing to do, and the broken pipe it
  causes was previously reported as a failure.

### Changed

- **Reading a view is no longer quadratic in the size of the inventory.** Every
  question a view read asks — which member owns this host, which hosts can I
  serve, whose data do I return — was answered by asking each member in turn,
  and a member answers by scanning its ownership group's host list. A whole-view
  read therefore did that scan once per host, then again for every host it
  served, so the cost grew with the square of the inventory on the read path
  consumers poll.

  A snapshot now resolves the ownership table once and answers from it. Measured
  on a two-member view (release build):

  | Hosts | `/dataset` routing before | after |
  |---|---|---|
  | 2 000 | ~9.7 ms | ~0.5 ms |
  | 4 000 | ~50 ms | ~1.8 ms |

  Growth is now roughly linear rather than quadratic — doubling the inventory
  roughly doubles the cost instead of quadrupling it.

  A read that names a **single host** still answers without building the table,
  so the common `?host=` query keeps costing microseconds rather than paying to
  index a datacenter it will not look at. Both paths resolve ownership by the
  same rule, in the same declared order, and a test pins them to the same answer.

## [0.10.1] - 2026-07-31

### Fixed

- **A `sync_mode: merge` source no longer ages forever.** A merge sync patches
  its cache entry in place instead of replacing it, and nothing renewed the
  dataset-level timestamp — that is set when the entry is *created* and was
  never touched again. So a merge-mode source kept the `fetched_at` of its very
  first sync for the life of the process: `dataset_age_seconds` grew without
  bound, and `dataset_is_fresh` went false one TTL after boot and stayed false,
  however faithfully the source had been syncing every interval since.

  An operator alerting on `unified_api_source_fresh` or
  `unified_api_source_age_seconds` therefore got a permanent alarm for a
  perfectly healthy source — and a view with a merge-mode member was reported
  stale for the same reason. A merge sync now restarts the dataset clock, which
  is the truth: merge preserves hosts upstream has stopped listing, and that is
  a statement about what the entry *contains*, not about when it was gathered.

  Per-host timestamps are unaffected — they were already being stamped by the
  merge itself.

- **A script enricher no longer resets how fresh a host looks.** 0.10.0 fixed
  this for the declarative merge, but the script path wrote through
  `merge_dataset` — which exists for data a connector *gathered*, and stamps the
  hosts it carries as collected now. So enriching a host still made it look
  freshly gathered, and a read arriving with `refresh=true` found nothing stale
  and did nothing: the consumer asked for current facts, got cached ones, and
  was told the refresh had succeeded.

  Enrichment derives from data already in the cache and gathers nothing, so it
  no longer touches the timestamps of hosts that are already there. A host an
  enricher *introduces* is still stamped — every host needs a timestamp, one
  without is absent from `/status` and never fresh, and "now" is when it became
  known. Removals and group merges are unchanged.

  **This will increase gathering load.** Hosts that looked fresh only because an
  enricher had touched them now read as stale, so `refresh=true` requests that
  have been quietly doing nothing will start doing real work. That is the bug
  being fixed — the consumer's explicit request was being dropped — and it stays
  bounded by `ttl_seconds` and `refresh_max_concurrent`, but it is worth knowing
  before you see SSH volume move.

- **A scoped sync no longer reports the whole source as freshly gathered.** A
  host- or group-scoped sync landing on a cold cache creates the entry — that is
  what lets a consumer ask a central for one host before any scheduled sync has
  run. It created it with the dataset-level clock started, so `/status` answered
  `dataset_age_seconds: 0` and `dataset_is_fresh: true`, and `GET /sources` said
  the same: a single host presented as a freshly gathered datacenter.

  The damage was to the one signal built to catch this. A source that has never
  completed a full sync reported `unified_api_source_fresh = 1`, so an alert on
  that gauge stayed quiet precisely when it had something to say.

  A cache entry now records whether a sync of the *whole* source has ever landed
  in it. Until one has, the dataset reads as not fresh — the dataset-level TTL
  measures a full gather, and there is none to measure. The per-host timestamps
  are untouched: those hosts really were gathered, and still say so. A later
  full sync clears the state, in either sync mode, and it survives a restart so
  a snapshot reload cannot launder a hosts-only entry into a complete one.

  Watch for this if you alert on `unified_api_source_fresh`, `is_fresh` or
  `dataset_is_fresh`: a source that only ever receives scoped syncs now reads
  as not fresh, which is the truth it was hiding before.

- **A source that takes its host list from another source no longer loses the
  race at boot.** Every source's first tick fires at once, so a source using
  `hosts_from_source` started syncing before the source it reads had any data.
  It failed with *"not in the cache yet — sync it first"* and then said nothing
  until its next interval: on an hourly source, an hour of a datacenter missing
  from the inventory for no reason but startup order.

  Such a source now waits for its dependency to have data before its first
  sync, up to five minutes. The wait is bounded on purpose — a dependency with
  no schedule of its own may never arrive, and a task that waits forever is a
  task that never reports why. When the budget runs out the sync proceeds and
  fails exactly as before, so the reason still lands in `sync_health`. Only the
  first sync waits; after boot, an absent dependency is a real failure and
  belongs in `sync_health` immediately.

- **A slow sync no longer triggers a burst of catch-up syncs.** Each scheduled
  loop awaits its work inline, so a run that outlasts its interval leaves ticks
  behind it — and tokio's default behaviour is to fire those back-to-back until
  the schedule has caught up. A sync that took an hour on a ten-minute interval
  was followed by five more with no pause between them, hammering the source at
  exactly the moment it was already struggling. Missed ticks are now skipped and
  the original schedule resumes, so a slow run costs the runs it displaced and
  nothing more. Applies to source syncs, enricher runs and project pulls alike.

- **A host that fails to answer keeps its groups, not just its variables.**
  0.10.0 stopped a `replace` sync from deleting hosts the connector could not
  reach, but it put back only their hostvars. Group membership is derived from
  the hosts a connector managed to gather, so the retained host landed in the
  dataset and in no group at all — and an Ansible inventory is groups. A
  consumer running `hosts: oracle_version` still saw the host vanish on every
  sync that missed it, which is the disappearance the retention was written to
  prevent.

  A retained host now comes back with its whole previous state: variables, its
  true age, and every group it belonged to. A group that vanished entirely is
  recreated, because a group disappears from a gather exactly when every host
  in it failed to answer — dropping it would take the retained hosts with it.
  A host upstream has stopped listing is still removed, groups included: it is
  never attempted, so it is never retained.

### Changed

- **A script enricher no longer copies the whole dataset to read it.**
  `EnricherPort::execute` took the dataset by reference, and the returned future
  has to own what it reads, so the adapter had no choice but to deep-copy it —
  on a facts source that is megabytes of nested maps duplicated on every run, of
  every enricher, on every interval. The port now takes the `Arc<Dataset>` the
  cache already holds, so the enricher reads the cached dataset itself and the
  copy disappears. Same signature as `OutputPort`, which never had the problem.

## [0.10.0] - 2026-07-30

### Fixed

- **Swagger UI no longer freezes on an enterprise-sized response.** Pagination
  (0.4.0) only ever helped the caller who remembered to ask for it, and it
  addressed the wrong half of the problem: the server was answering in
  milliseconds and the browser was dying afterwards. Swagger renders a
  response through highlight.js, which wraps every token in its own DOM
  element — a 2000-host dataset is ~10MB of JSON and millions of elements, so
  the tab locks up. Syntax highlighting is now disabled in the UI config, and
  the body renders as plain text.

  On top of that, the `limit` parameter of `GET /sources/{id}/dataset` carries
  an example of `50`, which is what Swagger prefills into the input box — so
  pressing *Execute* asks for a page instead of a whole datacenter. It is an
  example, not a server-side default: a client that omits `limit` still gets
  the raw, unpaginated Dataset, and clearing the field in the UI does the
  same. Routes whose body is script-defined (output endpoints) cannot
  paginate at all, which is why the fix had to be in the UI config and not
  only in the query string.
- **Enrichment no longer loses keys when several enrichers share a target.**
  A declarative enricher wrote the whole host map back — a clone of everything
  it had read, plus its own field — and `merge_dataset` replaced the map with
  it. Each enricher runs in its own task, so two of them on one target raced:
  both cloned the host, both wrote, and whichever committed last erased the
  other's key. It only stayed invisible because the intervals were long enough
  to rarely overlap.

  An enricher now writes only the keys it owns, and those keys are merged into
  the host rather than replacing it, so the work is additive and the order of
  two enrichers no longer decides what survives.

- **Enrichment no longer resets how fresh a host looks.** The merge stamped
  `host_timestamps`, the same timestamps a read consults to decide whether to
  refresh before answering. Enriching a host therefore made it look freshly
  gathered and could suppress a refresh the consumer had asked for. Derived
  data has nothing to say about when a host was last gathered, so it no longer
  touches those timestamps.

- **A sync no longer leaves its target un-enriched.** Enrichers ran only on
  their own timer, while a sync replaces what it writes — so every refresh of
  a target dropped the derived keys until the next enricher tick, up to a full
  interval later. Consumers saw the keys appear and disappear. `sync_source`
  now re-applies the enrichers that target the source it just wrote, at the
  one place every sync in the process passes through, so no caller can forget.
  The enricher's own interval remains as the backstop for the write paths that
  do not go through it.

- **A host that fails to answer is no longer dropped from the inventory.** The
  SSH connector gathers through a bounded worker pool, and a host that misses
  its connect timeout was simply absent from the dataset it returned. A full
  sync in `replace` mode then swapped the entry wholesale, so one saturated
  batch of workers took every host in it out of the inventory until the next
  run — a healthy server would come and go on the sync interval, and consumers
  saw it disappear for no reason of its own.

  The connector already knew which hosts had failed; it named them in its
  summary log and then discarded the list. It now reports them, and a replace
  keeps their previous data instead of deleting it.

  A host that upstream has stopped listing is never attempted, so it never
  appears in that list and is still removed — which is what tells a
  decommissioned host from one that merely did not answer. Retained hosts keep
  the age they already had rather than being stamped fresh, so the TTL still
  expires them and a refresh still targets them: the data is last-known-good
  and says so, instead of looking current.

### Added

- **`GET /api/v1/enrichers`** lists the configured enrichers with their
  target, source, fields and whether the target is in the cache yet. Sources,
  endpoints and projects could all be listed; enrichers could only be run, so
  the only way to find out whether one was loaded was to try it.

### Changed

- Enrichers that share a target are applied in a stable order, sorted by id.
  Additive merging makes concurrent writes safe, not meaningful: if two ever
  claim the same key on the same host, the winner should be a documented rule
  rather than whichever task finished first — the same reasoning as a view's
  member order.

- `sync_source` and `refresh_hosts` take the enrichment dependencies as one
  optional borrowed parameter. `None` keeps the previous behaviour for a
  caller with no enrichers configured.

## [0.9.0] - 2026-07-30

### Added

- **Views** (`views.yaml`): a read-only composite that presents several sources
  as one id, routes a per-host read to whichever member *owns* that host, and
  delegates an on-demand refresh to that member. It gathers nothing itself.

  Federation solved this at one end and not the other. `connector_type: remote`
  means a central needs no credentials and no SSH path into a datacenter,
  because the edge that owns the hosts does the gathering — but the *consumer*
  still had to know the topology, since the facts of a datacenter B host lived
  under a different source id than those of a datacenter A host. Every consumer
  learned the split and relearned it whenever an edge was added. A view is one
  address for "the facts".

  It answers on the **source routes**, in the same shapes — `/dataset`,
  `/status`, `/groups`, `/hosts` — and shares the source id space, so migrating
  a consumer is a one-word change to an id and its parsing is untouched. Views
  appear in `GET /sources` with `kind: "view"`.

  **Ownership is declared, not inferred from what a member has cached.** The
  obvious implementation is wrong in two ways, both found in production: facts
  sources are often synced daily on purpose (the bulk is a floor, freshness
  comes from on-demand), so a host provisioned this morning is in no cache
  until tomorrow; and some hosts — appliances that take no SSH — never enter
  any cache at all. Both are exactly the hosts on-demand refresh exists for.
  Declared ownership is also the only rule that works for both member kinds: a
  `remote` member has no `hosts_from_source` at the central, so there is
  nothing else to ask about what it owns.

  A host **no** member claims answers `404` naming the fact, never a silent
  empty result and never a default member — a default member turns a config
  error into empty data nobody investigates. A host that *is* claimed but whose
  owner has no data for it routes normally, so a refresh can go and get it.

- `ttl_seconds` on a view: its own freshness policy, which is also the **gate**
  for `refresh=true` (a read only gathers hosts older than the TTL). Absent =
  each host inherits its owning member's TTL. A member's per-host and per-group
  `ttl_overrides` win either way, so a view cannot silently cancel the
  five-minute TTL somebody put on a critical host.

- `members` on `GET /sources/{view}/status`: per member, whether its data is
  cached, whether the source its ownership resolves against is cached, its age,
  TTL, host count and sync health. The second flag is the one that distinguishes
  "this member has no data" from "the routing table has not loaded", which are
  the two ways a view answers nothing.

- `kind` on `GET /sources` entries (`"source"` or `"view"`), and
  `unified_api_view_unclaimed_hosts_total{view}` so a routing gap shows on a
  dashboard before somebody reports it.

- `docs/views.md`.

### Changed

- Restricted API keys may name a view id under `sources:`. A key granted the
  view needs **no** access to the members: the view is the contract, the
  members are internal topology.

- The write routes refuse a view id with `400` and a body naming the members —
  `POST /sync`, `DELETE /sources/{id}`, and host `PUT`/`DELETE`. A view gathers
  nothing and holds no cache entry, and the tempting reading of a view sync
  ("sync every member") would let a request aimed at one consumer's view
  quietly re-gather somebody else's datacenter. Endpoints and enrichers
  pointed at a view fail startup for the same reason, with a message that says
  which members to target instead.

- Unknown keys inside a `views.yaml` entry are a hard startup error, unlike the
  rest of the config. Ownership is the routing table: `grups:` instead of
  `groups:` would otherwise parse as an empty pattern, and an empty pattern
  claims everything.

## [0.8.0] - 2026-07-30

### Added

- `refresh=true` on `GET /sources/{id}/dataset`: bring the requested hosts up to
  date before answering, so a consumer that can only fetch a URL (a form, a
  dashboard) gets current facts without knowing the topology or issuing a write.
  It requires `?host=` — a whole-source refresh triggered by opening a page
  would gather the entire inventory, so the hosts have to be named — and the
  source must carry the new `allow_on_demand_refresh: true`, off by default,
  because a read that can cause SSH into a datacenter is a capability rather
  than a convenience.

  **The caller does not get a freshness knob.** How stale is too stale is the
  source's `ttl_seconds` (and its `ttl_overrides`), which the operator writes;
  `refresh=true` says only "I would rather wait than be served stale data". Any
  consumer-supplied staleness bound would be a consumer-supplied load knob, and
  the load lands on somebody's datacenter. What that buys is a load ceiling from
  arithmetic rather than trust: a host is re-gathered at most once per TTL
  window however many consumers ask for it, so the worst case for a source is
  the load of setting `sync_interval_seconds` equal to `ttl_seconds`, and in
  practice far less, since only the hosts somebody looks at are refreshed at
  all. Note this makes `ttl_seconds` load bearing where it used to be purely
  informational, so it is worth a look before enabling the flag on a source.

  Two limits sit under that one, for what the TTL window does not cover.
  Concurrent requests for the same host would each start a gather before the
  first finished, so they queue on a per-host lock and the late ones re-check
  freshness rather than gathering again (per host, not per source: a
  source-wide lock would make everyone wait behind a refresh of an unreachable
  host). Requests for many *different* hosts are all first in their window, so
  the TTL does not bound them at all and `server.refresh_max_concurrent`
  (default 8) does.

  A refresh that fails or outlasts `server.refresh_timeout_seconds` (default 15)
  never fails the read: the cached data is served and
  `x-unified-api-refreshed: false` plus `x-unified-api-refresh-error` say not to
  trust it as current. On success, `x-unified-api-refreshed-hosts` names what was
  re-gathered. The information travels in headers so neither response shape
  changes: a consumer adding `&refresh=true` to a call it already makes keeps
  parsing exactly what it parsed before. `unified_api_refresh_total{source,
  result}` counts the outcomes, so "how much of my gathering load comes from
  consumers?" is answerable.

- `refresh_origin=true` on `POST /sources/{id}/sync`: make a federated source's
  origin re-gather before answering, instead of handing over whatever it has
  cached. A central holding an edge's data cannot produce newer facts by
  itself — only the instance with the SSH path to the host can — so until now
  the only way to get current data through a mesh was to call the edge
  directly, which means the consumer has to know the topology and hold a
  credential per datacenter. The intent travels down the chain and recurses
  (edge → region → global), bounded by `refresh_depth` (default 3), so a
  topology accidentally wired into a cycle stops instead of amplifying. It
  pairs with the host scope: `?host=X&refresh_origin=true` re-gathers that
  host and nothing else. A local source accepts and ignores the flag — its
  sync already gathers fresh data — so consumers do not have to know whether
  the source id they were given is local or federated. An origin that cannot
  re-gather fails the sync naming its own error, rather than quietly returning
  older data as a success.
- `server.readyz_require_all_sources` (default `false`): make `/readyz` turn
  green only once every configured source has synced. The default stays "ready
  when at least one source has synced", under which a deployment with ten
  sources reports ready with nine of them broken — fine when a partial
  inventory beats none, wrong when a job template would then run against half
  a datacenter.
- `server.metrics_require_auth` (default `false`): require an API key on
  `GET /metrics`. The endpoint is public by default because that is what a
  Prometheus scrape config expects, but its exposition labels every source id
  and host count — a description of the inventory topology available to
  anything that can reach the port. `/healthz` and `/readyz` stay public
  either way; with no API keys configured the flag has no effect, since
  authentication is off entirely.

### Changed

- `?host=` on `POST /sources/{id}/sync` accepts a comma-separated list, like
  every other `?host=` in the API. Previously the whole value was treated as
  one hostname: `?host=a,b` gathered everything and then cached nothing,
  because no host is named "a,b". A consumer refreshing the five hosts a form
  displays now pays for one gather instead of five. Connector scripts receive
  the list verbatim in `target` and should split it on commas (both sample
  scripts under `tests/` show the shape); a value that names no host at all
  falls back to a full sync rather than gathering and discarding.

### Fixed

- A host-scoped sync now actually gathers only that host. The scope reached
  every connector as `scope`/`target` in its config, but two of them ignored
  it: the SSH connector read only its host list, so
  `POST /sync?host=one-box` opened a session to every host in the datacenter
  and kept one; and the remote (federation) connector fetched the whole remote
  dataset across the WAN — megabytes on a facts source — to keep one host. The
  SSH connector now narrows its host list to the target (a target it does not
  have is an error naming it, not a bland success), and the remote connector
  translates the scope into `?host=` on both remote calls. Group scope is
  unchanged: a `HostSpec` carries addresses, not group membership, so the SSH
  connector has nothing to narrow by.
- A host-scoped federated sync no longer resets that host's age. Origin ages
  were applied only on the full-dataset path (`CacheEntry::restore`), so a
  per-host pull through a central stamped the host "now" and reported
  six-hour-old facts as fresh — the exact lie federation exists to avoid.
  Host timestamps are now backdated by the age the origin reported
  (`CacheEntry::update_host_aged`), including on the entry's first write.

## [0.7.0] - 2026-07-29

### Added

- `GET /api/v1/endpoints/{id}` alongside the existing `POST`: query parameters
  become the endpoint's dynamic parameters. Rendering an inventory is a read,
  and POST-only shut out consumers that can only fetch a URL (browsers, proxy
  caches, tools pointed at an inventory URL). A query string carries no types,
  so parameters arrive as strings; `POST` is still the way to pass numbers,
  booleans or nested structures.
- `ETag` / `If-None-Match` on filtered and paginated `/dataset` responses, not
  just the plain path. A consumer polling one slice (`?group=linux` every few
  minutes) now gets `304` while nothing changes instead of re-transferring it.
  The validator combines the cache's write counter with the query parameters,
  so it is invalidated by any write — including a sync of an unrelated source
  — and does not survive a restart. Never stale, occasionally redundant; the
  plain path keeps its content-derived, restart-stable ETag.

## [0.6.0] - 2026-07-28

### Added

- `GET /api/v1/sources/{id}/groups`: group names with host counts, children
  and whether they carry group vars — no hostvars. Auto-groups derive their
  names from fact keys, so the group set is data-dependent and cannot be read
  off the config; discovering it previously meant fetching the whole dataset
  (~11 MB on an SSH source).
- `GET /api/v1/sources/{id}/hosts`: the hostnames only, sorted. The cheap
  answer to "what is in this source" for UIs and operators, which previously
  required the full dataset or passing a deliberately non-existent `?fields=`
  value to empty out the vars.
- `DELETE /api/v1/sources/{id}`: drop a source's cache entry without
  restarting, reporting how many hosts went with it. Removing a source from
  `sources.yaml` previously left its entry served — and re-written into every
  snapshot — until the process restarted. Cached data only: a source still in
  config is refilled by its next sync.

## [0.5.0] - 2026-07-27

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

### Changed

- Error responses carry a JSON body (`{"error": "..."}`) instead of an empty
  one. The source, sync, host, enricher and project routes answered `403`/`404`
  with no body at all, so a consumer could not tell "source is not in the
  cache" from "source is not configured", or a missing source from a missing
  host on the same `DELETE`. The messages now name the id and the reason, and
  the status codes are unchanged. Output endpoints already answered this way;
  the rest of the API now agrees. Registered as `ErrorBody` in the OpenAPI
  spec, so Swagger documents the shape.

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

[Unreleased]: https://github.com/OpusProjects/unified-api/compare/v0.29.0...HEAD
[0.29.0]: https://github.com/OpusProjects/unified-api/compare/v0.28.0...v0.29.0
[0.28.0]: https://github.com/OpusProjects/unified-api/compare/v0.27.0...v0.28.0
[0.27.0]: https://github.com/OpusProjects/unified-api/compare/v0.26.0...v0.27.0
[0.26.0]: https://github.com/OpusProjects/unified-api/compare/v0.25.0...v0.26.0
[0.25.0]: https://github.com/OpusProjects/unified-api/compare/v0.24.1...v0.25.0
[0.24.1]: https://github.com/OpusProjects/unified-api/compare/v0.24.0...v0.24.1
[0.24.0]: https://github.com/OpusProjects/unified-api/compare/v0.23.0...v0.24.0
[0.23.0]: https://github.com/OpusProjects/unified-api/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/OpusProjects/unified-api/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/OpusProjects/unified-api/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/OpusProjects/unified-api/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/OpusProjects/unified-api/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/OpusProjects/unified-api/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/OpusProjects/unified-api/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/OpusProjects/unified-api/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/OpusProjects/unified-api/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/OpusProjects/unified-api/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/OpusProjects/unified-api/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/OpusProjects/unified-api/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/OpusProjects/unified-api/compare/v0.10.3...v0.11.0
[0.10.3]: https://github.com/OpusProjects/unified-api/compare/v0.10.2...v0.10.3
[0.10.2]: https://github.com/OpusProjects/unified-api/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/OpusProjects/unified-api/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/OpusProjects/unified-api/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/OpusProjects/unified-api/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/OpusProjects/unified-api/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/OpusProjects/unified-api/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/OpusProjects/unified-api/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/OpusProjects/unified-api/compare/v0.4.0...v0.5.0
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
