# Connectors, enrichers & outputs

Everything pluggable in Unified API is an **executable** speaking JSON. Any language
works — the shipped examples in `test-connectors/` are Python. This page defines the
three contracts.

## The Dataset shape

All three script types deal in the same JSON document:

```json
{
  "hostvars": {
    "motoko.section9.net": { "ansible_host": "10.9.1.1", "os": "OracleLinux" }
  },
  "groups": {
    "section9": {
      "hosts": ["motoko.section9.net"],
      "children": [],
      "vars": { "ntp_server": "ntp.section9.net" }
    }
  },
  "remove_hosts": []
}
```

Every field is optional (`hostvars`/`groups` default to empty). `remove_hosts` is
only meaningful in enricher output.

## Source connectors (`connector_type: script`)

The connector script is executed on every sync and must print a Dataset to stdout.

**Input (environment variables):**

| Variable | Content |
|---|---|
| `SOURCE_CONFIG` | The source's `config` map as a JSON object. On scoped syncs it additionally carries `scope` (`host`/`group`) and `target` |
| `CREDENTIAL_<KEY>` | One per resolved credential key, uppercased — e.g. `CREDENTIAL_USERNAME`, `CREDENTIAL_PASSWORD`, `CREDENTIAL_SSH_KEY_PATH` |

**Output:** the full Dataset JSON on stdout. Exit non-zero to fail the sync; stderr
is captured into the error message.

**Time limit:** the script must finish within the source's `timeout_seconds`
(default 300). A slower run is aborted and the sync fails with
`sync timed out after Ns` — a hung script never blocks the scheduler or an API call.

Minimal example:

```python
#!/usr/bin/env python3
import json, os

config = json.loads(os.environ.get("SOURCE_CONFIG", "{}"))
token = os.environ.get("CREDENTIAL_TOKEN")

inventory = fetch_from_backend(token, scope=config.get("scope"), target=config.get("target"))
print(json.dumps({"hostvars": inventory.hosts, "groups": inventory.groups}))
```

Supporting `scope`/`target` is optional but recommended: it lets consumers refresh a
single host or group without paying for a full inventory pull.

## Source connectors (`connector_type: ssh`)

The native SSH connector needs no script on the API host — it connects to the fleet
in parallel and builds the Dataset from what it finds.

**Source `config` keys:**

| Key | Default | Meaning |
|---|---|---|
| `hosts` | — | Comma-separated hostnames to connect to |
| `port` | `22` | SSH port |
| `concurrency` | `50` | Max parallel connections (tokio semaphore) |
| `ssh_connect_timeout_seconds` | `30` | Per-host connection/exec timeout |
| `gather_mode` | `facts` | `facts` reads Ansible local facts; `script` runs `script_path` remotely |
| `fact_path` | `/etc/ansible/facts.d` | Where facts live (facts mode) |

> `ssh_connect_timeout_seconds` bounds a **single host** connection; the
> source-level `timeout_seconds` (default 300) separately bounds the **whole
> sync** across all hosts. They are different knobs.

**Credentials:** expects `username` (or `ssh_username`) and an `ssh_key_path` /
`key_path` from `file_keys` — see [configuration](configuration.md).

## Enrichers

An enricher post-processes a dataset already in the cache: resolve DNS, probe
reachability, tag hosts, drop stale entries.

**Input:** `SOURCE_CONFIG` env var (the enricher's `config`), and the **current
dataset on stdin** as JSON.

**Output:** a *partial* Dataset on stdout — only what changed:

- `hostvars` entries are merged over the existing ones (per-host timestamps refresh)
- `groups` entries replace their counterparts
- `remove_hosts` lists hostnames to delete (they're also pulled out of groups)

The merge into the cache is atomic; concurrent writes that land while the enricher
script is running are not lost (the enricher only overwrites hosts it returns).

## Output endpoints

An output script transforms one or more cached datasets into whatever a consumer
needs — the shipped example renders a merged Ansible inventory.

**Input:**

| Channel | Content |
|---|---|
| `ENDPOINT_CONFIG` env var | The endpoint's static `config` as JSON |
| `ENDPOINT_PARAMS` env var | The JSON body of the `POST` request (`{}` if none) |
| stdin | `{ "<source_id>": <Dataset>, ... }` for every configured source |

**Output:** anything on stdout — it is returned to the HTTP caller verbatim.

**Time limit:** the endpoint's `timeout_seconds` (default 300); exceeding it returns
`504 Gateway Timeout` to the caller. Enrichers have the same knob and fail with a
clear error when exceeded.

## Testing your script

Wire it into `config/sources.yaml` (or enrichers/endpoints) pointing at your file,
`cargo run`, then drive it through the API:

```bash
curl -X POST localhost:8182/api/v1/sources/src-mine/sync
curl localhost:8182/api/v1/sources/src-mine/dataset
```

For automated tests, follow the patterns in `tests/` — the suite runs entirely
against the fake scripts in `test-connectors/`.
