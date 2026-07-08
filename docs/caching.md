# Caching & TTLs

The cache is the heart of the service: consumers read from it, syncs and enrichers
write to it. It is in-memory (a concurrent DashMap keyed by source id) — by default
a restart starts empty, repopulated by scheduled syncs. Optional disk persistence
(below) changes that to "starts from the last snapshot".

## The three-level freshness model

Each cached source is a `CacheEntry`: the Dataset plus timestamps.

| Level | Tracked how | Fresh when |
|---|---|---|
| Dataset | `fetched_at` set on full sync | `age < ttl_seconds` |
| Host | one timestamp per host, refreshed whenever that host is written (host-scoped sync, merge, enricher, `PUT /hosts`) | `age < effective TTL` |
| Group | derived — a group is as fresh as its member hosts | — |

The **effective TTL** for a host is resolved in order: a `ttl_overrides.hosts` entry
for that hostname → a `ttl_overrides.groups` entry for a group containing it → the
source's `ttl_seconds`.

Staleness is *reported*, not enforced: `GET /sources` and `/status` expose
`is_fresh`/`age_seconds`, but stale data keeps being served — by design, a slow
backend should degrade to "older data", not "no data". Consumers that care check
the status endpoint or trigger a scoped sync.

## How writes land

A **full sync** applies the source's `sync_mode`:

- `replace` (default) — the new dataset swaps in wholesale; all host timestamps reset
- `merge` — incoming `hostvars` patch over existing ones (their timestamps refresh),
  incoming groups replace their counterparts, everything else is untouched

A **host-scoped sync** (`?host=x`) updates just that host's vars and timestamp.
A **group-scoped sync** (`?group=y`) updates the vars of the hosts that belong to
that group, and the group's own vars. In both cases, if the source wasn't cached at
all yet, the full returned dataset seeds the entry.

**Enrichers** merge their partial output; their `remove_hosts` deletes hosts from
`hostvars`, the per-host timestamps, and every group's member list.

## Atomicity guarantees

`CachePort::get` returns a **clone** — a read snapshot. All mutations therefore go
through two atomic operations implemented on DashMap's entry API:

- `update(key, f)` — run `f` against the *live* entry under the cache lock;
  returns `false` if the key is absent
- `merge_or_insert(key, dataset, ttl, f)` — same, but seeds a fresh entry when the
  key is absent

The whole read-modify-write cycle holds the lock, so a scheduled sync, an enricher
and a `PUT /hosts/{hostname}` hitting the same source concurrently cannot lose each
other's writes. Two rules follow:

1. Never implement a mutation as `get` → modify → `set` — that's the lost-update
   race these operations exist to prevent (there's a regression test:
   `concurrent_updates_do_not_lose_writes`)
2. Closures passed to the atomic operations must be quick and must never call back
   into the cache — script execution happens *outside* the lock, on a snapshot,
   and only the final merge is atomic

## Disk persistence (optional)

By default nothing touches disk. Adding a `cache.persistence` block to
`config.yaml` turns on periodic snapshots:

```yaml
cache:
  persistence:
    path: "/var/lib/unified-api/cache.json"
    interval_seconds: 60   # default 60
```

Behavior:

- **Boot:** the snapshot is loaded before the schedulers start, so `/readyz`
  is green immediately and consumers get the pre-restart data while the first
  syncs run. A missing file just means "start empty"; a corrupt or
  version-mismatched file is logged and ignored — persistence never blocks
  startup.
- **Runtime:** every `interval_seconds` the whole cache is serialized and
  written atomically (temp file + rename), so a crash mid-write leaves the
  previous snapshot intact. A final snapshot is written on graceful shutdown.
- **Freshness survives:** snapshots store per-entry and per-host *ages*, not
  timestamps, and loading reconstructs them — an entry that was 40s old with a
  60s TTL comes back 40s old (plus the downtime), and anything past its TTL is
  reported stale exactly as if the process had never restarted.

This is a durability optimization for restarts, not shared storage: with
multiple replicas each pod snapshots its own cache, so give each its own path
(or its own volume). The DashMap remains the source of truth — reads and
writes never wait on disk.

## Memory notes

Entries are only removed by `CachePort::remove` (currently unused by any route) or
process restart; an entry whose source disappears from `sources.yaml` survives in
the cache until then. With inventory-sized payloads this is harmless, but it's worth
knowing when reading `GET /api/v1/sources` output.
