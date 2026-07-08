# Configuration

Configuration is split across YAML files in a directory (default `./config`,
override with the `CONFIG_DIR` environment variable). Only `config.yaml` is
mandatory; every other file defaults to empty if absent.

All cross-references are **validated at startup** — an unknown source, credential
or project id anywhere in the config aborts boot with a list of every error found.

## config.yaml (required)

```yaml
server:
  host: "0.0.0.0"
  port: 8182
  # Optional. Empty/absent (default) = no CORS headers at all — right for
  # server-to-server consumers. List browser origins to enable, "*" for any.
  cors_allowed_origins: []

# Optional. Absent = purely in-memory cache (restarts start empty).
# With the block, the cache is snapshotted to `path` every `interval_seconds`
# (default 60) and on graceful shutdown, and reloaded at boot. See docs/caching.md.
cache:
  persistence:
    path: "/var/lib/unified-api/cache.json"
    interval_seconds: 60
```

## sources.yaml

One entry per inventory source; the key is the source id used in URLs.

```yaml
src-section9:
  name: "Section 9 Inventory"
  project_id: "prj-connectors-infra"   # must exist in projects.yaml
  script_path: "tests/adapters/out/connectors/inventory.py"
  connector_type: "script"             # "script" (default) or "ssh"
  sync_mode: "replace"                 # "replace" (default) or "merge"
  credential_ids: ["cred-section9-api"] # must exist in credentials.yaml
  sync_interval_seconds: 60            # background sync; 0/absent = manual only
  ttl_seconds: 3600                    # dataset-level TTL
  timeout_seconds: 300                 # abort a sync that runs longer (default 300)
  ttl_overrides:                       # optional per-group / per-host TTLs
    groups:
      production: 900
    hosts:
      motoko.section9.net: 300
  config:                              # free-form strings passed to the script
    scenario: "large"
```

| Field | Notes |
|---|---|
| `script_path` | Executable run by the script connector, or remote command/facts selector for SSH |
| `connector_type` | `script` runs a local process; `ssh` fans out over hosts (see [connectors](connectors.md)) |
| `sync_mode` | How a **full** sync lands in the cache: `replace` swaps the dataset, `merge` patches it |
| `ttl_*` | See [caching](caching.md) for the freshness model |
| `timeout_seconds` | Hard limit on connector execution; a timed-out sync fails with a clear error instead of hanging its scheduler task or HTTP request |
| `config` | Arbitrary `key: value` strings the connector script receives as JSON. The SSH connector reads `hosts`, `port`, `concurrency`, `ssh_connect_timeout_seconds` (per-host, default 30), `gather_mode`, `fact_path` from here — see [connectors](connectors.md) |

## credentials.yaml

Credentials **never contain secrets** — they describe *where* to read them
(environment variables or files that the infrastructure injects).

```yaml
cred-section9-api:
  name: "Section 9 API"
  type: "username_password"    # username_password | token | ssh_key
  env_prefix: "SECTION9"       # reads SECTION9_USERNAME, SECTION9_PASSWORD...
  secret_keys:                 # our key -> env var suffix (or JSON field)
    username: "USERNAME"
    password: "PASSWORD"

cred-ssh-infra:
  name: "SSH Infrastructure"
  type: "ssh_key"
  env_prefix: "INFRA_SSH"
  secret_keys:
    username: "USERNAME"
  file_keys:                   # our key -> file path; passed as <key>_path
    ssh_key: "/run/secrets/infra-ssh/id_rsa"
```

Resolution order per credential: `env_prefix` (environment variables) or
`secret_file` (a JSON file), plus `file_keys` entries which are passed through as
`<key>_path` values. A credential that fails to resolve **fails the sync** with a
clear error — it is never silently skipped.

## enrichers.yaml

Post-processors over data already in the cache.

```yaml
enrich-resolve-ssh:
  name: "Resolve SSH reachability"
  source_id: "src-section9"        # whose cached dataset to enrich
  script_path: "enrichers/resolve.py"
  sync_interval_seconds: 300       # scheduled run; 0/absent = manual only
  timeout_seconds: 300             # abort a run that takes longer (default 300)
  config: {}
```

## endpoints.yaml

Output endpoints combine one or more cached datasets through a transformer script.

```yaml
ep-ansible-full:
  name: "Full Ansible Inventory"
  source_ids: ["src-section9", "src-infra"]
  script_path: "tests/adapters/out/output/ansible_inventory.py"
  timeout_seconds: 300              # abort a transform that takes longer (default 300)
  config:
    filter_datacenter: "section9"   # free-form, script-specific
```

## projects.yaml

Git repositories that hold connector/transformer scripts. The clone/pull feature
is on the roadmap; today the file is loaded and its references are validated
(`source.project_id` must exist here; a project's `credential_id` must exist in
credentials).

```yaml
prj-connectors-infra:
  name: "Infrastructure Connectors"
  git_url: "https://github.com/OpusProjects/connectors-infra.git"
  branch: "main"                  # default "main"
  credential_id: "cred-github-token"  # optional, for private repos
  sync_interval: "0 */30 * * *"
```

## api_keys.yaml

API keys with per-consumer permissions. The secret is NEVER here — `env` names
the environment variable that holds it, so this file can live in git and
rotation stays an external process (swap the env var, restart).

```yaml
key-awx:
  name: "AWX"
  env: "UNIFIED_API_KEY_AWX"
  role: "admin"                    # everything

key-forms:
  name: "AnsibleForms"
  env: "UNIFIED_API_KEY_FORMS"
  # role defaults to "restricted": only what is listed below
  sources: ["src-ssh-linux"]       # may list/read/sync these sources
  endpoints: ["ep-ansible-full"]   # may list/run these endpoints
```

A declared key whose env var is missing or empty fails startup (a typo must
not silently lock a consumer out at request time). Restricted keys referencing
unknown source/endpoint ids also fail startup. No file and no
`UNIFIED_API_KEY` = open API (with a loud warning). See
[API → Authentication](api.md#authentication) for the exact route semantics.

## Environment variables

| Variable | Effect |
|---|---|
| `CONFIG_DIR` | Config directory (default `config`) |
| `UNIFIED_API_KEY` | Legacy single admin key: when set, it authenticates like an `api_keys.yaml` admin entry (see [API](api.md)); health probes and Swagger stay public |
| per `api_keys.yaml` | Each key definition names the env var holding its secret |
| `RUST_LOG` | Log filter, e.g. `debug` or `unified_api=debug` (default `info`) |
| `<PREFIX>_<SUFFIX>` | Secret values referenced by `credentials.yaml` `env_prefix`/`secret_keys` |
