# Output endpoints

The consumer-facing side of the cache: an **output endpoint** merges one or
more cached datasets through a transformer script and returns whatever the
consumer needs — the shipped example renders a merged Ansible inventory for
AWX and AnsibleForms. Field reference lives in
[configuration → endpoints.yaml](configuration.md#endpointsyaml).

- [What an endpoint is](#what-an-endpoint-is)
- [The script contract](#the-script-contract)
- [GET versus POST](#get-versus-post)
- [Failure shapes](#failure-shapes)
- [Permissions](#permissions)
- [Observability](#observability)

---

## What an endpoint is

An endpoint is a read with a transformation attached: it never gathers and
never writes to the cache — it renders what is already there, at request time,
every time.

```yaml
ep-awx-full:
  name: "Full AWX inventory"
  source_ids: ["src-d42", "src-fleet-facts", "src-inventory"]
  script_path: "outputs/ansible_inventory.py"
  project_id: "prj-connectors"     # optional: resolve the script (and its
                                   # virtualenv) inside this checkout
  timeout_seconds: 300
  config:                          # static, script-specific
    filter_os: "OracleLinux"
```

Endpoints read cached **sources**, never views — a view has no cache entry of
its own, so config validation tells you to list the members instead.

---

## The script contract

The transformer is spawned per request with a scrubbed environment (no API
keys, no other credentials), bounded by `timeout_seconds`, and its stdout
becomes the HTTP response body verbatim.

| Channel | Content |
|---|---|
| stdin | `{ "<source_id>": <Dataset>, ... }` — every configured source's cached dataset |
| CLI arguments | `script_args`, verbatim — no shell |
| `ENDPOINT_CONFIG` env | The endpoint's static `config` as JSON — plus the reserved keys: `trigger` (the request id) and, with a project virtualenv, `python_venv_bin` |
| `ENDPOINT_PARAMS` env | The request's dynamic parameters as JSON (`{}` if none) |
| stdout | The response body, as-is. Output starting with `{` or `[` is served as `application/json`, anything else as `text/plain` |

The script decides the format entirely — inventory JSON, INI, CSV, plain
text. With `project_id` set, the script path resolves inside the checkout at
every execution and the project's [virtualenv](projects.md#python-virtualenvs)
leads its PATH.

---

## GET versus POST

Both run the same script with the same `ENDPOINT_PARAMS` shape; they differ
only in how the parameters arrive — pick per consumer, not per endpoint.

- **`GET /api/v1/endpoints/{id}?env=prod&limit=5`** — for browsers, proxy
  caches, and tools that only take a URL (an AWX inventory source). A query
  string carries no types, so every parameter arrives as a **string**.
- **`POST /api/v1/endpoints/{id}`** with a JSON body — when a parameter has to
  be a real number, boolean, or nested structure.

A transformer that coerces its inputs works identically under both.

---

## Failure shapes

An endpoint distinguishes "not ready" from "broken", and both carry a JSON
body naming the problem.

| Status | Meaning |
|---|---|
| `503` | One or more sources not yet synced — the body lists `missing_sources`, so the caller knows what to wait for |
| `504` | The transformer exceeded `timeout_seconds` and was killed |
| `500` | The script exited non-zero; the body carries its error |
| `404` | The endpoint id is not configured |
| `403` | The API key is not granted this endpoint |

---

## Permissions

Granting an endpoint grants its **rendered output** — even when the key
cannot read the underlying sources raw. The endpoint is the product: a
consumer given `ep-awx-full` gets the merged inventory without also getting
`GET /sources/{id}/dataset` on the members.

Restricted keys list endpoint ids under `endpoints:` in `api_keys.yaml`;
`GET /api/v1/endpoints` filters to what the key may run and reports each
endpoint's `sources_ready` / `sources_missing`.

---

## Observability

Every run lands in `/metrics` as
`unified_api_endpoint_total{endpoint, result}` and a per-endpoint duration
histogram; timed-out and failed runs count as `result="error"`, so alerting
on the error rate catches hung transformers too — see
[observability](observability.md). The request's id rides into the script as
`ENDPOINT_CONFIG.trigger`, so a transformer's own logs join the same trace as
the access log.
