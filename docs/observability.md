# Observability

What the service says about itself: when it works, what it logs, and the metrics
to alert on.

For what the *data* looks like rather than the process, see
[caching & TTLs](caching.md); for the health probes as Kubernetes consumes them,
see [deployment](deployment.md).

## Scheduling behavior

Background sync tasks start at boot for every source with
`sync_interval_seconds > 0` (tokio `interval`, first tick immediately). Enrichers
with an interval likewise. A failed run logs the error and waits for the next tick —
there is no retry/backoff beyond the interval itself. Every script execution is
bounded by its `timeout_seconds` (default 300), so a hung connector or enricher
cannot wedge its scheduler task. Exceeding it **kills** the process rather than
abandoning it, so a wedged script does not leave a live copy behind on every
tick; the SSH connector likewise aborts the per-host gathers still in flight.

A run that outlasts its own interval **skips** the ticks it missed and resumes on
the original schedule, rather than firing them back to back to catch up. A sync
that took an hour on a ten-minute interval therefore costs the runs it displaced
and nothing more — it does not come back as five immediate syncs against a source
that is evidently already struggling. The same applies to enricher runs and project
pulls. A source using `hosts_from_source` additionally waits, once, for the source
it reads to have data before its first sync (up to five minutes) — see
[connectors](connectors.md#dynamic-host-lists-hosts_from_source).

Shutdown is graceful for
in-flight HTTP requests (SIGTERM/Ctrl-C); scheduler tasks stop with the process.

## Logs and metrics

Structured logs via `tracing` to stdout; tune with `RUST_LOG` (e.g.
`unified_api=debug`). Every HTTP request is logged at INFO with method, path,
status and latency (a `tower-http` trace layer); set `tower_http=debug` for more
detail. Sync and enrich outcomes are logged with source ids, host counts and
durations.

**Prometheus metrics** are exposed at `GET /metrics` (public, like the health
probes — scrapers don't carry the API key):

| Metric | Labels | Meaning |
|---|---|---|
| `unified_api_sync_total` | `source`, `result` | Sync runs, success vs error |
| `unified_api_sync_duration_seconds` | `source` | Sync duration histogram |
| `unified_api_enrich_total` | `source`, `result` | Enricher runs |
| `unified_api_enrich_duration_seconds` | `source` | Enricher duration histogram |
| `unified_api_endpoint_total` | `endpoint`, `result` | Output endpoint runs |
| `unified_api_endpoint_duration_seconds` | `endpoint` | Endpoint duration histogram |
| `unified_api_source_cached` | `source` | 1 if the configured source has a cache entry, 0 if it has never synced |
| `unified_api_source_age_seconds` | `source` | Seconds since the dataset was last fetched |
| `unified_api_source_ttl_seconds` | `source` | The source's dataset TTL |
| `unified_api_source_fresh` | `source` | 1 while the dataset is within its TTL, 0 once expired — and 0 for a source that has only ever received host- or group-scoped syncs, which have no full gather for the TTL to be measured against |
| `unified_api_source_hosts` | `source` | Hosts in the cached dataset |
| `unified_api_source_groups` | `source` | Groups in the cached dataset |
| `unified_api_view_fresh` | `view` | 1 while every member is cached and inside its TTL |
| `unified_api_view_age_seconds` | `view` | Age of the **stalest** member — a view is no more current than the least current thing it serves |
| `unified_api_view_ttl_seconds` | `view` | The view's declared TTL, or the loosest member's |
| `unified_api_view_hosts` | `view` | Hosts the view can actually serve |
| `unified_api_view_members_total` | `view` | Members declared |
| `unified_api_view_members_cached` | `view` | Members that have data. Short of `_total` = the view is serving part of its inventory |
| `unified_api_view_members_routable` | `view` | Members whose *ownership* source is cached. Short of `_total` = those members claim nothing beyond literally named hosts, so the view 404s hosts that plainly exist |
| `unified_api_enricher_consecutive_failures` | `enricher` | Failed runs since this enricher last succeeded — 0 while healthy. A target that is not in the cache counts as a failure: the enricher is not doing its job either way |
| `unified_api_enricher_last_success_age_seconds` | `enricher` | Seconds since this enricher last ran successfully |
| `unified_api_project_sync_consecutive_failures` | `project` | Failed git pulls since this project's checkout last updated |
| `unified_api_project_sync_last_success_age_seconds` | `project` | Seconds since the checkout last updated |
| `unified_api_snapshot_consecutive_failures` | — | Failed cache snapshot writes since one last succeeded (a full disk, revoked permissions, a vanished volume) |
| `unified_api_snapshot_last_success_age_seconds` | — | Seconds since a snapshot was last written |

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

The `unified_api_source_*` gauges are read from the cache on every scrape
rather than pushed on sync, so age keeps growing while a source is not
syncing — the case worth alerting on. A source whose scheduler task died
produces no errors at all, so the error rate alone will not catch it:

```yaml
# Data older than three sync intervals, or never synced at all
- alert: UnifiedApiSourceStale
  expr: unified_api_source_fresh == 0 or unified_api_source_cached == 0
  for: 15m
```

The gauges are labeled per source, so a source removed from both config and
cache keeps its last value until the process restarts.
