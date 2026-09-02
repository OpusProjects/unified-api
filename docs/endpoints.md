# Output endpoints

The consumer-facing side of the cache: an **output endpoint** merges one or
more cached datasets through a transformer and returns whatever the consumer
needs — the builtin `ansible` transformer renders a merged Ansible inventory
for AWX and AnsibleForms. A transformer is either a **builtin** (`output:`, run
in-process) or an external **script** (`script_path:`). Field reference lives in
[configuration → endpoints.yaml](configuration.md#endpointsyaml).

- [What an endpoint is](#what-an-endpoint-is)
- [Builtin transformers](#builtin-transformers)
- [Limits: a constructed inventory](#limits-a-constructed-inventory)
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
  output: ansible                  # a builtin — or script_path: for a script
  config:                          # static transformer settings
    filter_os: "OracleLinux"
```

Endpoints read cached **sources**, never views — a view has no cache entry of
its own, so config validation tells you to list the members instead.

---

## Builtin transformers

A builtin renders in-process — no script, no interpreter spawn, no project
checkout — and every builtin shares one pipeline: merge the configured sources
(sorted by id, later ids win on overlap), apply the filters below, then write
the survivors in the builtin's format.

| Builtin | Renders | Served as |
|---|---|---|
| `output: ansible` | Ansible dynamic inventory — `_meta.hostvars` plus one key per group | `application/json` |
| `output: json` | The merged, filtered inventory in the raw source shape (`hostvars` + `groups`) | `application/json` |
| `output: csv` | One row per host, sorted by hostname — columns picked by `columns` | `text/csv` |

Every setting lives in the endpoint's free-form `config:` map, and a request
overrides any of them dynamically — a query parameter on GET, a body field on
POST:

| Setting | Effect |
|---|---|
| `filter_datacenter` | Keep hosts whose `datacenter` hostvar equals this |
| `filter_os` | Keep hosts whose `os` hostvar equals this |
| `filter_group` | Keep hosts in any of these groups (comma-separated) |
| `exclude_vars` | Drop these variables (comma-separated) from every host **and every group** — a name left on a group would otherwise reach its members when the consumer resolves the inventory |
| `columns` (csv only) | Hostvar names (comma-separated) to emit as columns, in order, after the leading `host` column. Default: every hostvar name seen, sorted |

A group that loses every host to a filter is dropped. **A `children` list is
not rewritten when that happens**, so a parent can name a child group that the
render no longer defines. Ansible treats an undefined child as an empty group,
which changes no host and no variable — and by the rule above a dropped group
had neither vars nor children of its own, so nothing is lost. What it can
break is a consumer that walks the tree and looks each child up directly: a
missing key, or a phantom group in a group picker. Read the group keys, not
the `children` names, when building a list to choose from.

A CSV cell renders a string verbatim, a missing or null var as an empty cell,
and anything structured as compact JSON, quoted per RFC 4180. Renders are
deterministic — identical inventory renders byte-for-byte identically, so
responses diff cleanly across instances and across time.

The script-only knobs (`script_path`, `script_args`, `project_id`,
`timeout_seconds`) are config errors on a builtin endpoint, named at load —
a builtin runs in-process, so none of them can mean anything.

---

## Limits: a constructed inventory

A `limit:` merges everything the endpoint's sources carry and then hands back
only **part** of it. What it narrows is the host list, and nothing else: a host
it keeps arrives with every variable, group and membership the other sources
gave it.

```yaml
ep-awx-managed:
  name: "Only what the CMDB manages"
  source_ids: ["src-cmdb", "src-vmware", "src-facts"]
  output: ansible
  limit:
    by_hosts_from_inventory: "src-cmdb"
```

That endpoint merges three sources and returns the hosts `src-cmdb` lists —
enriched with everything vCenter and the facts gatherer know about them, in
every group any of the three put them in. A VM that exists in vCenter and not
in the CMDB does not appear. The result is what Ansible calls a constructed
inventory: one source decides *who is in*, the others decide *what is known*.

| Rule | Effect |
|---|---|
| `by_hosts_from_inventory` | Keep only the hosts this source has — the same hosts `GET /sources/{id}/hosts` returns for it. It must be one of the endpoint's `source_ids` |

Three things worth knowing:

- **It applies to every transformer**, builtins and scripts alike: the limit
  runs on the datasets before one is chosen, so a script is handed an already
  limited inventory on stdin. An endpoint's scope does not depend on how it is
  rendered.
- **It is not a request parameter.** The `config:` settings above are
  transformer settings and a request may override any of them; a limit decides
  the endpoint's scope, and an endpoint is granted to keys that may not read
  its sources raw — so a caller must not be able to widen it with a query
  string.
- **A group the limit empties keeps its variables.** The limit says which hosts
  the inventory contains, not which groups stopped meaning anything, so a group
  whose every member was outside it survives as a declaration for members
  settled elsewhere (`group_by` at play time picks up the vars it finds). It is
  dropped only when it carries nothing else — no vars, no children. That is the
  one place a limit and a `filter_*` differ: a filter's answer for a group it
  emptied is "nothing", and the group goes — leaving, like any dropped group, a
  `children` entry on its parent that no longer resolves (see above).

Config errors, all named at load: a limit naming a source outside `source_ids`
(the intersection would be against data the endpoint never reads and never
waits for), a `limit:` with no rule in it, and a misspelled rule name.

More kinds of limit will live under the same key, one field each.

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
| `504` | The script exceeded `timeout_seconds` and was killed (scripts only — a builtin runs in-process, with no timeout) |
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
