# REST API

Interactive documentation lives at **`/swagger-ui/`** (the root `/` redirects there),
with the raw OpenAPI spec at `/api-docs/openapi.json`. This page is the quick
reference; the spec is generated from the code and is always authoritative.

- [Authentication](#authentication)
- [Errors](#errors)
- [Health](#health)
- [Sources](#sources)
- [Enrichers](#enrichers)
- [Output endpoints](#output-endpoints)
- [Projects (admin-only)](#projects-admin-only)

---

## Authentication

API keys are defined in `api_keys.yaml` (see [configuration](configuration.md));
each key's secret lives in the environment variable the definition names, never
in the YAML. The legacy `UNIFIED_API_KEY` environment variable still works and
acts as one extra admin key. Either header authenticates:

```
X-API-Key: <key>
Authorization: Bearer <key>
```

Wrong or missing key → `401`. Keys are compared in constant time. Health probes
(`/healthz`, `/readyz`), `/metrics` and the Swagger UI remain public. With no
keys configured at all, authentication is disabled (useful for local
development) and the app logs a warning at startup.

`/metrics` is public because that is what a Prometheus scrape config expects,
but its exposition labels every source id and host count — a description of
your inventory topology. On a shared network set
`server.metrics_require_auth: true` and give the scraper a key; the health
probes stay public regardless, since they carry no inventory data. With no
keys configured the flag has no effect, because authentication is off
entirely.

### Authorization

Each key has a role:

- **`admin`** — every route, every id.
- **`restricted`** (the default) — only the source ids in its `sources` list
  and the endpoint ids in its `endpoints` list.

For a restricted key: list routes (`GET /sources`, `GET /endpoints`) are
*filtered* to the allowed ids; id routes on anything else return `403`.
Running an enricher requires permission on the enricher's **target** (that is
what it writes to). Running an output endpoint requires the **endpoint** id
only — granting an endpoint grants its rendered output even if the key cannot
read the underlying sources raw.

Rotation is external by design: swap the secret in the env var (Secret, Vault,
…) and restart — no config change involved.

**CORS** is off by default (no CORS headers; server-to-server consumers are
unaffected). Browser-based consumers need their origins listed in
`server.cors_allowed_origins` — see [configuration](configuration.md).

---

## Errors

**Every** failure from every route carries the same JSON body:

```json
{ "error": "source 'src-d42' is not in the cache (never synced, or evicted)" }
```

The message distinguishes cases that share a status code — `404` from
`/dataset` means "not in the cache", `404` from `/sync` means "not in
`sources.yaml`", and a `404` from `DELETE .../hosts/{hostname}` names whichever
of the two is missing. Treat the text as loggable, not matchable: branch on the
status code, read the message.

The one exception is `401`: the API-key middleware rejects a request before any
handler runs, so an unauthenticated call gets the status alone.

---

## Health

Three unauthenticated probes, meant for load balancers, orchestrators and Prometheus rather than for consumers.

| Route | Meaning |
|---|---|
| `GET /healthz` | Liveness — always `200 ok` while the process runs |
| `GET /readyz` | Readiness — `200` when no sources are configured or at least one has synced; `503` otherwise, with the pending list. `server.readyz_require_all_sources: true` waits for every source |
| `GET /metrics` | Prometheus metrics (sync/enrich/endpoint counters and durations, per-source freshness gauges) — see [observability](observability.md) |

---

## Sources

Reading cached inventory: what is configured, how fresh it is, and the datasets and groups themselves.

| Route | Meaning |
|---|---|
| `GET /api/v1/sources` | Cached sources with freshness, host counts and sync health, then every configured [view](views.md) (`kind: "view"`) |
| `GET /api/v1/sources/{id}/dataset` | The cached dataset (hostvars + groups); supports `ETag`/`If-None-Match`; paginate/filter with `?limit=&offset=&host=&group=`; `?host=x&refresh=true` brings those hosts up to date first — see [on-demand refresh](on-demand-refresh.md) |
| `GET /api/v1/sources/{id}/groups` | Group names with host counts and children — no facts |
| `GET /api/v1/sources/{id}/hosts` | Hostnames only, sorted |
| `GET /api/v1/sources/{id}/status` | Per-host age/TTL/freshness; filter with `?host=` or `?group=`. `total_hosts` counts the whole source, `returned` this response |
| `POST /api/v1/sources/{id}/sync` | Run the connector now. `?host=x` (comma-separated list) or `?group=y` scope the sync; `&refresh_origin=true` makes a federated source's origin re-gather first — see [on-demand refresh](on-demand-refresh.md) |
| `PUT /api/v1/sources/{id}/hosts/{hostname}` | Upsert one host's vars in the cache (body: JSON object) |
| `DELETE /api/v1/sources/{id}/hosts/{hostname}` | Remove a host from the cached dataset |
| `DELETE /api/v1/sources/{id}` | Drop the whole cache entry (the configuration is untouched) |

A sync always answers `200` with a result body — `success: false` carries the
connector or credential error rather than mapping it to an HTTP status:

```json
{
  "source_id": "src-section9",
  "success": true,
  "scope": "full",
  "total_hosts": 42,
  "total_groups": 5,
  "sync_duration_ms": 130,
  "error": null,
  "coalesced": false
}
```

`404` means the source id itself isn't configured.

The request's `x-request-id` travels into the connector as `SOURCE_CONFIG`'s
`trigger` key (scheduled syncs say `scheduled`, on-demand refreshes say
`refresh`), so a connector's own logs can be stitched into the same trace as
the access log — end to end from consumer to script.

Concurrent **full** syncs of one source coalesce: they always ran one after
another (so an older gather can never overwrite a newer one), but each used to
pay for its own complete gather — N impatient requests were N sequential
datacenter inventories. Now a full sync that finds a full sync completed while
it queued answers from that result instead, with `coalesced: true` and
`sync_duration_ms: 0`: a sync that *started after the request began* is
everything the request could have asked for. Scoped syncs (`?host=`,
`?group=`) and `refresh_origin` requests never coalesce — they ask for
something a plain bulk gather does not deliver — and a *failed* sync satisfies
nobody: the next request in the queue gathers for real.

### Views

A [view](views.md) answers on every route above, in the same shapes: a per-host
read is served by whichever member owns that host, and `refresh=true` is
delegated to that member. `GET .../status` gains a `members` array, and the
write routes (`POST .../sync`, `DELETE`, host `PUT`/`DELETE`) answer `400` with
a body naming the members — a view gathers nothing and holds no cache entry.

One read semantic differs, deliberately: on a view, a **named** host that no
member claims is a `404` rather than an empty result, because the request cannot
be routed at all. An unmatched `?group=` is still an empty result.

`DELETE /api/v1/sources/{id}` drops the cached entry and reports how many
hosts went with it. It removes **cached data, not configuration**: a source
still listed in `sources.yaml` will be refilled by its next scheduled sync or
an explicit `POST .../sync`. The point is a source you have *removed* from
config, whose entry would otherwise be served — and re-persisted in
snapshots — until the next restart.

### Sync health

`GET /sources` and `GET /sources/{id}/status` carry a `sync_health` block once
a source has synced at least once in the current process:

```json
{
  "sync_health": {
    "last_attempt_age_seconds": 45,
    "last_success_age_seconds": 21600,
    "last_error": "ssh: connection refused (motoko.section9.net)",
    "consecutive_failures": 12
  }
}
```

The freshness fields answer *how old is this data*; these answer *is anything
still managing to refresh it*. The example above is the case that used to be
invisible outside the logs: data six hours old, still being served, with a
connector that has failed twelve times since. A long sync interval and a
broken connector both look like a dataset slowly getting older — only
`last_error` and `consecutive_failures` tell them apart.

The same block, in the same shape, appears on the other periodic work:
`GET /enrichers` carries one per enricher (a permanently failing enricher, or
one whose target never syncs, is the same "only in the logs" problem), and
`GET /projects` carries one per project — which is where "the checkout exists
but is stuck on a stale commit because every pull fails" becomes visible,
since `checkout_present` stays `true` the whole time.

A successful sync clears `last_error` and resets `consecutive_failures` to 0,
but `last_success_age_seconds` is deliberately kept across failures.

The block is absent when no sync has been attempted yet, and a source that
has **never** synced successfully has no cache entry, so it doesn't appear in
`GET /sources` at all — watch `unified_api_source_cached` or `/readyz`'s
`sources_pending` for that case.

### Discovery

Two cheap routes answer "what's in this source" without transferring the
facts, which matters because auto-groups take their names from fact keys —
the group set is data-dependent and can't be read off the config:

```bash
curl localhost:8182/api/v1/sources/src-d42/groups   # [{"name":"linux","host_count":412,...}]
curl localhost:8182/api/v1/sources/src-d42/hosts    # {"total_hosts":987,"hosts":["..."]}
```

Both are served from the cache and answer `404` only when the source isn't
cached. `host_count` counts unique members.

### Dataset pagination

Without query parameters, `/dataset` returns the **raw Dataset** — the exact
shape consumers parse, unchanged. That is a lot of JSON for an enterprise
inventory (1000 hosts ≈ 8-10MB), so add any of `limit`, `offset`, `host` or
`group` and the response becomes a paginated envelope instead:

```
GET /api/v1/sources/src-d42/dataset?limit=50&offset=100
GET /api/v1/sources/src-d42/dataset?group=linux&limit=50
```

```json
{
  "source_id": "src-d42",
  "total_hosts": 987,
  "offset": 100,
  "limit": 50,
  "returned": 50,
  "hostvars": { "...50 hosts, sorted by name for stable pages..." : {} },
  "groups": { "...all groups (or just the filtered one with ?group=)..." : {} }
}
```

`host=` returns a single host, `group=` restricts to that group's members
(and returns only that group). Group membership lists are always included —
they're tiny next to the hostvars, which carry the facts.

An unmatched filter is an **empty result**, not a `404`: a filter that
matches nothing is an empty collection, not a missing resource. This matters
for `group=` in particular, because auto-groups take their names from fact
keys — `?group=autofs` is a valid query that selects nothing until some host
reports autofs data. `404` means the *source* isn't in the cache.

### Conditional requests and compression

Most consumers poll: AWX pulls the same inventory on a schedule, federation
peers re-fetch sources that usually haven't changed. Two standard HTTP
mechanisms keep that cheap, and both are transparent to clients that ignore
them:

**ETag / If-None-Match.** Every plain `/dataset` response (no query
parameters) carries a strong `ETag` derived from the dataset's serialized
bytes. Send it back on the next poll and the server answers `304 Not
Modified` with an empty body while the dataset is unchanged — no
serialization, no transfer, just a header comparison:

```bash
$ curl -sD- localhost:8182/api/v1/sources/src-d42/dataset -o inventory.json | grep -i etag
etag: "cafe1234deadbeef-524288"

# Later polls: full body only when something actually changed
$ curl -s -H 'If-None-Match: "cafe1234deadbeef-524288"' \
       -w '%{http_code}\n' localhost:8182/api/v1/sources/src-d42/dataset
304
```

The ETag changes whenever the dataset does (sync, enricher, host PUT/DELETE)
and is stable across restarts for identical data.

Filtered and paginated queries carry a validator too, so a consumer polling
one slice (`?group=linux` every few minutes) gets `304` while nothing
changes. It is built differently — from the cache's write counter plus the
query parameters, rather than the response bytes — which has two
consequences worth knowing:

- **Any write invalidates it**, including a sync of an unrelated source. You
  may get a full body back when your slice didn't actually change. Never
  stale, occasionally redundant.
- **It does not survive a restart.** The counter starts from zero again, so a
  stored validator stops matching and the client re-fetches once. The plain
  path's content-derived ETag does survive.

**Gzip.** Responses are compressed when the client sends
`Accept-Encoding: gzip` (`curl --compressed`, and most HTTP libraries, do by
default). Inventory JSON repeats the same variable names for every host, so
it typically shrinks ~10×, which matters for WAN consumers like remote
federation. Clients that don't advertise gzip get identity bytes, unchanged.

---

## Enrichers

One route, which runs a configured enricher against the cached dataset of its target.

| Route | Meaning |
|---|---|
| `POST /api/v1/enrichers/{id}/run` | Run an enricher against its target's cached dataset |

`404` if the enricher isn't configured **or** its target has never synced.
The result reports `hosts_updated` / `hosts_removed` and any script error.
For declarative merges, `404` is also returned if the source has never synced.

---

## Output endpoints

Running a configured transformer and returning whatever it produces, unaltered.

| Route | Meaning |
|---|---|
| `GET /api/v1/endpoints` | Configured endpoints and whether their sources are cached |
| `POST /api/v1/endpoints/{id}` | Run the transformer and return its output verbatim |
| `GET /api/v1/endpoints/{id}` | The same, with query parameters as the dynamic parameters |

The optional JSON body is passed to the script as dynamic parameters
(`ENDPOINT_PARAMS`), overriding static `config` where the script chooses to.
`503` if a required source isn't in the cache yet.

```bash
curl -X POST localhost:8182/api/v1/endpoints/ep-ansible-full \
     -H 'Content-Type: application/json' \
     -d '{"filter_os": "OracleLinux"}'

# Same run, reachable by anything that can only fetch a URL
curl 'localhost:8182/api/v1/endpoints/ep-ansible-full?filter_os=OracleLinux'
```

Rendering an inventory is a read, so `GET` works too — for browsers, proxy
caches, and tools that only take a URL. A query string carries no types, so
every parameter arrives at the script as a **string**; the script sees the
same `ENDPOINT_PARAMS` object either way. Use `POST` when a parameter has to
be a real number, boolean, or nested structure.

---

## Projects (admin-only)

Operational routes for git project checkouts — restricted keys always get `403`.

| Route | Meaning |
|---|---|
| `GET /api/v1/projects` | Configured projects with `checkout_present`, their sync settings and `sync_health` |
| `POST /api/v1/projects/{id}/sync` | Clone/update the checkout to the branch tip, on demand |

The sync route is how a pipeline in the scripts repository rolls new script
versions without restarting the app (see
[configuration → projects.yaml](configuration.md#projectsyaml)): `200` with the
duration on success, `502` when git fails (bad URL, auth, network), `404` for
an unknown project id. Scripts are re-read from disk on every execution, so an
updated checkout takes effect on the next run.
