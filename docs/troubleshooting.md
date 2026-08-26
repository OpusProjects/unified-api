# Troubleshooting

Symptom first, then where to look. Every check here is a route or a metric the
service already exposes — none of it needs a debugger or a restart.

The two questions worth separating before anything else:

- **How old is the data?** `age_seconds`, `is_fresh`, `dataset_is_fresh`
- **Is anything still refreshing it?** `sync_health`

A long sync interval and a connector that has been failing since Tuesday both
look like a dataset slowly getting older. Only the second pair tells them apart.

- [A source's data is older than its interval](#a-sources-data-is-older-than-its-interval)
- [A source is missing from `GET /sources`](#a-source-is-missing-from-get-sources)
- [`dataset_is_fresh` is false but the hosts look fine](#dataset_is_fresh-is-false-but-the-hosts-look-fine)
- [A view 404s a host that plainly exists](#a-view-404s-a-host-that-plainly-exists)
- [A view serves an empty dataset while every member looks healthy](#a-view-serves-an-empty-dataset-while-every-member-looks-healthy)
- [`refresh=true` returned success but the data did not change](#refreshtrue-returned-success-but-the-data-did-not-change)
- [A manual sync seems to have had no effect](#a-manual-sync-seems-to-have-had-no-effect)
- [A script enricher's keys keep disappearing](#a-script-enrichers-keys-keep-disappearing)
- [A sync times out](#a-sync-times-out)
- [Hosts vanish from a group after a sync](#hosts-vanish-from-a-group-after-a-sync)
- [Everything answers 401 or 403](#everything-answers-401-or-403)
- [A configuration push is refused or does not take effect](#a-configuration-push-is-refused-or-does-not-take-effect)
- [Useful one-liners](#useful-one-liners)

---

## A source's data is older than its interval

Start by asking the source itself how its last few syncs went.

```bash
curl -s localhost:8182/api/v1/sources/src-d42/status -H "$KEY" | jq '.sync_health'
```

```json
{
  "last_attempt_age_seconds": 41,
  "last_success_age_seconds": 21600,
  "last_error": "Script 'd42/fetch.py' failed with exit code Some(1)",
  "consecutive_failures": 12
}
```

`last_attempt` recent + `consecutive_failures` climbing = the scheduler is
running fine and the connector is failing. The error is the connector's own, so
read `last_error` and go look at the script. Stale data keeps being served
throughout — by design, a slow backend should degrade to *older data*, not *no
data*.

If `last_success_age_seconds` is absent entirely, the source has **never**
synced successfully.

---

## A source is missing from `GET /sources`

That listing is driven by the cache, so a source that has never synced is not in
it. It is not lost — look at:

```bash
curl -s localhost:8182/readyz | jq '.sources_pending'
curl -s localhost:8182/metrics | grep unified_api_source_cached
```

`unified_api_source_cached 0` is the alertable form: a configured source with no
cache entry. This is the one state `/sources` and `/status` cannot show you,
which is why the gauge exists.

---

## `dataset_is_fresh` is false but the hosts look fine

Expected when the entry was created by a **scoped** sync. A host- or
group-scoped sync gathers the hosts it was asked for, not the source, so there
is no full gather for `ttl_seconds` to be measured against. The per-host
timestamps are true; the dataset-level ones say "no full sync has landed here".

The next full sync clears it, in either sync mode. See
[caching & TTLs](caching.md).

---

## A view 404s a host that plainly exists

A view routes by **declared ownership**, not by which member happens to have the
host cached. A 404 means no member claims it.

```bash
curl -s localhost:8182/api/v1/sources/vw-all/status -H "$KEY" | jq '.members'
```

```json
[{ "source_id": "src-dc1", "cached": true, "ownership_cached": false, ... }]
```

`ownership_cached: false` is the usual cause: the inventory source the member's
`owns.source` points at has never synced, so its group patterns cannot be
expanded and the member claims nothing beyond hosts named literally in the view.
Sync that inventory source and the routing comes back.

Otherwise the host's group is genuinely not in any member's `owns.groups` — the
error names the members so you can see which one should have claimed it.

---

## A view serves an empty dataset while every member looks healthy

Same cause, visible in one metric:

```
unified_api_view_members_cached{view="vw-all"}   2
unified_api_view_members_routable{view="vw-all"} 0
unified_api_view_hosts{view="vw-all"}            0
```

Members have data (`cached`), none can expand its ownership (`routable`), so the
view claims nothing. Alert on `members_routable < members_total`.

---

## `refresh=true` returned success but the data did not change

Check the response headers rather than the body:

```
x-unified-api-refreshed: true
x-unified-api-refreshed-hosts: web01.example.com
```

- **no `refreshed-hosts` header** — every named host was already inside its TTL,
  so nothing was gathered. That is the TTL doing its job: a host is re-gathered
  at most once per window however many consumers ask.
- **`x-unified-api-refreshed: false`** — a gather was attempted and failed;
  `x-unified-api-refresh-error` says why. The cached data is served anyway, so
  the read succeeds and the header is the only signal.
- **`403`** — the source has no `allow_on_demand_refresh`. On a view, the member
  that owns the named host is the one that needs it.
- **`400`** — `refresh=true` without `?host=`. A whole-source refresh on a read
  would gather the entire inventory, so the hosts have to be named.

---

## A manual sync seems to have had no effect

Syncs of one source run one at a time. If a scheduled sync was already under way
your `POST /sync` waited for it, and on a source that takes minutes that looks
like nothing happening. The response arrives when the sync does, with its own
`sync_duration_ms`.

The same queueing is why a `refresh=true` issued during a long full sync can come
back `x-unified-api-refreshed: false` with `refresh did not finish within Ns`:
it waited for the sync, ran out of its own budget, and served the cached data
rather than overtaking a gather already in flight.

---

## A script enricher's keys keep disappearing

A script enricher's `hostvars` entry **replaces** a host's variable map rather
than patching it key by key. Whatever the script omits is dropped — including
keys another enricher owns. The dataset arrives on its stdin precisely so it can
carry the rest through.

A *declarative* enricher (`source_id` + `fields`) has no such constraint: it
writes only the fields it owns. See [connectors](connectors.md).

---

## A sync times out

The error names the limit it hit, which is the first clue to which limit to look at.

```json
{ "success": false, "error": "sync timed out after 300s" }
```

The script exceeded the source's `timeout_seconds` and was **killed**. Nothing
is written to the cache, and the previous data keeps being served.

Two things worth knowing: the connector is killed rather than abandoned, so it
leaves no live copy behind — and it may be killed part-way through whatever it
was doing, so scripts must be interruptible. For the SSH connector, check
whether `ssh_connect_timeout_seconds` × the number of hosts it cannot reach
exceeds the source-level `timeout_seconds`; the per-host and whole-sync limits
are different knobs.

---

## Hosts vanish from a group after a sync

If the connector reported them **unreachable**, they do not vanish — they keep
their previous variables, their true age and their group memberships, and the
sync log names them (`failed_hosts=[...]`). A host that upstream simply stopped
listing is never attempted, so it is not retained: that is how a decommissioned
host is told from one that did not answer.

If hosts really are missing from a group in a **static inventory**, check
whether the group is declared in more than one place — those declarations are
merged, so a host in either one belongs to the group.

---

## Everything answers 401 or 403

The two are different failures, and the message in the body tells them apart.

- **401** — authentication failed, before any handler ran. `missing API key`
  means no credential arrived (the message names the `X-API-Key` header);
  `invalid API key` means one arrived and matched no configured key.
- **403** — the key is valid but not scoped to that id. The message names the
  source, view or endpoint it refused.

A key granted a **view** needs no grant on the members: the members are internal
topology, the view is the contract. A key granted an **endpoint** likewise needs
no grant on the sources behind it.

If nothing requires a key at all, no keys are configured — the startup log says
so loudly, and every caller is treated as admin.

---

## A configuration push is refused or does not take effect

Every refusal from the [configuration API](config-api.md) is deliberate and
names its cause; this maps the common ones to their fix. The API is
transactional — a rejected push touched nothing.

| Response | Cause and fix |
|---|---|
| `403` naming `config_api.enabled` | The API is off (the default). Set `config_api.enabled: true` in the mounted `config.yaml` and restart — it cannot be enabled over the API itself, on purpose |
| `400` with an `errors` list | The staged directory did not validate — the same list `--check-config` prints, every problem at once. Nothing was written; fix and re-push |
| `412` | Your `If-Match` no longer matches: someone else wrote the file (or the directory) first. `GET` it again for a fresh `ETag`, merge, retry |
| `409` about API keys | The change would leave the API with **no keys at all** (silent auth removal), or names a key env var that is not set on the instance. Both are refused before anything commits |
| `413` | The push exceeds `server.max_body_bytes` (default 2 MiB) — a whole-directory `PUT` is one body. Raise the key (restart-only) |
| `200`, but nothing changed | The write landed on disk and was never applied: no `?reload=true` on the write, and nobody called `POST /config/reload`. `GET /api/v1/config` shows `reload_pending: true` for exactly this state |
| `restart_required` will not clear | The change touches a restart-only key (`server.port`, `cache.persistence`, …). It keeps being reported — by `GET /api/v1/config` and the `unified_api_config_restart_required` gauge — until a restart adopts it. That persistence is the design: restart the instance |

The audit log records every write and reload (`action: config_write`,
`config_write_reload`, `config_reload`) with the key name and outcome, so "who
pushed this and when" is a log search away — see
[observability](observability.md).

---

## Useful one-liners

Commands worth keeping to hand when something looks wrong.

```bash
# every source with its freshness and health, at a glance
curl -s localhost:8182/api/v1/sources -H "$KEY" | \
  jq -r '.[] | "\(.source_id)\t\(.kind)\tfresh=\(.is_fresh)\tage=\(.age_seconds)"'

# which sources are configured but have never synced
curl -s localhost:8182/readyz | jq -r '.sources_pending[]'

# every failing source, with the reason
curl -s localhost:8182/api/v1/sources -H "$KEY" | \
  jq -r '.[] | select(.sync_health.consecutive_failures > 0)
         | "\(.source_id): \(.sync_health.last_error)"'
```

More detail per topic: [caching & TTLs](caching.md), [views](views.md),
[refresh](on-demand-refresh.md), [observability](observability.md).
