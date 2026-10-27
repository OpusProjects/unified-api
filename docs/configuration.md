# Configuration

Everything is YAML in one directory (default `./config`, override with the
`CONFIG_DIR` environment variable), laid out the way AWX config as code is:
`config.yaml` at the root holds the server settings, and every other `*.yaml` /
`*.yml` file, **at any depth**, holds items under a typed top-level key.
Only `config.yaml` is mandatory; a directory with nothing else is a valid,
empty inventory.

All cross-references are **validated at startup** — an unknown source, connector,
credential or project anywhere in the config aborts boot with a list of every
error found. The same validation runs standalone with `unified-api --check-config`
(see [CLI](cli.md#validating-configuration---check-config)) — for CI on a config repository.

- [The directory](#the-directory)
- [config.yaml (required)](#configyaml-required)
- [sources](#sources)
- [connectors](#connectors)
- [views](#views)
- [credentials](#credentials)
- [enrichers](#enrichers)
- [endpoints](#endpoints)
- [projects](#projects)
- [api_keys](#api_keys)
- [Upgrading from 0.34](#upgrading-from-034)
- [Environment variables](#environment-variables)

Configuration is read at startup, and can also be **written and reloaded over
HTTP** where that is enabled — see [Configuration API](config-api.md).

---

## The directory

```
/etc/unified-api/
├── config.yaml                        # required, at the root, under this name
├── sources/                           # folders are for people: any names, any depth, or none
│   ├── acme/
│   │   ├── src-d42-acme.yaml
│   │   └── src-ssh-acme.yaml
│   └── src-netbox.yaml
├── connectors/
│   ├── d42.yaml
│   └── ssh-facts.yaml
├── views/
│   └── vw-facts-all.yaml
├── credentials/
│   └── cred-d42.yaml
├── projects/
│   └── prj-inventory.yaml
└── api_keys/
    └── key-awx.yaml
```

| Top-level key | Holds |
|---|---|
| `sources` | Each inventory instance: its connector, credentials, config, schedule and TTLs |
| `connectors` | How to talk to a kind of system: the script (or SSH command), its project, its arguments |
| `views` | Read-only composites over several sources, routed by declared ownership |
| `endpoints` | Outputs combining cached datasets through a transformer |
| `enrichers` | Post-processors over data already in the cache |
| `projects` | Git repositories holding scripts and static inventories |
| `credentials` | Where each credential's secrets are read from (never the values) |
| `api_keys` | API keys by name and role (the values come from env vars) |

Every item file looks the same: a key, and a list of items under it.

```yaml
# sources/acme/src-d42-acme.yaml
sources:
  - name: src-d42-acme              # identity: URLs, references, cache keys
    description: "Device42 Acme"    # label, for people
    connector: d42
    credentials: [cred-d42]
    sync_interval_seconds: 300
    ttl_seconds: 3600
```

The rules, and why they are what they are:

- **The key says what an item is, never the folder.** A file in `connectors/`
  holding `sources:` is read as a source. Folders exist for people — by team, by
  estate, by datacenter — and may nest as deep as they like, or not exist at all.
  Tying meaning to a folder would make moving a file a semantic change.
- **One item per file is the convention**, `<section>/<name>.yaml`, and the one the
  [upgrade](#upgrading-from-034) writes. It is not a rule: a file may hold several
  items, of several types. A review of "add a tenant" is then one new file, not a
  hunk in the middle of a 2,000-line map.
- **`name` is the identity**, as in AWX: what other items reference, what URLs
  (`/api/v1/sources/{id}`) and cache keys use. Letters, digits, `-`, `_` and `.`,
  not starting with a dot — it appears in URLs and in file names. The values are
  the same ids 0.34 used as map keys, so nothing that reads the API changes.
- **`description` is optional, for people.** Every API response field called
  `name` answers the description, falling back to the identity when there is
  none — so responses read exactly as they did before item files.
- **A `name` defined twice is an error naming both files.** Which one won would
  otherwise depend on the order files happen to be read in.
- **References drop the `_id`:** a source names its `connector` and
  `credentials`, an enricher its `target`, `source` and `project`, an endpoint its
  `sources` and `project`, a project its `credential`.
- **Hidden files and folders are skipped** (editor swap files, `.git`, the config
  API's staging folders, the upgrade's backup). **Symlinks are followed**: a
  Kubernetes ConfigMap mount is nothing but links into a hidden `..data` folder,
  and skipping them would load an empty configuration.
- **A path may only use letters, digits, `-`, `_` and `.`** in each segment, none
  starting with a dot, ending in `.yaml` or `.yml` — the same rule the
  [config API](config-api.md) applies to what it is asked to write, so a file the
  API cannot name cannot be part of the configuration either.

Parsing is **strict**. Every way a file can be wrong fails startup naming the
file, and every error is reported at once, so a push with three typos in three
files names all three:

| Mistake | Error |
|---|---|
| An unknown top-level key | `sources/a.yaml: unknown key 'sourcse' (expected one of: sources, views, ...)` |
| A 0.34 file pushed as-is | the same, ending `— this is the 0.34 layout (items keyed by id); convert it with unified-api --migrate-config <dir>` |
| An unknown key inside an item | ``sources/a.yaml: source 'src-a': unknown field `ttl_second`, expected one of ...`` |
| An item without `name` | `sources/a.yaml: sources item 2 needs a 'name', its identity` |
| A name defined twice | `sources/b.yaml: source 'src-a' is already defined in sources/a.yaml` |

A typo'd key failing loudly is the point: silently applying the default for
`ttl_second:` is how an inventory ends up an hour stale with nobody knowing why.
The free-form `config:` maps are the exception — they are arbitrary by design,
and their scalars are taken as text, exactly as written (`concurrency: 20`
arrives as `"20"`, `version: 1.10` as `"1.10"`, `account: 0x1F` as `"0x1F"`), since
they become environment variables and script settings. Every other text field
reads an unquoted scalar the same way: `script_args: ["--days", 7]` is
`["--days", "7"]`.

---

## config.yaml (required)

The one file that must exist, holding the `server`, `cache`, `projects` and `secrets` settings.
It is always at the root, under exactly this name, and holds no items.

```yaml
server:
  host: "0.0.0.0"
  port: 8182
  # Optional. Empty/absent (default) = no CORS headers at all — right for
  # server-to-server consumers. List browser origins to enable, "*" for any.
  cors_allowed_origins: []
  # Optional, default false. false = /readyz is green once ANY source has
  # synced (a pod serving part of the inventory beats one serving nothing).
  # true = every configured source must have synced first — for deployments
  # where a partial inventory is worse than none.
  readyz_require_all_sources: false
  # Optional, default false. false keeps /metrics public alongside the health
  # probes, which is what Prometheus scrape configs expect. true requires an
  # API key — for a shared network, since the exposition labels every source
  # id and host count.
  metrics_require_auth: false
  # Optional, default 15. How long a read with ?refresh=true may wait before it
  # gives up and serves the cached data. Sized for a consumer waiting on a page,
  # not for a scheduled sync. See docs/on-demand-refresh.md.
  refresh_timeout_seconds: 15
  # Optional, default 8. Concurrent on-demand refreshes, process-wide. The TTL
  # bounds repeat requests for the same host; this bounds requests for many
  # different hosts arriving together.
  refresh_max_concurrent: 8
  # Optional, default 20. How long shutdown waits for background tasks (syncs,
  # enrichers, project pulls, the snapshot task) to finish in-flight work
  # before writing the final cache snapshot anyway. Size it to fit inside the
  # pod's terminationGracePeriodSeconds with room for the snapshot write.
  shutdown_grace_seconds: 20
  # Optional, default 2097152 (2 MiB — what was always enforced, implicitly).
  # Largest request body accepted on any route; an oversized one answers 413
  # naming this key. Size it for the biggest configuration push (a whole
  # directory is one body) or host-vars write you expect.
  max_body_bytes: 2097152
  # Optional. This instance's address, as people and other instances reach it
  # (the ingress, not the port above). A central sends it with the federation
  # map, so each edge's Topology page links back to it. Reloadable.
  url: "https://unified-api-central.example.com"

# Optional. Absent = purely in-memory cache (restarts start empty).
# With the block, the cache is snapshotted to `path` every `interval_seconds`
# (default 60) and on graceful shutdown, and reloaded at boot. See docs/caching.md.
cache:
  persistence:
    path: "/var/lib/unified-api/cache.json"
    interval_seconds: 60

# Optional. Where git projects are cloned (one subdirectory per project name).
projects:
  dir: "/var/lib/unified-api/projects"   # default "projects"

# Optional. Credential resolution behavior.
secrets:
  # How long a resolved credential is reused before the backend is asked
  # again (default 60; 0 = resolve fresh on every sync). Resolution runs on
  # every sync of every source — free against env vars, a request storm
  # against a networked backend. This is also the rotation latency: a rotated
  # secret is picked up within this many seconds.
  cache_ttl_seconds: 60

# Optional, OFF by default. Lets an admin key read, write and reload this
# directory over HTTP, so a pipeline can push a change instead of publishing
# an artifact the instance has to pull. Turning it on means an admin key can
# rewrite every file the loader reads, the api_keys items included — the same
# authority as editing the mounted directory. See docs/config-api.md.
config_api:
  enabled: false
```

---

## sources

One item per inventory **instance**: which system, reached how (its
`connector`), as whom (`credentials`), with which settings (`config`), how often,
and how long the data stays fresh. The name is the source id used in URLs.

```yaml
# sources/src-section9.yaml
sources:
  - name: src-section9
    description: "Section 9 Inventory"
    connector: "sample-inventory"        # a connector item, or a built-in (below)
    credentials: ["cred-section9-api"]   # credential items
    sync_mode: "replace"                 # "replace" (default) or "merge"
    sync_interval_seconds: 60            # background sync; 0/absent = manual only
    # schedule: "30 2 * * *"             # OR a cron cadence (UTC) — pick one
    ttl_seconds: 3600                    # dataset-level TTL (required)
    # timeout_seconds: 900               # override the connector's bound, this source only
    ttl_overrides:                       # optional per-group / per-host TTLs
      groups:
        production: 900
      hosts:
        motoko.section9.net: 300
    host_sync_updates_group_vars: false  # may a host-scoped sync take the vars of
                                         #   the groups it joins? (default false)
    allow_on_demand_refresh: false       # may a READ trigger a sync? (default false)
    config:                              # free-form strings passed to the connector
      scenario: "large"
```

| Field | Notes |
|---|---|
| `connector` | Required. The name of a [connector](#connectors) item, or a built-in: `remote` (another unified-api, see [federation](federation.md)) or `static_inventory` (an Ansible YAML inventory in a project checkout). See [connectors](connectors.md) |
| `credentials` | Credential items resolved before each sync and handed to the connector |
| `project`, `path` | `static_inventory` sources only: the project whose checkout holds the inventory, and the file inside it. Refused on every other source, where they would look like they select a script and would not |
| `timeout_seconds` | Overrides the connector's `timeout_seconds` for this source only — the bigger tenant of the same system. Absent = the connector's (default 300). A timed-out sync fails with a clear error instead of hanging its scheduler task or HTTP request |
| `host_sync_updates_group_vars` | Whether a host-scoped sync also takes the vars of the groups it places those hosts in, and only those groups. Off by default: a granular call answers about a host, not about what a group means for its other members |
| `sync_mode` | How a **full** sync lands in the cache: `replace` swaps the dataset, `merge` patches it |
| `ttl_*` | See [caching](caching.md) for the freshness model |
| `allow_on_demand_refresh` | Whether `GET /dataset?host=X&refresh=true` may gather the named hosts. Off by default: a read that can cause SSH into a datacenter is a capability. Turning it on makes `ttl_seconds` load bearing — it becomes the threshold at which a read pays for a gather — so review it first. See [on-demand refresh](on-demand-refresh.md) |
| `schedule` | Cron cadence instead of an interval: standard 5-field cron, optional leading seconds field, evaluated in **UTC**. Validated at startup; mutually exclusive with a non-zero `sync_interval_seconds`. Cron sources get no startup jitter (their times are deliberate) but back off on failure like interval sources, by letting occurrences pass |
| `hosts_from_source` | SSH sources only: dynamic host list from another source's cached dataset (`source` + optional `match_pattern.groups`/`hosts` + `connect_via`); mutually exclusive with `config.hosts` — see [connectors](connectors.md#dynamic-host-lists-hosts_from_source) |
| `advertise_scope` | Explicit ownership claim served on `GET /sources/{id}/scope` (`groups` + `hosts`; must name at least one). Optional — an SSH source already advertises its `hosts_from_source` pattern; this overrides the derivation. The consumer side of federation reads it so view ownership stops being a duplicated truth |
| `config` | Arbitrary `key: value` strings the connector receives (`SOURCE_CONFIG` for a script). The SSH connector reads `hosts`, `port`, `concurrency`, `ssh_connect_timeout_seconds` (per-host, default 30) and `ssh_known_hosts` (host key verification — unset accepts any key, with a warning per sync) from here (0.35's `gather_mode` and `fact_path` are refused: the connector decides what runs); a `remote` source reads `url` and `source` — see [connectors](connectors.md) |

### Source or connector?

What a source used to say about its script now lives on a connector, so ten
Device42 tenants are ten sources and **one** connector instead of ten copies of
the same script path, arguments and output format that drift apart one edit at a
time. The split follows one question — *does it describe the script, or this
instance of the system?*

| Field | Lives on | Why |
|---|---|---|
| `type` (script / ssh) | connector | The protocol |
| `project`, `path` | connector | Where the script is |
| `script_args`, `host_args` | connector | How the script is called |
| `output_format` | connector | What the script prints |
| `timeout_seconds` | connector, overridable per source | A property of the script, except for the tenant ten times bigger |
| `credentials`, `config` | source | Which instance, as whom |
| `schedule`, `sync_interval_seconds`, `ttl_seconds`, `ttl_overrides` | source | How often and how fresh, per instance |
| `sync_mode`, `host_sync_updates_group_vars` | source | Cache policy of this instance's entry, not a property of the script |
| `hosts_from_source`, `allow_on_demand_refresh`, `advertise_scope` | source | Per instance |

### Built-in connectors

Two connectors need no item: the protocol is fixed, and the only thing to
configure — where — is per source.

```yaml
sources:
  # Another unified-api instance (federation)
  - name: src-edge-dc2
    description: "Datacenter 2"
    connector: remote
    credentials: ["cred-edge-dc2"]     # a token credential = the remote API key
    sync_interval_seconds: 120
    ttl_seconds: 600
    config:
      url: "https://unified-api-dc2.example.com"
      source: "src-ssh"                # the source id ON THE REMOTE instance

  # An Ansible YAML inventory in a project checkout
  - name: src-linux
    description: "Linux static inventory"
    connector: static_inventory
    project: "prj-inventories"         # the checkout
    path: "linux/inventory.yaml"       # the file inside it
    sync_interval_seconds: 300
    ttl_seconds: 600
```

- `remote` requires `config.url` and `config.source`, and refuses `project` /
  `path`. See [federation](federation.md).
- `static_inventory` requires `project` and `path` on the source itself. See
  [connectors](connectors.md#static-inventory-sources-connector-static_inventory).

`project` + `path` mean the same thing everywhere they appear — a file inside
that project's checkout — whether it is a connector's script or a static
source's inventory.

---

## connectors

How to talk to a kind of system. Full treatment, including the script contract,
in [connectors](connectors.md).

```yaml
# connectors/d42.yaml
connectors:
  - name: d42
    description: "Device42 inventory script"
    type: "script"                      # script (default) | ssh
    project: "prj-inventory"            # the checkout `path` resolves in
    path: "dynamic/d42/inventory.py"
    script_args: ["--list"]             # CLI args, verbatim (no shell)
    host_args: ["--host", "{target}"]   # replace script_args for a host-scoped sync
    output_format: "ansible"            # native (default) | ansible
    timeout_seconds: 300                # default 300; a source may override it

# connectors/ssh-facts.yaml
connectors:
  - name: ssh-facts
    description: "SSH fact gathering"
    type: "ssh"
    location: "host"                    # path is on each host (or a script here,
    path: "/etc/ansible/facts.d/*.fact" #   with project / location: instance)
```

| Field | Notes |
|---|---|
| `type` | `script` (default) runs a local process; `ssh` connects to the fleet natively |
| `project` | The project whose checkout holds the script. Required for a script connector with a relative `path`; on ssh, the project of a script streamed to each host (refused with `location: host`) |
| `location` | ssh only: `host` (path is on each host: files run or read as facts) or `instance` (a script here, streamed to each host; the default with a `project`). See [connectors](connectors.md#ssh-connectors-type-ssh) |
| `path` | A file, always. script: the one to run, relative to the project checkout or absolute. ssh: a script here, or a file or pattern of files on each host |
| `script` | ssh only, instead of `path`: the script written in the item, streamed to each host as a file from here would be |
| `script_args` | CLI arguments, quoted. `["--list"]` makes standard Ansible dynamic inventory scripts work unmodified. On ssh, passed to the streamed script or to every executable a host path matches |
| `host_args` | Script only. CLI arguments for a sync that names hosts, replacing `script_args` for that run, `{target}` substituted by the requested hostnames (comma-joined). Empty = call the script as a full sync would and filter the answer. See [connectors](connectors.md#asking-for-one-host-on-the-command-line-host_args) |
| `output_format` | Script only. `native` (the Dataset shape) or `ansible` (dynamic inventory JSON with `_meta`, converted on the fly) |
| `timeout_seconds` | Bound on a sync through this connector (default 300); a source's own `timeout_seconds` overrides it |

Settings that look active and are not are refused rather than ignored:
`host_args` and `output_format` on an ssh connector, `project` with
`location: host`, `location` or `script` on a script connector, `path` and
`script` together, a relative script `path`
without a `project`, and the names `remote` and `static_inventory`, which belong
to the built-ins.

---

## views

Read-only composites over several sources: one id that routes a per-host read
to whichever member owns that host. Views answer on the **source routes**, share
the source id space (a collision fails startup) and hold no cache entry of their
own. Full treatment in [views](views.md).

```yaml
# views/vw-facts-all.yaml
views:
  - name: vw-facts-all
    description: "Facts, both datacenters"
    members:                            # ordered: the first claim wins
      - source: "src-ssh-dc1"           # where the facts come from
        owns:
          source: "src-d42"             # inventory the pattern resolves against
          groups: ["datacenter_dc1"]
      - source: "src-edge-dc2"          # a remote (federated) member works too
        owns:
          source: "src-d42"
          groups: ["datacenter_dc2"]
          hosts: ["appliance01.dc2.example"]   # claimed literally as well
    # ttl_seconds: 30                   # optional; absent = inherit the owner's
```

| Field | Meaning |
|---|---|
| `members` | Ordered list. Overlap resolves by declared order — the first member that claims a host wins it |
| `members[].source` | Source id this member serves data from; must exist and must not be another view |
| `members[].owns.source` | Source whose cached dataset the groups below resolve against |
| `members[].owns.groups` | Groups of that source whose members this member owns |
| `members[].owns.hosts` | Hosts claimed literally, whether or not the inventory knows them — a host built this morning is exactly the one somebody needs to reach |
| `members[].owns.advertised` | Route by what the member SOURCE claims to own (its `advertise_scope` locally, the edge's `GET /scope` for remote members, refreshed per sync with the last-known claim surviving an unreachable edge). `groups`/`hosts` become the fallback; with neither claim nor fallback the member claims nothing — see [views](views.md#advertised-ownership) |
| `ttl_seconds` | The view's own freshness policy, which is also the **gate** for `refresh=true`. Absent = each host inherits its owning member's TTL. A member's per-host/per-group `ttl_overrides` win either way |

Ownership is **declared, not inferred from what a member has cached**: facts
sources often sync daily on purpose, and some hosts (appliances that take no
SSH) never enter any cache at all — both are exactly the hosts on-demand refresh
exists for. See [views → ownership](views.md#ownership-is-declared-not-inferred).

Strict parsing matters most here. Ownership is the routing table, and `grups:`
instead of `groups:` would otherwise parse as an empty pattern, which claims
everything.

A view is granted to an API key exactly like a source, by its id under
`sources:`; the key needs no access to the members.

---

## credentials

Credentials **never contain secrets** — they describe *where* to read them;
this is the field reference, and the full resolution story (backends, the
cache and rotation, native Vault, failure semantics) is
[its own page](secrets.md).

```yaml
# credentials/cred-section9-api.yaml
credentials:
  - name: cred-section9-api
    description: "Section 9 API"
    type: "username_password"    # username_password | token | ssh_key
    env_prefix: "SECTION9"       # reads SECTION9_USERNAME, SECTION9_PASSWORD...
    secret_keys:                 # our key -> env var suffix (or JSON field)
      username: "USERNAME"
      password: "PASSWORD"

  - name: cred-ssh-infra
    description: "SSH Infrastructure"
    type: "ssh_key"
    env_prefix: "INFRA_SSH"
    secret_keys:
      username: "USERNAME"
    file_keys:                   # our key -> file path; passed as <key>_path
      ssh_key: "/run/secrets/infra-ssh/id_rsa"

  - name: cred-d42-vault
    description: "Device42 (from Vault)"
    type: "username_password"
    vault_path: "infra/device42"  # KV v2 path under secrets.vault.mount
    secret_keys:                  # our key -> field in the Vault secret
      username: "user"            # (omit secret_keys to take every field)
      password: "pass"
```

Resolution order per credential: `vault_path` (native Vault, see below), or
`env_prefix` (environment variables) / `secret_file` (a JSON file), plus
`file_keys` entries which are passed through as `<key>_path` values. A
credential that fails to resolve **fails the sync** with a clear error — it is
never silently skipped.

Native Vault resolution is opt-in via the `secrets.vault:` block in
`config.yaml` (a `vault_path` without it fails validation at startup):

```yaml
secrets:
  cache_ttl_seconds: 60          # see the config.yaml section above
  vault:
    address: "https://vault.example.com:8200"
    mount: "secret"              # KV v2 mount (default "secret")
    token_env: "VAULT_TOKEN"     # token auth: env var holding the token
    # kubernetes_role: "unified-api"   # OR Kubernetes auth: exchange the
    # jwt_path: "/var/run/secrets/kubernetes.io/serviceaccount/token"
    #                                  # service-account JWT for a client token
    timeout_seconds: 10          # bound on every Vault request
```

The adapter speaks **KV v2 only** and reads `<address>/v1/<mount>/data/<vault_path>`.
Credentials *without* a `vault_path` keep resolving from env/files, so adoption
is per credential — move them one at a time. With Kubernetes auth the client
token is cached and renewed at 80% of its lease; with token auth the env var is
re-read on every resolution, so rotating the token needs no restart. Pair Vault
with `secrets.cache_ttl_seconds` (it is what keeps the sync schedule from
becoming a request storm against Vault).

---

## enrichers

Post-processors over data already in the cache. Two modes:

**Script-based** — runs a script that receives the target dataset on stdin and
returns a partial dataset (modified/removed hosts):

```yaml
# enrichers/enrich-resolve-ssh.yaml
enrichers:
  - name: enrich-resolve-ssh
    description: "Resolve SSH reachability"
    target: "src-section9"           # dataset being enriched
    project: "prj-connectors"        # optional: resolve script_path in this checkout
    script_path: "enrichers/resolve.py"
    script_args: []                  # optional CLI args for the script
    sync_interval_seconds: 300       # scheduled run; 0/absent = manual only
    # schedule: "45 2 * * *"         # OR a cron cadence (UTC) — pick one
    timeout_seconds: 300             # abort a run that takes longer (default 300)
    config: {}
```

**Declarative merge** — copies specified hostvars fields from one source into
another, matched by hostname. No script needed:

```yaml
# enrichers/enrich-business-unit.yaml
enrichers:
  - name: enrich-business-unit
    description: "Add business_unit from D42"
    target: "src-ssh-dc1"           # dataset being enriched
    source: "src-d42"               # dataset to copy fields from
    fields: ["business_unit"]       # top-level hostvars keys to copy
    sync_interval_seconds: 3600
```

An enricher is not a connector — it gathers nothing — so it keeps its own
`script_path` rather than naming a connector.

---

## endpoints

Output endpoints combine one or more cached datasets through a transformer.
Each endpoint sets **exactly one** of `output` (a builtin, in-process
transformer) or `script_path` (an external script).

```yaml
# endpoints/ep-ansible-full.yaml — builtin transformer, no process spawn
endpoints:
  - name: ep-ansible-full
    description: "Full Ansible Inventory"
    sources: ["src-section9", "src-infra"]
    output: ansible                   # builtins: ansible, json, csv
    config:
      filter: "datacenter=section9"   # host, group, filter, fields, exclude_fields (see docs/endpoints.md)

# endpoints/ep-ansible-managed.yaml — a constructed inventory: merge everything,
# return only the hosts one source has. The others still enrich them and still
# group them. Optional; a limit must name one of the endpoint's own sources.
endpoints:
  - name: ep-ansible-managed
    description: "Managed Hosts Only"
    sources: ["src-section9", "src-infra"]
    output: ansible
    limit:
      by_hosts_from_inventory: "src-section9"

# endpoints/ep-dns-zone.yaml — script transformer, for a bespoke format
endpoints:
  - name: ep-dns-zone
    description: "DNS zone file"
    sources: ["src-infra"]
    project: "prj-connectors"         # optional: resolve script_path in a checkout
    script_path: "outputs/dns_zone.py"
    script_args: []                   # optional CLI args for the script
    timeout_seconds: 300              # script only: abort a transform that takes longer (default 300)
```

---

## projects

Git repositories that hold connector/enricher/transformer scripts and static
inventories — the field reference; the full lifecycle (boot behavior, script
resolution, virtualenvs, health) is [its own page](projects.md). At boot the
app clones each project (shallow, one directory per project name under the
`projects.dir` from `config.yaml`, default `./projects`) and re-pulls it every
`sync_interval_seconds` (fetch + hard reset: local drift is discarded). The
boot clones run in the **background, after the listener is up**: they run
concurrently and each is bounded by the project's `timeout_seconds`, so an
unreachable git remote delays that project's scripts — never `/healthz` or a
Kubernetes startup probe. The sync schedulers start once the clones have had
their bounded chance.

```yaml
# projects/prj-connectors-infra.yaml
projects:
  - name: prj-connectors-infra
    description: "Infrastructure Connectors"
    git_url: "https://github.com/OpusProjects/connectors-infra.git"
    branch: "main"                  # default "main"
    credential: "cred-github-token" # optional, for private repos
    sync_interval_seconds: 1800     # 0/absent = no periodic re-pull
    # schedule: "15 2 * * *"        # OR a cron cadence (UTC) — pick one
    timeout_seconds: 300            # abort a clone/pull that runs longer (default 300)
    python_venv: false              # build a venv from requirements.txt; see below
    sync_on_boot: true              # default true; see below
```

Three sync styles compose from those two knobs:

| Style | Config | Behavior |
|---|---|---|
| Automatic | `sync_interval_seconds: N` or a cron `schedule` | update at boot + every N seconds / at the cron's UTC times |
| Boot only | interval 0/absent | clone/update at boot, then frozen |
| Manual / pipeline-driven | `sync_on_boot: false`, interval 0/absent | an existing checkout is used **as-is** (no network at startup); updates only via `POST /api/v1/projects/{id}/sync` |

With `sync_on_boot: false` a *missing* checkout is still cloned at boot (no
scripts, nothing to run), so first bring-up needs no special casing. Keep
`projects.dir` on a persistent volume for the manual style — the checkout IS
the durable state, there is nothing else to save at shutdown. A pipeline in
the scripts repo can push and then call the sync endpoint (admin key) to roll
new scripts without restarting; script files are re-read on every execution,
so the update takes effect on the next run.

How script paths resolve: a *relative* path whose file exists inside the
checkout runs from there (`<projects.dir>/<project>/<path>`); otherwise the
configured path is kept as-is (image-baked and mounted scripts keep working,
with a warning when the checkout exists but the file doesn't). Resolution
happens at **every execution**, not once at boot, so a script that first
appears after startup — a slow clone, a pipeline's first push — is used on the
very next run without a restart. The project comes from the connector (its
`project` + `path`), from a static source (its own `project` + `path`), or from
an enricher's or endpoint's optional `project` (for their `script_path`). SSH
connectors are never resolved — their `path` is a remote command. Scripts must
carry the executable bit in git (`git update-index --chmod=+x`).

With `python_venv: true`, a `requirements.txt` in the checkout gets a real
virtualenv: built after the clone, refreshed after any pull that changed
`requirements.txt` (unchanged pulls cost two file reads, not a pip run), and
kept OUTSIDE the checkout (`<projects.dir>/.venvs/<project>`) so the hard
reset cannot wipe it. When this project's scripts run, the venv's `bin/` is
prepended to their PATH — a `#!/usr/bin/env python3` shebang resolves to the
venv's interpreter, pip-installed imports work, and non-Python scripts in the
same project are untouched. A failing install (a typo'd package, an
unreachable index) **fails the project sync**, visible in its `sync_health`
and bounded by `timeout_seconds` — not a per-source import error at the next
sync with the cause nowhere.

Private repos: a `token` credential authenticates over https (the token is
passed to git through the environment, never on the command line); an
`ssh_key` credential sets `GIT_SSH_COMMAND` with the key file. A failed
clone/pull logs an error and does not stop the boot — affected sources fail
at sync time and the periodic re-pull retries.

---

## api_keys

API keys with per-consumer permissions. The secret is NEVER here — `env` names
the environment variable that holds it, so these files can live in git and
rotation stays an external process (swap the env var, restart).

```yaml
# api_keys/consumers.yaml
api_keys:
  - name: key-awx
    description: "AWX"
    env: "UNIFIED_API_KEY_AWX"
    role: "admin"                    # everything

  - name: key-forms
    description: "AnsibleForms"
    env: "UNIFIED_API_KEY_FORMS"
    # role defaults to "restricted": only what is listed below
    sources: ["src-ssh-linux"]       # may list/read/sync these sources (or views)
    endpoints: ["ep-ansible-full"]   # may list/run these endpoints
```

A declared key whose env var is missing or empty fails startup (a typo must
not silently lock a consumer out at request time). Restricted keys referencing
unknown source/endpoint ids also fail startup; a `sources:` entry may name a
[view](views.md), which grants the view without granting its members. No
`api_keys` items and no `UNIFIED_API_KEY` = open API (with a loud warning). See
[API → Authentication](api.md#authentication) for the exact route semantics.

---

## Upgrading from 0.34

Until 0.35 the directory was eight fixed files — `config.yaml` plus
`credentials.yaml`, `projects.yaml`, `sources.yaml`, `views.yaml`,
`enrichers.yaml`, `endpoints.yaml` and `api_keys.yaml` — each a map keyed by id,
and every source carried its own script. **0.35 converts that layout itself**,
so a pod upgraded in place starts instead of refusing its own volume.

### At startup

A `CONFIG_DIR` whose root still holds one of those legacy file names, with a map
of maps at its top level, is migrated in place **once**, before loading:

1. **Plan, in memory.** Every item becomes `<section>/<name>.yaml`: the map key
   becomes `name`, the old `name` becomes `description`, the reference keys
   lose their `_id`. Values are written as they were: `1.10`, `0x1F` and
   `True` stay exactly that, never rewritten as the number they look like.
2. **Extract connectors.** Sources sharing the same type, project, script,
   `script_args`, `host_args` and `output_format` share one connector, named
   after the script (its folder when the file is generic — `d42/inventory.py`
   becomes `d42`), `ssh-<command>` for ssh. A name already taken (by another
   connector, or by a built-in: a script in a `remote/` folder) gets the lowest
   free `-2`, `-3`, ... suffix instead. The
   timeout most of its sources used becomes the connector's; a source that
   differed keeps its own as an override. ssh sources lose the `project_id` they
   were required to set and never used; a remote source's `script_path` moves to
   `config.source`, a static source's to `path`.
3. **Validate.** The plan is written into a hidden staging folder beside the
   real files and loaded exactly as startup would load it.
4. **Commit.** The old files move to `.legacy-config-<UTC timestamp>/` (hidden,
   so the loader skips it) and the new ones are renamed into place.

Every step is logged at WARN — which files, which connector came from which
sources, what was dropped — because an operator should find out from the log,
not from a diff weeks later. A conversion that would not load **refuses to
start, having changed nothing**: the old files stay where they were, and the
error says why.

Comments in the old files are not carried over: they are not data, and a YAML
reader does not see them. They are still in the backup, next to the new files,
for whoever wants to move them across.

**Commit the new layout wherever the directory comes from.** A pipeline that
pushes the old files back over a migrated directory is pushing a layout this
version no longer reads, and the config API will say so (with a hint naming
`--migrate-config`).

### A read-only directory

A ConfigMap or a `:ro` bind mount cannot be converted in place. The conversion
is then written into a **private temporary copy**, which is what the process
loads, with a loud warning. Nothing is lost by that — the source of truth is
wherever the read-only files come from — but every restart converts them again,
and writes through the [config API](config-api.md) land in the copy. Convert
the source with `--migrate-config` and redeploy. The copy is removed when the
process stops; one left behind by a process that was killed is removed by the
next start (a copy in use is locked, so instances sharing a temp folder never
remove each other's).

### In a configuration repository

```bash
unified-api --migrate-config ./config     # convert in place, print what it did, exit
CONFIG_DIR=./config unified-api --check-config   # on a 0.34 directory: validate the
                                                 # conversion, write nothing
```

`--migrate-config` runs the same code as startup, so a repository and a pod
upgraded in place end up with the same files. See [CLI](cli.md#converting-a-034-directory---migrate-config).

### Rolling back

0.34 cannot read item files, so rolling back needs the old files back: move
them from `.legacy-config-<timestamp>/` to the root. 0.34 reads only its eight
file names, so the new folders are ignored rather than refused — but any change
made since the upgrade lives only in them, and has to be carried back by hand.

### Before and after

A script source (two tenants of the same system become one connector):

```yaml
# 0.34 — sources.yaml
src-d42-a:
  name: "Device42 tenant A"
  connector_type: "script"
  project_id: "prj-inventory"
  script_path: "dynamic/d42/inventory.py"
  script_args: ["--list"]
  output_format: "ansible"
  credential_ids: ["cred-d42"]
  sync_interval_seconds: 300
  ttl_seconds: 3600
  config:
    tenant: "a"
src-d42-b:
  # ... the same, with tenant "b"
```

```yaml
# 0.35 — connectors/d42.yaml
connectors:
  - name: d42
    type: "script"
    project: "prj-inventory"
    path: "dynamic/d42/inventory.py"
    script_args: ["--list"]
    output_format: "ansible"

# 0.35 — sources/src-d42-a.yaml (src-d42-b.yaml alike)
sources:
  - name: src-d42-a
    description: "Device42 tenant A"
    connector: "d42"
    credentials: ["cred-d42"]
    sync_interval_seconds: 300
    ttl_seconds: 3600
    config:
      tenant: "a"
```

An ssh source (the unused `project_id` is dropped, and what its `gather_mode`
meant moves to the connector: facts become files on the host):

```yaml
# 0.34
src-ssh-dc1:
  name: "Facts DC1"
  connector_type: "ssh"
  project_id: "prj-inventory"
  script_path: "gather_facts"
  credential_ids: ["cred-ssh"]
  sync_interval_seconds: 86400
  ttl_seconds: 3600
  hosts_from_source:
    source: "src-d42-a"
  config:
    gather_mode: "facts"
```

```yaml
# converted — connectors/ssh-gather-facts.yaml
connectors:
  - name: ssh-gather-facts
    type: "ssh"
    location: "host"
    path: "/etc/ansible/facts.d/*.fact"

# converted — sources/src-ssh-dc1.yaml
sources:
  - name: src-ssh-dc1
    description: "Facts DC1"
    connector: "ssh-gather-facts"
    credentials: ["cred-ssh"]
    sync_interval_seconds: 86400
    ttl_seconds: 3600
    hosts_from_source:
      source: "src-d42-a"
```

A source in `gather_mode: script` gets a connector whose `script` is its
`script_path` with its `script_args` appended, as 0.35 ran them.

A remote source (the remote source id moves into `config`):

```yaml
# 0.34
src-edge-dc2:
  name: "Datacenter 2"
  connector_type: "remote"
  project_id: "prj-unused"
  script_path: "src-ssh"
  credential_ids: ["cred-edge-dc2"]
  sync_interval_seconds: 120
  ttl_seconds: 600
  config:
    url: "https://unified-api-dc2.example.com"
```

```yaml
# 0.35 — sources/src-edge-dc2.yaml
sources:
  - name: src-edge-dc2
    description: "Datacenter 2"
    connector: "remote"
    credentials: ["cred-edge-dc2"]
    sync_interval_seconds: 120
    ttl_seconds: 600
    config:
      source: "src-ssh"
      url: "https://unified-api-dc2.example.com"
```

A static inventory source (`script_path` becomes `path`, on the source):

```yaml
# 0.34
src-linux:
  name: "Linux static inventory"
  connector_type: "static_inventory"
  project_id: "prj-inventories"
  script_path: "linux/inventory.yaml"
  sync_interval_seconds: 300
  ttl_seconds: 600
```

```yaml
# 0.35 — sources/src-linux.yaml
sources:
  - name: src-linux
    description: "Linux static inventory"
    connector: "static_inventory"
    project: "prj-inventories"
    path: "linux/inventory.yaml"
    sync_interval_seconds: 300
    ttl_seconds: 600
```

The renamed reference keys, for anything converted by hand:

| Item | 0.34 | 0.35 |
|---|---|---|
| source | `credential_ids` | `credentials` |
| source | `connector_type`, `project_id`, `script_path`, `script_args`, `host_args`, `output_format` | `connector` (the rest moved to the connector item) |
| project | `credential_id` | `credential` |
| enricher | `target_id`, `source_id`, `project_id` | `target`, `source`, `project` |
| endpoint | `source_ids`, `project_id` | `sources`, `project` |
| every item | the map key / `name` | `name` / `description` |

Unchanged: a view member's `source` and `owns.source`, `hosts_from_source.source`,
an endpoint's `limit.by_hosts_from_inventory`, an API key's `sources` /
`endpoints`, an enricher's and endpoint's `script_path`, and LDAP's
`bind_credential` in `config.yaml`. The API is unchanged too: responses keep
`source_id`, `name`, `target_id`, `source_ids` and the rest, so no consumer
needs to change.

---

## Environment variables

Everything the process reads from the environment, including where each credential looks for its secret.

| Variable | Effect |
|---|---|
| `CONFIG_DIR` | Config directory (default `config`) |
| `UNIFIED_API_KEY` | Legacy single admin key: when set, it authenticates like an admin `api_keys` item (see [API](api.md)); health probes and Swagger stay public |
| per `api_keys` item | Each key definition names the env var holding its secret |
| `RUST_LOG` | Log filter, e.g. `debug` or `unified_api=debug` (default `info`) |
| `<PREFIX>_<SUFFIX>` | Secret values referenced by a credential's `env_prefix`/`secret_keys` |
