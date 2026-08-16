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
| `SOURCE_CONFIG` | The source's `config` map as a JSON object. On scoped syncs it additionally carries `scope` (`host`/`group`) and `target`; every sync carries `trigger` — the HTTP request id for a manual sync, `scheduled` or `refresh` otherwise — so a script's own logs can join the request's trace. Projects with a virtualenv also see `python_venv_bin` |
| `CREDENTIAL_<KEY>` | One per resolved credential key, uppercased — e.g. `CREDENTIAL_USERNAME`, `CREDENTIAL_PASSWORD`, `CREDENTIAL_SSH_KEY_PATH` |

These are the only variables the service sets, and the rest of the script's
environment is **scrubbed**: apart from a small passthrough list (`PATH`,
`HOME`, `TMPDIR`, `LANG`/`LC_ALL`, `TZ`, `PYTHONPATH`, and the usual proxy and
CA-bundle variables), nothing from the service's own environment reaches the
script — in particular not the API-key secrets or other sources' credentials.
Anything else a script needs must travel in its `config` map or a credential.

**Output:** the full Dataset JSON on stdout. Exit non-zero to fail the sync; stderr
is captured into the error message.

**Time limit:** the script must finish within the source's `timeout_seconds`
(default 300). A slower run is aborted and the sync fails with
`sync timed out after Ns` — a hung script never blocks the scheduler or an API call.
The process is **killed**, not merely abandoned, so a script that wedges does not
accumulate a live copy per sync interval. Write scripts to be interruptible at any
point: a partially-written file or a half-finished remote change will not be
cleaned up for you.

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
| `ssh_legacy_algorithms` | `false` | `"true"` additionally offers SHA-1 KEX/MACs (appended last, modern servers still pick modern ones) for OpenSSH 5.x-era hosts (EL6) that lack `hmac-sha2` |
| `ssh_known_hosts` | — | Path to an OpenSSH `known_hosts` file. When set, every server key is checked against it **before authentication**: an unknown or mismatched host is refused (it lands in `unreachable`, so its last known data is kept), and the refusal logs both fingerprints. Unset = any server key is accepted, and every sync warns about it |

**Host key verification:** collect the fleet's keys with `ssh-keyscan` (append
`-p` entries for non-22 ports) and point `ssh_known_hosts` at the file. Plain,
`[host]:port` and hashed (`HashKnownHosts`) entries are all understood;
`@cert-authority`/`@revoked` markers are not (such lines are skipped with a
warning, which fails closed — a key recorded only behind a marker counts as
unrecorded). The file is re-read on every sync, so rotating a host key is an
edit, not a restart. Startup validation fails fast if the path does not exist.

**RSA keys across mixed fleets:** the signature hash is negotiated per host
automatically. Servers advertising `server-sig-algs` (OpenSSH ≥ 7.2) get
their preferred `rsa-sha2-*` (or `ssh-rsa` if that's all they take); servers
without the extension are tried with SHA-256 first and retried with the
legacy SHA-1 signature if rejected. This covers everything from EL6 to
current RHEL9-era hosts, whose crypto policies reject SHA-1 signatures —
without negotiation, the same key "mysteriously" works with the OpenSSH
client but not through the API. ed25519/ecdsa keys are unaffected (no hash
to negotiate).

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
- The list is resolved against the **cache** at each sync. At startup this source
  waits for the origin to have data before its first sync — up to five minutes —
  rather than racing it and failing, since every source's first tick fires at
  once. If the origin still has not synced by then, the sync runs and fails with
  a clear error (which lands in `sync_health`), and recovers on the next tick
  once the origin is cached. Only the first sync waits: after startup an absent
  origin is a real failure and is reported immediately. `hosts_from_source` and
  `config.hosts` are mutually exclusive (startup validation).
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

A group may be declared more than once — under two parents, or twice under the
same one. The declarations are **merged**: hosts, children and group vars all
accumulate, and a host inherits the vars of every ancestry its group is declared
under. Naming the same host in both declarations lists it once.

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

## Remote sources — federation (`connector_type: remote`)

Another unified-api instance is the source. This is how multi-datacenter
setups compose: one instance per DC does the local work (SSH with LAN
latency, local credentials, local firewall rules), and a central instance
aggregates them — consumers only ever talk to the central.

```yaml
# on the central
src-dc1:
  name: "Datacenter A"
  connector_type: "remote"
  project_id: "prj-unused"          # required by schema; unused here
  script_path: "src-ssh"            # the source id ON THE REMOTE instance
  credential_ids: ["cred-edge-dc1"] # token credential = the remote API key
  sync_interval_seconds: 120
  ttl_seconds: 600
  config:
    url: "https://unified-api-dc1.example.com"
    # http_timeout_seconds: "30"    # default 30
    # insecure_tls: "true"          # self-signed remotes; opt-in, never default
```

On the edge, give the central a **restricted API key** (least privilege):

```yaml
# api_keys.yaml on the edge
key-central:
  name: "Central aggregator"
  env: "UNIFIED_API_KEY_CENTRAL"
  sources: ["src-ssh"]
```

**How it works:** `GET /dataset` on the remote returns exactly the Dataset a
connector must produce — the API itself is the federation protocol. A second
call to `/status` recovers the data's real age at the origin (dataset-level
and per-host), and the local cache entry is built with those ages: freshness
reporting stays truthful across hops instead of resetting to zero on every
transfer. If the `/status` call fails, the data still lands (with a warning)
and is treated as fresh — data beats metadata.

Failure semantics follow the house rules: `401`/`403`/`404` produce errors
that say what to check; a WAN cut fails the sync loudly while the central
keeps serving the last good dataset from its cache (stale beats nothing).
Centrals can themselves be federated by another instance — regions → global
with no extra machinery. Combine with an output endpoint listing all the DC
sources to serve one merged world view.

## Enrichers

An enricher post-processes a dataset already in the cache: resolve DNS, probe
reachability, tag hosts, drop stale entries. This section is the script I/O
contract; triggers, ordering, freshness rules and health live in
[enrichers](enrichers.md).

**Input:** `SOURCE_CONFIG` env var (the enricher's `config`), the enricher's
`script_args` as CLI arguments (default: none), and the **current dataset on
stdin** as JSON. The environment is scrubbed the same way as for connector
scripts (see above).

**Output:** a *partial* Dataset on stdout — only what changed:

- `hostvars` entries are merged over the existing ones
- `groups` entries replace their counterparts
- `remove_hosts` lists hostnames to delete (they're also pulled out of groups)

**Enrichment does not make a host look freshly gathered.** The per-host timestamps
record when a host was last *collected*, and a read consults them to decide whether
to refresh before answering — so a host already in the entry keeps the age it had.
A host the enricher *introduces* is stamped, because that is when it became known.
Returning a host is therefore not a way to suppress a refresh, and enriching a stale
host leaves it stale.

The merge into the cache is atomic; concurrent writes that land while the enricher
script is running are not lost (the enricher only overwrites hosts it returns).

> **Return the host's full variable map, not only your own keys.** A script
> enricher's `hostvars` entry *replaces* the host's map rather than patching it key
> by key, so any key you omit is dropped. The dataset arrives on stdin precisely so
> you can carry the rest through. (A *declarative* enricher — `source_id` +
> `fields` — has no such constraint: it writes only the fields it owns.)

## Output endpoints

An output script transforms one or more cached datasets into whatever a consumer
needs — the shipped example renders a merged Ansible inventory. This section is
the script I/O contract; routes, GET-vs-POST semantics, permissions and failure
shapes live in [endpoints](endpoints.md).

**Input:**

| Channel | Content |
|---|---|
| CLI arguments | The endpoint's `script_args` list, verbatim (default: none) |
| `ENDPOINT_CONFIG` env var | The endpoint's static `config` as JSON |
| `ENDPOINT_PARAMS` env var | The JSON body of the `POST` request (`{}` if none) |
| stdin | `{ "<source_id>": <Dataset>, ... }` for every configured source |

The environment is scrubbed the same way as for connector scripts (see above).

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
