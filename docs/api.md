# REST API

Interactive documentation lives at **`/swagger-ui/`** (the root `/` redirects there),
with the raw OpenAPI spec at `/api-docs/openapi.json`. This page is the quick
reference; the spec is generated from the code and is always authoritative.

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

### Authorization

Each key has a role:

- **`admin`** — every route, every id.
- **`restricted`** (the default) — only the source ids in its `sources` list
  and the endpoint ids in its `endpoints` list.

For a restricted key: list routes (`GET /sources`, `GET /endpoints`) are
*filtered* to the allowed ids; id routes on anything else return `403`.
Running an enricher requires permission on the enricher's **source** (that is
what it writes to). Running an output endpoint requires the **endpoint** id
only — granting an endpoint grants its rendered output even if the key cannot
read the underlying sources raw.

Rotation is external by design: swap the secret in the env var (Secret, Vault,
…) and restart — no config change involved.

**CORS** is off by default (no CORS headers; server-to-server consumers are
unaffected). Browser-based consumers need their origins listed in
`server.cors_allowed_origins` — see [configuration](configuration.md).

## Health

| Route | Meaning |
|---|---|
| `GET /healthz` | Liveness — always `200 ok` while the process runs |
| `GET /readyz` | Readiness — `200` when no sources are configured or at least one has synced; `503` otherwise, with the pending list |
| `GET /metrics` | Prometheus metrics (sync/enrich/endpoint counters and durations) — see [deployment](deployment.md) |

## Sources

| Route | Meaning |
|---|---|
| `GET /api/v1/sources` | Cached sources with freshness and host counts |
| `GET /api/v1/sources/{id}/dataset` | The cached dataset (hostvars + groups); paginate/filter with `?limit=&offset=&host=&group=` |
| `GET /api/v1/sources/{id}/status` | Per-host age/TTL/freshness; filter with `?host=` or `?group=` |
| `POST /api/v1/sources/{id}/sync` | Run the connector now. `?host=x` or `?group=y` scope the sync |
| `PUT /api/v1/sources/{id}/hosts/{hostname}` | Upsert one host's vars in the cache (body: JSON object) |
| `DELETE /api/v1/sources/{id}/hosts/{hostname}` | Remove a host from the cached dataset |

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
  "error": null
}
```

`404` means the source id itself isn't configured.

### Dataset pagination

Without query parameters, `/dataset` returns the **raw Dataset** — the exact
shape consumers parse, unchanged. But a large inventory (1000 hosts ≈ 8-10MB
of JSON) will hang a browser UI like Swagger if rendered whole; add any of
`limit`, `offset`, `host` or `group` and the response becomes a paginated
envelope instead:

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
(and returns only that group); unknown names are `404`. Group membership
lists are always included — they're tiny next to the hostvars, which carry
the facts.

## Enrichers

| Route | Meaning |
|---|---|
| `POST /api/v1/enrichers/{id}/run` | Run an enricher against its source's cached dataset |

`404` if the enricher isn't configured **or** its source has never synced.
The result reports `hosts_updated` / `hosts_removed` and any script error.

## Output endpoints

| Route | Meaning |
|---|---|
| `GET /api/v1/endpoints` | Configured endpoints and whether their sources are cached |
| `POST /api/v1/endpoints/{id}` | Run the transformer and return its output verbatim |

The optional JSON body is passed to the script as dynamic parameters
(`ENDPOINT_PARAMS`), overriding static `config` where the script chooses to.
`503` if a required source isn't in the cache yet.

```bash
curl -X POST localhost:8182/api/v1/endpoints/ep-ansible-full \
     -H 'Content-Type: application/json' \
     -d '{"filter_os": "OracleLinux"}'
```

## Projects (admin-only)

Operational routes for git project checkouts — restricted keys always get `403`.

| Route | Meaning |
|---|---|
| `GET /api/v1/projects` | Configured projects with `checkout_present` and their sync settings |
| `POST /api/v1/projects/{id}/sync` | Clone/update the checkout to the branch tip, on demand |

The sync route is how a pipeline in the scripts repository rolls new script
versions without restarting the app (see
[configuration → projects.yaml](configuration.md#projectsyaml)): `200` with the
duration on success, `502` when git fails (bad URL, auth, network), `404` for
an unknown project id. Scripts are re-read from disk on every execution, so an
updated checkout takes effect on the next run.
