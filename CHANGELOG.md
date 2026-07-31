# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
  still had to know the topology, since the facts of a DC4 host lived under a
  different source id than those of an aa1 host. Every consumer learned the
  split and relearned it whenever an edge was added. A view is one address for
  "the facts".

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

[Unreleased]: https://github.com/OpusProjects/unified-api/compare/v0.10.1...HEAD
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
