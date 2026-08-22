# Observability

What the service says about itself: when it works, what it logs, and the metrics
to alert on.

For what the *data* looks like rather than the process, see
[caching & TTLs](caching.md); for the health probes as Kubernetes consumes them,
see [deployment](deployment.md).

- [Scheduling behavior](#scheduling-behavior)
- [Logs and metrics](#logs-and-metrics)

---

## Scheduling behavior

Background sync tasks start for every source with
`sync_interval_seconds > 0` (tokio `interval`). Enrichers and project pulls
with an interval likewise. They start once the boot project clones have had
their bounded chance (concurrent, each capped by the project's
`timeout_seconds`) — and all of that happens **behind the listener**, so
`/healthz` answers while clones are still running and an unreachable git
remote can no longer fail a startup probe. Every script execution is
bounded by its `timeout_seconds` (default 300), so a hung connector or enricher
cannot wedge its scheduler task. Exceeding it **kills** the process rather than
abandoning it, so a wedged script does not leave a live copy behind on every
tick; the SSH connector likewise aborts the per-host gathers still in flight.

Sources, enrichers and project pulls may pace themselves by **cron schedule**
instead of an interval (`schedule`, standard 5-field cron evaluated in UTC). A
cron task fires at its exact times — no jitter, since "02:30" was chosen by a
person — and a failing one backs off by letting occurrences pass, exactly like
the interval backoff below. Two tasks sharing an expression will fire
together; that is the operator's explicit choice.

Each interval task's schedule is shifted by a small **deterministic jitter** (a hash of
its id, capped at 30 seconds and at the interval itself), so every source does
not gather at the same instant at boot — and, since intervals keep their
phase, does not collide again at every common multiple forever. Deterministic
on purpose: the same config produces the same spread on every boot.

A **failing task backs off** instead of hammering: after a failure the next
attempt comes 1 interval later, then 2, 4, and at most 8, resetting on the
first success. The ticker keeps its cadence and the backoff just lets ticks
pass, so attempts stay aligned to the configured schedule. `sync_health`
carries the failure streak the whole time — note that during backoff
`last_attempt_age_seconds` legitimately grows to up to 8 intervals, which is
why the alert examples below key on `consecutive_failures` for "it is
failing" and reserve the attempt age for "nothing is even trying".

Every periodic task runs under a **supervisor**: a panic in the task body is
counted in `unified_api_scheduler_task_panics_total`, logged, and the task is
restarted after one interval — instead of the tokio default, where the task
dies silently and that source simply stops syncing until someone notices the
data went stale.

A run that outlasts its own interval **skips** the ticks it missed and resumes on
the original schedule, rather than firing them back to back to catch up. A sync
that took an hour on a ten-minute interval therefore costs the runs it displaced
and nothing more — it does not come back as five immediate syncs against a source
that is evidently already struggling. The same applies to enricher runs and project
pulls. A source using `hosts_from_source` additionally waits, once, for the source
it reads to have data before its first sync (up to five minutes) — see
[connectors](connectors.md#dynamic-host-lists-hosts_from_source).

Shutdown (SIGTERM/Ctrl-C) is graceful end to end: in-flight HTTP requests
drain first, then every background task is signalled and given up to
`server.shutdown_grace_seconds` (default 20) to finish its in-flight run — a
sync mid-gather completes and lands in the cache, it is never cut mid-write —
and only then is the final cache snapshot written. That ordering is what makes
the final snapshot consistent: nothing is still mutating the cache while it
serializes, and the periodic snapshot task has already stopped, so it cannot
race the final save on the same temp file. A task that outlives the grace is
logged and the snapshot proceeds anyway (best effort beats a SIGKILL with no
snapshot at all).

---

## Logs and metrics

Structured logs via `tracing` to stdout; tune with `RUST_LOG` (e.g.
`unified_api=debug`). Every HTTP request is logged at INFO with method, path,
status and latency (a `tower-http` trace layer); set `tower_http=debug` for more
detail. Sync and enrich outcomes are logged with source ids, host counts and
durations.

Every request also carries a `request_id`: taken from the client's
`x-request-id` header when one is sent (so a consumer can stitch these lines
into its own trace), assigned from a per-process counter otherwise, and echoed
on the response either way — an error report quoting the id finds its exact
log lines. Authenticated requests additionally log the API key's `key_name`,
so an access-log line answers *who* did what; the field stays empty on public
routes and on an API running open, where absence means nobody authenticated.

**Mutating operations additionally emit an audit event** under the dedicated
`audit` tracing target — one line per operation that actually ran, with
`actor` (the key name, or `open` on an unauthenticated deployment), `action`
(`sync`, `evict`, `host_put`, `host_delete`, `enricher_run`, `project_sync`,
`config_write`, `config_write_reload`, `config_reload`),
`resource`, `request_id` and `outcome` (`success`/`error` — a sync answers
HTTP 200 either way, so the status alone cannot say). Denied attempts (401/403)
are deliberately absent: they return before anything happens, and the access
log line with the same `key_name` and `request_id` already records the attempt.
The target makes the trail separable from the rest of the logs — keep it while
quieting everything else with `RUST_LOG=warn,audit=info`, or route it in the
log pipeline by the `audit` target field.

**Prometheus metrics** are exposed at `GET /metrics` (public, like the health
probes — scrapers don't carry the API key):

| Metric | Labels | Meaning |
|---|---|---|
| `unified_api_http_requests_total` | `method`, `path`, `status` | Every HTTP request, labeled by the **matched route pattern** (`/api/v1/sources/{id}/dataset`), never the raw URL — one series per route, not per host. Unrouted requests share `path="unmatched"` |
| `unified_api_http_request_duration_seconds` | `method`, `path` | Request latency histogram — handler time, measured inside the gzip layer |
| `unified_api_sync_total` | `source`, `result` | Sync runs: `success`, `error`, or `coalesced` (answered by a concurrent full sync's result — no gather ran) |
| `unified_api_remote_not_modified_total` | `url`, `source` | Federation pulls answered `304 Not Modified` — the transfer and re-parse were skipped, the sync still succeeded (see [connectors → remote](connectors.md#remote-sources--federation-connector_type-remote)) |
| `unified_api_sync_duration_seconds` | `source` | Sync duration histogram |
| `unified_api_refresh_total` | `source`, `result` | On-demand refreshes triggered by reads (`?refresh=true`): `fresh` (the cache answered, no gather ran), `coalesced`, `refreshed`, `failed` or `timeout` — how much gathering load comes from consumers rather than the scheduler (see [on-demand refresh](on-demand-refresh.md)) |
| `unified_api_enrich_total` | `source`, `result` | Enricher runs |
| `unified_api_enrich_duration_seconds` | `source` | Enricher duration histogram |
| `unified_api_endpoint_total` | `endpoint`, `result` | Output endpoint runs |
| `unified_api_config_writes_total` | `outcome` | Configuration writes over the API: `success`, `rejected` (did not validate — nothing was written) or `error` (could not be written). See [Configuration API](config-api.md) |
| `unified_api_config_reloads_total` | `outcome` | Configuration reloads: `success` or `invalid` |
| `unified_api_config_generation` | — | Applied configuration reloads since this process started, 0 at boot — the pod whose generation lags a fleet-wide push is one query away |
| `unified_api_config_restart_required` | — | Restart-only `config.yaml` keys the last applied reload changed. Non-zero = this pod runs on a configuration it could only partially adopt, and only a restart clears it; `GET /api/v1/config` names the keys |
| `unified_api_build_info` | `version` | Always 1; the label carries the running version, so any series can be joined onto it |
| `unified_api_endpoint_duration_seconds` | `endpoint` | Endpoint duration histogram |
| `unified_api_source_cached` | `source` | 1 if the configured source has a cache entry, 0 if it has never synced |
| `unified_api_source_age_seconds` | `source` | Seconds since the dataset was last fetched |
| `unified_api_source_ttl_seconds` | `source` | The source's dataset TTL |
| `unified_api_source_fresh` | `source` | 1 while the dataset is within its TTL, 0 once expired — and 0 for a source that has only ever received host- or group-scoped syncs, which have no full gather for the TTL to be measured against |
| `unified_api_source_hosts` | `source` | Hosts in the cached dataset |
| `unified_api_source_groups` | `source` | Groups in the cached dataset |
| `unified_api_source_sync_consecutive_failures` | `source` | Failed syncs since the last success — 0 while healthy. Appears once the source has attempted a sync in this process |
| `unified_api_source_sync_last_attempt_age_seconds` | `source` | Seconds since a sync was last **attempted**, successful or not. Growing past the sync interval = the scheduler task is not running at all |
| `unified_api_source_sync_last_success_age_seconds` | `source` | Seconds since the last successful sync. Absent until one has succeeded |
| `unified_api_view_fresh` | `view` | 1 while every member is cached and inside its TTL |
| `unified_api_view_age_seconds` | `view` | Age of the **stalest** member — a view is no more current than the least current thing it serves |
| `unified_api_view_ttl_seconds` | `view` | The view's declared TTL, or the loosest member's |
| `unified_api_view_hosts` | `view` | Hosts the view can actually serve |
| `unified_api_view_members_total` | `view` | Members declared |
| `unified_api_view_members_cached` | `view` | Members that have data. Short of `_total` = the view is serving part of its inventory |
| `unified_api_view_members_routable` | `view` | Members whose *ownership* source is cached. Short of `_total` = those members claim nothing beyond literally named hosts, so the view 404s hosts that plainly exist |
| `unified_api_view_unclaimed_hosts_total` | `view` | Hosts requested through a view that no member claimed — the request was refused naming them. Non-zero = ownership is not declared where consumers think it is |
| `unified_api_enricher_consecutive_failures` | `enricher` | Failed runs since this enricher last succeeded — 0 while healthy. A target that is not in the cache counts as a failure: the enricher is not doing its job either way |
| `unified_api_enricher_last_success_age_seconds` | `enricher` | Seconds since this enricher last ran successfully |
| `unified_api_project_sync_consecutive_failures` | `project` | Failed git pulls since this project's checkout last updated |
| `unified_api_project_sync_last_success_age_seconds` | `project` | Seconds since the checkout last updated |
| `unified_api_snapshot_consecutive_failures` | — | Failed cache snapshot writes since one last succeeded (a full disk, revoked permissions, a vanished volume) |
| `unified_api_snapshot_last_success_age_seconds` | — | Seconds since a snapshot was last written |
| `unified_api_scheduler_task_panics_total` | `task` | Panics caught and restarted by the task supervisor (`sync:<id>`, `enrich:<id>`, `project:<id>`). Any non-zero value is a bug worth reporting |

A view holds no cache entry of its own, so it has none of the `unified_api_source_*`
series — its members do. The names are separate rather than reusing the source
ones with a view id, because a view's hosts *are* its members' hosts: folding
them into one series would double-count every host in any sum across the label.

`_members_routable` is the one with no equivalent elsewhere. Every member can be
cached and fresh while the inventory source their ownership resolves against has
never synced — the view then claims nothing, serves an empty dataset, and looks
healthy in every other number.

Timed-out and failed runs count as `result="error"`, so alerting on the error
rate catches hung connectors too.

Every `_duration_seconds` histogram exports real buckets (`_bucket` series
with `le` edges from 5 ms up to the 300-second script timeout) rather than
client-side summary quantiles. That is what makes latency aggregatable across
instances — an average of per-pod p99s is not a fleet p99, but bucket sums are:

```promql
histogram_quantile(0.99,
  sum by (le, path) (rate(unified_api_http_request_duration_seconds_bucket[5m])))
```

The `_consecutive_failures` / `_last_success_age_seconds` pairs are read from
the health registries at scrape time, like the freshness gauges. A series
appears once the job has run at least once in this process — a
configured-but-never-run enricher has no series yet, exactly as a never-synced
source has no dataset gauges. For the snapshot task, alert on
`unified_api_snapshot_consecutive_failures` rather than the success age: an
idle cache **skips** its snapshots on purpose, so the age since the last write
grows while nothing is wrong.

```yaml
# Persistence has been failing for three straight intervals
- alert: UnifiedApiSnapshotFailing
  expr: unified_api_snapshot_consecutive_failures >= 3
  for: 5m
```

The `unified_api_source_*` gauges are read from the cache and the sync-health
registry on every scrape rather than pushed on sync, so the ages keep growing
while a source is not syncing — the case worth alerting on.

The sync-health gauges mirror the `sync_health` block on `GET /sources` and
`/status` (only `last_error` stays API-only: an error string as a label value
is unbounded cardinality). They exist because the freshness gauges alone made
a failing connector something to *infer*: `unified_api_source_fresh` only
drops once the whole TTL has run out, so a source failing for two hours on a
six-hour TTL still read as healthy. Alert on the failure streak directly, and
on the attempt age for the failure mode that produces no errors at all — a
scheduler task that has stopped running pushes nothing, and only a
clock-driven gauge can show its silence:

```yaml
# The connector has been failing for at least three attempts in a row
- alert: UnifiedApiSourceSyncFailing
  expr: unified_api_source_sync_consecutive_failures >= 3
  for: 5m

# Nothing is even trying. Past 8 intervals (here: 10-minute ones) not even
# a fully backed-off failing source stays this quiet — the task is gone.
- alert: UnifiedApiSourceSyncSilent
  expr: unified_api_source_sync_last_attempt_age_seconds > 5400
  for: 5m

# The backstop on the data itself: older than its TTL, or never synced at all
- alert: UnifiedApiSourceStale
  expr: unified_api_source_fresh == 0 or unified_api_source_cached == 0
  for: 15m
```

The gauges are labeled per source, so a source removed from both config and
cache keeps its last value until the process restarts.
