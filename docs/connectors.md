# Connectors, enrichers & outputs

Everything pluggable in Unified API is an **executable** speaking JSON. Any language
works — the shipped examples under `tests/` are Python. This page defines the
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

**Input (command line):** the source's `script_args` list is passed verbatim as
CLI arguments (no shell involved, so no quoting concerns). This is how scripts
that implement the standard Ansible dynamic inventory interface get their
`--list`:

```yaml
src-d42:
  script_path: "d42_inventory.py"
  script_args: ["--list"]
  output_format: "ansible"   # see below — such scripts emit Ansible JSON
```

Without `script_args` the script is invoked bare, exactly as before.

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

### Ansible inventory scripts (`output_format: ansible`)

Scripts written for Ansible print a different JSON shape than the Dataset:
hostvars under `_meta.hostvars` and groups as top-level keys. With
`output_format: "ansible"` on the source, that output is converted to a
Dataset on the fly — any existing dynamic inventory script works unmodified
(pair it with `script_args: ["--list"]`):

```yaml
src-d42:
  script_path: "d42_inventory.py"
  script_args: ["--list"]
  output_format: "ansible"
```

Conversion rules:

- `_meta.hostvars` → `hostvars`. A missing `_meta` is accepted with a warning
  (hosts will have no variables).
- Every other top-level key becomes a group. Both the object form
  (`{hosts, children, vars}`) and the legacy list form (`"web": ["h1", "h2"]`)
  are accepted.
- The implicit meta-groups `all` and `ungrouped` are skipped; if they carried
  `vars` or `children`, a warning says so (that information has no Dataset
  equivalent).
- Malformed input is an **error that fails the sync**, naming the offending
  group — never a silent skip.

**Misconfiguration safety net:** if a source left on the default
`output_format: native` parses to 0 hosts and 0 groups but the output contains
`_meta`, the sync logs a WARN suggesting `output_format: "ansible"`. (Both
Dataset fields are optional in JSON, so Ansible output "parses fine" as an
empty inventory — that silent zero is the failure mode this flag exists for.)

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

In `script` mode, `script_args` are appended to the remote command
(`script_path arg1 arg2 ...`); in `facts` mode they are ignored (the remote
command is fixed).

### Dynamic host lists (`hosts_from_source`)

Instead of a static `config.hosts`, an SSH source can take its hosts from
**another source's cached dataset** — the natural chain of "the inventory
source says WHAT exists, SSH says HOW it is doing":

```yaml
src-fleet-facts:
  connector_type: "ssh"
  credential_ids: ["cred-fleet-ssh"]
  sync_interval_seconds: 300
  ttl_seconds: 600
  hosts_from_source:
    source: "src-netbox"              # any source: script, static, even ssh
    match_pattern:                    # absent = every host in the dataset
      groups: ["linux", "proxmox_vms"]
      hosts: ["extra01.example.com"]
    connect_via: "ansible_host_then_hostname"
  config:
    gather_mode: "facts"
```

Semantics:

- `match_pattern` selects the **union** of the listed groups' members and the
  individually listed hosts; names match exactly. A group or host that doesn't
  exist in the origin dataset logs a warning naming it.
- The list is resolved against the **cache** at each sync. If the origin
  source hasn't synced yet, the SSH sync fails with a clear error and recovers
  on the next tick once the origin is cached (with disk persistence this only
  happens on the very first boot). `hosts_from_source` and `config.hosts` are
  mutually exclusive (startup validation).
- `connect_via` picks the address to dial per host: `hostname` (default, the
  inventory name via DNS), `ansible_host` (the variable; hosts without it are
  skipped with a warning), or the fallback combos `ansible_host_then_hostname`
  / `hostname_then_ansible_host`. With a fallback, candidates are tried in
  order and a **connection** failure (timeout, refused, DNS) moves to the next
  one — an authentication failure does not (it's the same server answering).
  Results are always keyed under the inventory hostname, whichever address
  connected.

**Finding the troublemakers:** every failed attempt logs a WARN with the host,
the address tried and the attempt number; successful hosts log their duration
at DEBUG; and the sync ends with a single summary line listing every
unreachable host (`failed_hosts=[...]`). A slow or dead host never delays the
others — it just occupies one of the `concurrency` slots until its timeout
(up to 2× `ssh_connect_timeout_seconds` with a fallback strategy).

> `ssh_connect_timeout_seconds` bounds a **single host** connection; the
> source-level `timeout_seconds` (default 300) separately bounds the **whole
> sync** across all hosts. They are different knobs.

**Credentials:** expects `username` (or `ssh_username`) and an `ssh_key_path` /
`key_path` from `file_keys` — see [configuration](configuration.md).

## Static inventory sources (`connector_type: static_inventory`)

For classic Ansible **static YAML inventories** — an `inventory.yaml` with the
`all/children/hosts` tree plus optional `group_vars/` and `host_vars/`
directories next to it. No process is spawned and no `ansible-core` is
needed: the files are parsed natively.

```yaml
src-inventory-linux:
  name: "Linux static inventory"
  connector_type: "static_inventory"
  project_id: "prj-inventories"        # git repo holding the inventory
  script_path: "inventory.yaml"        # path to the file inside the checkout
  sync_interval_seconds: 300
  ttl_seconds: 600
```

`script_path` doubles as "path to the inventory file"; with a git project it
resolves inside the checkout, so the project's periodic pull (or the
on-demand `POST /api/v1/projects/{id}/sync`) is what brings in new data — the
next source sync reads the updated files. `script_args`, `output_format`,
credentials and `SOURCE_CONFIG` don't apply to this connector.

**What lands where:**

- Hosts get their **effective variables flattened** into `hostvars`, merged in
  this precedence (lowest first): `all` inline vars → `group_vars/all` →
  each group containing the host (parents before children, alphabetical at
  the same depth; inline vars then `group_vars/<group>` per group) → the
  host's inline vars → `host_vars/<host>`. This is a simplified version of
  Ansible's own ordering; exotic overlaps may differ.
- Groups keep their structure: direct `hosts`, `children`, and the group's
  own (unflattened) vars. The implicit `all`/`ungrouped` are not emitted as
  groups — `all`'s vars reach every host through the flattening.

**Deliberately unsupported (loud, never silent):**

- INI inventories — YAML only
- host range patterns (`web[01:20].example.com`) → the sync fails
- ansible-vault encrypted files or values → the sync fails naming the file
- Jinja templating: `{{ ... }}` values pass through as literal strings
  (templating belongs to the consumer, e.g. Ansible itself)
- `group_vars`/`host_vars` files that match nothing log a warning

## Enrichers

An enricher post-processes a dataset already in the cache: resolve DNS, probe
reachability, tag hosts, drop stale entries.

**Input:** `SOURCE_CONFIG` env var (the enricher's `config`), the enricher's
`script_args` as CLI arguments (default: none), and the **current dataset on
stdin** as JSON.

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
| CLI arguments | The endpoint's `script_args` list, verbatim (default: none) |
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
against the sample scripts under `tests/adapters/out/` (`connectors/`, `enrichers/`, `output/`).
