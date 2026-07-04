# REST API

Interactive documentation lives at **`/swagger-ui/`** (the root `/` redirects there),
with the raw OpenAPI spec at `/api-docs/openapi.json`. This page is the quick
reference; the spec is generated from the code and is always authoritative.

## Authentication

If the `UNIFIED_API_KEY` environment variable is set, every `/api/v1/*` route
requires it — either header works:

```
X-API-Key: <key>
Authorization: Bearer <key>
```

Wrong or missing key → `401`. Keys are compared in constant time. Health probes
(`/healthz`, `/readyz`) and the Swagger UI remain public. If the variable is not
set, authentication is disabled (useful for local development).

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
| `GET /api/v1/sources/{id}/dataset` | The full cached dataset (hostvars + groups) |
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
