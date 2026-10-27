# Configuration API

The configuration of a unified-api instance is a directory of YAML files read
at startup. The configuration API makes that directory readable and writable
over HTTP, so a configuration-as-code pipeline can **push** a change to each
instance instead of publishing an artifact each instance has to **pull**.

It is **off by default**. Turning it on means an admin key can rewrite every
file the loader reads — the `api_keys` items included, which is the same authority
as editing the directory the container mounts.

```yaml
# config.yaml
config_api:
  enabled: true
```

- [What it is for](#what-it-is-for)
- [Routes](#routes)
- [Pushing a whole directory](#pushing-a-whole-directory)
- [What a reload can and cannot apply](#what-a-reload-can-and-cannot-apply)
- [Safety properties](#safety-properties)
- [Concurrency: ETags and If-Match](#concurrency-etags-and-if-match)
- [Permissions and audit](#permissions-and-audit)
- [Making the directory writable](#making-the-directory-writable)
- [Metrics](#metrics)

---

## What it is for

A pipeline that owns the configuration of a fleet has two ways to get a change
onto an instance:

| | Pull | Push (this API) |
|---|---|---|
| Delivery | publish an image/artifact, instance adopts it on its own schedule | `PUT` the files, instance validates and answers |
| Feedback | none — a bad file is discovered by the instance, later | immediate: `400` with every error, and nothing written |
| Latency | whatever the sync timer or image updater is set to | the request |
| Applying it | restart or replace the process | `?reload=true`, no restart |
| Needs | a registry, pull credentials, an updater per instance | an API key and a route to the instance |

Neither is wrong. Pull survives a pipeline that cannot reach the instance;
push tells the pipeline whether the change was accepted while it still has the
context to fix it.

---

## Routes

All admin-only, all under the API key, all `403` when `config_api.enabled` is
not set — with a body that says so rather than a bare `404`.

| Route | Meaning |
|---|---|
| `GET /api/v1/config` | The directory: every file (at any depth) with size, sha256 and mtime, what is missing, whether it still loads, whether the process is running it |
| `GET /api/v1/config/{path}` | One file, verbatim. `ETag` is the sha256; `If-None-Match` answers `304` |
| `PUT /api/v1/config/{path}` | Create or replace one file (its folders are created). Body is raw YAML |
| `DELETE /api/v1/config/{path}` | Remove one file (folders it leaves empty go too) |
| `PUT /api/v1/config` | Replace the whole directory in one transaction |
| `POST /api/v1/config/validate` | Dry run: same checks, nothing written, ever |
| `POST /api/v1/config/reload` | Apply what is on disk to the running process |

`PUT` and `DELETE` take `?reload=true` to apply the change as part of the
write, so a push is one request rather than two.

`{path}` is a file's path inside the directory, **slashes included** — the
directory is item files in folders (see [configuration](configuration.md#the-directory)):

```bash
curl -sS "$BASE/api/v1/config/sources/acme/src-d42-acme.yaml" -H "X-API-Key: $KEY"

curl -sS -X PUT "$BASE/api/v1/config/connectors/d42.yaml?reload=true" \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/yaml' \
  --data-binary @config/connectors/d42.yaml
```

A path must be one the loader would read: relative, segments of letters,
digits, `-`, `_` and `.`, none hidden, ending in `.yaml` or `.yml`. Anything else
is a `404` naming that rule — the same rule the loader applies to what it finds,
so a file the API cannot name cannot be part of the configuration either.
`config.yaml`, at the root, cannot be deleted, and it is the only file `missing`
in `GET /api/v1/config` can ever list: every other file is an item file that is
there or not.

A file still in the **0.34 layout** (items keyed by id, as `sources.yaml` used to
be) is refused like any other unknown top-level key, with a hint naming
`unified-api --migrate-config` — a pipeline that has not converted its
repository yet gets a one-line fix rather than a typo hunt. The startup
migration (see [configuration → upgrading](configuration.md#upgrading-from-034))
converts a directory on disk; it does not convert what is pushed.

---

## Pushing a whole directory

The verb a pipeline wants. `files` is keyed by path relative to the directory,
and `prune: true` makes the directory become *exactly* the payload — the same
semantics as the configuration image it replaces, where what is not in the image
is not in `/config`. Every file the payload leaves out, at any depth, is
removed, and so are the folders that leaves empty; `deleted` names them.

```bash
curl -sS -X PUT "$BASE/api/v1/config?reload=true" \
  -H "X-API-Key: $KEY" \
  -H 'Content-Type: application/json' \
  -H "If-Match: \"$ETAG\"" \
  -d @- <<'JSON'
{
  "prune": true,
  "files": {
    "config.yaml": "server:\n  host: \"0.0.0.0\"\n  port: 8080\n",
    "connectors/d42.yaml": "connectors:\n  - name: d42\n    project: prj-inv\n    path: d42/inventory.py\n",
    "sources/dc4/src-dc4.yaml": "sources:\n  - name: src-dc4\n    description: \"DC4\"\n    connector: d42\n    ...\n",
    "projects/prj-inv.yaml": "projects:\n  - name: prj-inv\n    git_url: \"...\"\n"
  }
}
JSON
```

```json
{
  "written": ["config.yaml", "connectors/d42.yaml", "projects/prj-inv.yaml",
              "sources/dc4/src-dc4.yaml"],
  "deleted": ["sources/src-old.yaml"],
  "etag": "9f2c…",
  "summary": {"sources": 1, "connectors": 1, "hosts": 0, "groups": 0, "views": 0,
              "credentials": 0, "enrichers": 0, "endpoints": 0, "projects": 1,
              "api_keys": 0},
  "reloaded": {
    "generation": 3,
    "applied": ["sources", "connectors"],
    "sources": {"added": ["src-dc4"], "removed": ["src-old"]},
    "connectors": {"added": ["d42"]},
    "api_keys": 2
  },
  "reload_pending": false
}
```

A pipeline builds that payload from its checkout of the configuration
repository — every YAML file, keyed by its path relative to the directory:

```bash
cd config
find . -type f \( -name '*.yaml' -o -name '*.yml' \) -not -path '*/.*' | sed 's|^\./||' | sort |
  while read -r f; do jq -n --arg p "$f" --rawfile c "$f" '{($p): $c}'; done |
  jq -s '{prune: true, files: add}' > /tmp/payload.json

curl -sS -X PUT "$BASE/api/v1/config?reload=true" \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  --data-binary @/tmp/payload.json
```

Hidden paths are left out because the loader skips them too (`.git`, editor
swap files); a file the push should not carry is one the directory should not
hold.

Validate first from CI, against the instance that will run it, without writing
anything:

```bash
curl -sS -X POST "$BASE/api/v1/config/validate" \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  --data-binary @/tmp/payload.json
```

```json
{"valid": false, "errors": [
  "Connector 'd42' references unknown project 'prj-typo'",
  "View 'view-all' references unknown source 'src-gone'"
]}
```

Every problem at once, the same list `--check-config` prints — because it is
the same code, run against a staged copy of the directory the push would
produce. One difference: startup and `--check-config` convert what a previous
version wrote (0.35 ssh connectors) in memory, and say so; a push is refused
until it is written in this version's shape, so the repository moves. Push a
connector and the sources it changes together (one file, or `PUT
/api/v1/config`): each alone is half a conversion and does not validate.

A whole-directory push is one request body, so it lives under
`server.max_body_bytes` (default 2 MiB) like every other route; a directory
that outgrows it answers `413` naming the key to raise.

---

## What a reload can and cannot apply

What a reload can do is decided by **where each setting is read**. Anything
read per request or per tick can be swapped, because the next reader reads the
new value. Anything consumed once, at construction, cannot — the thing it built
is already running.

**Applied by a reload, no restart:**

| | Effect |
|---|---|
| `sources` | New sources start syncing, removed ones stop, changed ones restart on the new settings |
| `connectors` | A source is resolved through its connector, so the sources using a changed connector count as changed themselves and restart on it |
| `views`, `enrichers`, `endpoints` | Live on the next request/tick |
| `projects` | Pull tasks restart; a project that arrived with the reload is cloned in the background |
| `credentials`, `secrets:` | The resolver chain is rebuilt (which also drops the resolution cache) |
| `api_keys` | In force on the next request |
| `server.readyz_require_all_sources` | Next probe |
| `server.refresh_timeout_seconds`, `server.refresh_max_concurrent` | Next refresh. A shrunk concurrency cap lets in-flight refreshes finish under the old limit and reclaims their permits as they land |
| `server.shutdown_grace_seconds` | The next shutdown — the drain reads whatever a reload set last |
| `server.metrics_require_auth` | The next scrape — the `/metrics` handler checks the flag per request |
| `server.cors_allowed_origins` | The next request — the CORS middleware reads the list per request, so adding a dashboard's origin is a push, not a fleet restart |
| `server.max_body_bytes` | The next request — raising the limit for a growing configuration push needs no restart |

**Needs a restart** — reported, never silently ignored:

`server.host`, `server.port`,
`cache.persistence`, `projects.dir`, `config_api.enabled`, `server.ui.enabled`,
`server.ui.session_ttl_seconds`.

A write that touches one of them still lands on disk; the response names it:

```json
{"reloaded": {"generation": 4, "applied": ["sources"],
              "restart_required": ["server.port"]}}
```

`GET /api/v1/config` keeps reporting `restart_required` until a restart
actually adopts it, so the state is visible to anything that looks, not only
to whoever made the write — and `/metrics` exports the count as the
`unified_api_config_restart_required` gauge, so a fleet where some pods took
a push they could only partially adopt is one alert away, not one `GET` per
pod (see [observability](observability.md)).

### What happens to the running work

The scheduler does not try to reconfigure its tasks — it replaces them. The
outgoing generation is told to stop and finishes whatever it was in the middle
of (a gather is never cut mid-write, the same rule shutdown follows); a new
generation is spawned from the new configuration immediately. The two can
overlap for the length of one gather, which is safe because syncs of a single
source are already serialised.

---

## Safety properties

- **Validated as a directory, before anything moves.** The proposed set is
  staged in a temporary directory and loaded there. A rejected change never
  touched the real one.
- **Rejected whole.** Cross-file references (a source naming a connector, a
  connector naming a project, a view naming a source, a restricted key naming an
  endpoint) only make sense
  against the complete set, so the complete set is what is accepted or refused.
- **Atomic per file.** Every file is written to a temporary name and renamed
  into place, so no reader ever sees a half-written file.
- **A reload cannot turn authentication off.** A configuration with no API
  keys is legitimate — it is how a fresh instance starts — but *arriving* at
  it under a running process is refused with `409`. Write the file and restart
  if that is really the intent.
- **A key that cannot be resolved fails the whole operation.** If
  an `api_keys` item names an env var that is not set, the write is refused
  **before** it is committed, rather than landing and then locking a consumer
  out.
- **A write without `?reload=true` says so.** `reload_pending: true` means the
  files are on disk and the process is still serving the previous
  configuration.

---

## Concurrency: ETags and If-Match

Every file's `ETag` is the sha256 of its bytes; the directory has one too, over
every file's name and hash. Send it back as `If-Match` and a write that would
clobber someone else's change is refused with `412` instead of winning
silently:

```bash
ETAG=$(curl -sS "$BASE/api/v1/config" -H "X-API-Key: $KEY" | jq -r .etag)
# … render the files …
curl -sS -X PUT "$BASE/api/v1/config" -H "If-Match: \"$ETAG\"" …
```

Content-addressed rather than mtime-based, on purpose: re-pushing identical
bytes changes nothing and does not change the ETag, so an idempotent pipeline
stays idempotent.

Omitting `If-Match` means "I am the only writer", which is the common case for
a single pipeline and stays unceremonious.

---

## Permissions and audit

Admin keys only, **reads included** — `config.yaml` and the credentials
describe the estate (which systems exist, which variable holds which
credential), which is exactly what a restricted consumer key has no business
enumerating.

Every write and reload emits an audit line under the `audit` target, with the
key that did it and the request id:

```
actor=pipeline action=config_write_reload resource=sources/dc4/src-dc4.yaml request_id=req-42 outcome=success
```

`RUST_LOG=audit=info` to isolate them. See [observability](observability.md).

---

## Making the directory writable

The API writes to `CONFIG_DIR`. Whatever is there must be writable **and
should survive a restart** — otherwise a push is applied to the running
process and then lost the next time the container starts from its image.

| Deployment | What to do |
|---|---|
| Kubernetes | Mount `CONFIG_DIR` from a PersistentVolumeClaim, or from an `emptyDir` seeded by an initContainer if losing pushes on restart is acceptable. A ConfigMap mount is read-only and cannot be used |
| Container on a host | A bind-mounted host directory, which is what an edge already uses |
| Systemd | The directory as-is, owned by the service user |

An instance whose directory is read-only will refuse writes with a `500`
naming the filesystem error; leave `config_api.enabled` off there.

One exception: a read-only directory still in the 0.34 layout is converted at
startup into a private temporary copy, and that copy is what the process runs —
so writes *succeed*, land in the copy, and are gone at the next restart (which
converts the read-only files again). Convert the source of the files first; see
[configuration → upgrading](configuration.md#a-read-only-directory).

---

## Metrics

| Metric | Meaning |
|---|---|
| `unified_api_config_writes_total{outcome}` | `success`, `rejected` (did not validate) or `error` (could not be written) |
| `unified_api_config_reloads_total{outcome}` | `success` or `invalid` |

The reload generation is in `GET /api/v1/config` rather than in a metric: it is
a property of a configuration, not a rate.
