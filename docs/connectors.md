# Connectors, enrichers & outputs

Everything pluggable in Unified API is an **executable** speaking JSON. Any language
works — the shipped examples under `tests/` are Python. This page defines the
three contracts, and the connector item that says how a source reaches its system.

- [The Dataset shape](#the-dataset-shape)
- [Connectors and sources](#connectors-and-sources)
- [Script connectors (`type: script`)](#script-connectors-type-script)
- [SSH connectors (`type: ssh`)](#ssh-connectors-type-ssh)
- [Static inventory sources (`connector: static_inventory`)](#static-inventory-sources-connector-static_inventory)
- [Remote sources — federation (`connector: remote`)](#remote-sources--federation-connector-remote)
- [Inspecting connectors (`/api/v1/connectors`)](#inspecting-connectors-apiv1connectors)
- [Enrichers](#enrichers)
- [Output endpoints](#output-endpoints)
- [Testing your script](#testing-your-script)

---

## The Dataset shape

Enrichers and outputs deal in this JSON document, and it is what the API serves
for a source. A connector script prints [Ansible inventory
JSON](#the-output-ansible-inventory-json) instead, which is converted to it:

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

---

## Connectors and sources

A **connector** says HOW to talk to a kind of system: which script, from which
project, with which arguments. A **source** says
WHICH instance of that system — its credentials, its `config`, its schedule and
TTLs — and names its connector. Ten Device42 tenants are ten sources and one
connector; before connectors existed, the script path and its arguments were
copied into each of the ten, and drifted apart one edit at a
time.

```yaml
# connectors/d42.yaml
connectors:
  - name: d42
    description: "Device42 inventory script"
    type: "script"                      # script (default) | ssh
    project: "prj-inventory"            # the checkout `path` resolves in
    path: "dynamic/d42/inventory.py"
    script_args: ["--list"]
    # timeout_seconds: 300              # default; a source may override it

# sources/src-d42-acme.yaml
sources:
  - name: src-d42-acme
    description: "Device42 Acme"
    connector: "d42"
    credentials: ["cred-d42-acme"]
    sync_interval_seconds: 300
    ttl_seconds: 3600
    config:
      tenant: "acme"
```

| Connector field | Meaning |
|---|---|
| `type` | `script` (default) or `ssh` |
| `project` | The project whose checkout holds the script. Required for a script connector whose `path` is relative; on ssh, the project of a script streamed to each host, refused with `location: host` |
| `location` | ssh only: where `path` is — `host` (on each host) or `instance` (here; the default with a `project`). See [SSH connectors](#ssh-connectors-type-ssh) |
| `path` | A file, always. script: the one to run, inside the project checkout (or absolute). ssh: a script here, or a file or pattern of files on each host |
| `script` | ssh only, instead of `path`: the script itself, written in the item |
| `script_args` | CLI arguments, verbatim and quoted. On ssh, passed to the streamed script, or to every executable a host path matches |
| `host_args` | Script only: CLI arguments for a host-scoped sync, replacing `script_args` (see below) |
| `timeout_seconds` | Bound on one sync (default 300). A source may set its own to override it — the tenant ten times bigger than the others |

Anything that describes the script or the protocol is the connector's; anything
about one instance — credentials, `config`, cadence, TTLs, and cache policy such
as `sync_mode` and `host_sync_updates_group_vars` — stays on the source. The
full split is in [configuration](configuration.md#source-or-connector).

Settings that look active and are not are refused at startup rather than
ignored: `host_args` on an ssh connector, `project` with
`location: host`, `location` or `script` on a script connector, `path` and
`script` together, a relative script `path`
without a `project`, and `project` / `path` on any source that is not
`static_inventory`. `output_format: ansible`, which 0.35 configurations carry,
is still read and changes nothing (it is the only format); `native` is refused
by name.

Two connectors are **built in** and need no item, because there is nothing about
their HOW to configure — the protocol is fixed, and the WHERE is per source:

| `connector:` | What it reads | The source sets |
|---|---|---|
| `remote` | Another unified-api instance | `config.url` and `config.source` (the source id there) |
| `static_inventory` | An Ansible YAML inventory in a project checkout | `project` and `path` |

Their names are reserved: a connector item called `remote` or `static_inventory`
fails startup. `project` + `path` mean the same thing everywhere — a file inside
that project's checkout — whether it is a connector's script or a static source's
inventory.

---

## Script connectors (`type: script`)

The connector script is executed on every sync and prints Ansible inventory JSON
to stdout, as any Ansible dynamic inventory script does.

**Input (command line):** the connector's `script_args` list is passed verbatim as
CLI arguments (no shell involved, so no quoting concerns). This is how scripts
that implement the standard Ansible dynamic inventory interface get their
`--list`:

```yaml
connectors:
  - name: d42
    project: "prj-inventory"
    path: "d42_inventory.py"
    script_args: ["--list"]
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

**Output:** [Ansible inventory JSON](#the-output-ansible-inventory-json) on stdout.
Exit non-zero to fail the sync; stderr is captured into the error message.

**Time limit:** the script must finish within the source's `timeout_seconds`, or
the connector's when the source sets none (default 300). A slower run is aborted and the sync fails with
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
print(json.dumps({"_meta": {"hostvars": inventory.hosts}, **inventory.groups}))
```

Supporting `scope`/`target` is optional but recommended: it lets consumers refresh a
single host or group without paying for a full inventory pull.

#### Asking for one host on the command line (`host_args`)

`scope`/`target` reach the script inside `SOURCE_CONFIG`, which is a convention
of ours. Inventory scripts have their own: Ansible defines `--list` and
`--host <hostname>`. A connector can name the arguments a **host-scoped** sync
should use, and they replace `script_args` for that run:

```yaml
connectors:
  - name: d42
    project: "prj-inventory"
    path: "connectors/d42_inventory.py"
    script_args: ["--list"]
    host_args: ["--host-with-groups", "{target}"]
```

`{target}` is substituted with the requested hostnames, comma-joined — the same
value `config.target` carries, since `?host=` accepts a list. The substitution
happens inside each argument, so `["--host={target}"]` works too.

**No CLI convention is imposed**, and Ansible's own `--host <hostname>` is not
one to copy: it prints one host's bare variables, `{"ansible_host": ...}`, with
no `_meta` and no groups, which reads as groups named after the variables and
fails the sync. A script states its own flag, as above, and answers it with
Ansible inventory JSON holding the requested hosts.

Omitting `host_args` keeps the old behaviour: the script is called exactly as a
full sync would call it and the requested hosts are picked out of whatever comes
back. That is less wasteful than it sounds — since `_meta.hostvars` made
per-host calls vestigial, Ansible's own `--host` was usually implemented as
"fetch everything, then filter" — but a script that CAN fetch one host should be
asked for one host.

`host_args` is refused at startup on an ssh connector, and the built-ins have no
place for it: none of them is spawned with CLI arguments at all.

#### What a host-scoped sync writes

The requested hosts' variables, and **where those hosts belong**:

- a group the connector places the host in is joined, and created if the target
  does not have it yet;
- a group it does **not** name is left — otherwise a machine moved in the source
  of truth stays in its old group until the next full sync, which is exactly the
  staleness a granular sync exists to avoid;
- every other host's membership is untouched, and so are `all` and `ungrouped`
  (meta-groups: membership in them is not a connector's to state);
- returning **no** groups at all says nothing about groups rather than "this
  host belongs to none" — a script following Ansible's `--host` contract returns
  hostvars alone, and reading that as a departure from everything would empty
  the inventory one scoped sync at a time.

Group **vars** stay out of it unless the source opts in with
`host_sync_updates_group_vars: true`, and even then only for the groups those
hosts landed in. A granular call answers about a host; letting it redefine what
a group means for every other member is a bigger statement than the sync was
asked to make.

Worked example — one script, two modes:

```python
#!/usr/bin/env python3
"""d42_inventory.py — --list for everything, --host-with-groups for one host."""
import argparse, json, sys

parser = argparse.ArgumentParser()
parser.add_argument("--list", action="store_true")
parser.add_argument("--host-with-groups")          # named in host_args
args = parser.parse_args()

if args.host_with_groups:
    # `?host=a,b` arrives as one comma-separated argument
    wanted = args.host_with_groups.split(",")
    hosts = d42.hosts(names=wanted)                # one API call, not the fleet
    print(json.dumps({
        "_meta": {"hostvars": {h.name: h.vars for h in hosts}},
        # Where these hosts belong. Naming a group joins it; a group NOT named
        # is one the host has left. Naming none says nothing about groups.
        **{
            g.name: {"hosts": [h.name for h in hosts if g in h.groups],
                     "vars": g.vars}
            for g in {g for h in hosts for g in h.groups}
        },
    }))
    sys.exit(0)

inventory = d42.everything()
print(json.dumps({"_meta": {"hostvars": inventory.hostvars}, **inventory.groups}))
```

```yaml
connectors:
  - name: d42
    project: "prj-inventory"
    path: "connectors/d42_inventory.py"
    script_args: ["--list"]
    host_args: ["--host-with-groups", "{target}"]

sources:
  - name: src-d42
    connector: "d42"
    host_sync_updates_group_vars: true   # cache policy: the source's, not the script's
    ttl_seconds: 3600
```

The pay-off is in what the granular call does NOT do. Registering a new machine
and calling `POST /sources/src-d42/sync?host=new.example.net` costs one API
lookup instead of the whole inventory, so a provisioning job stops waiting on a
full pull — and if the slow part of the full pull is enrichment (resolving which
of a host's many addresses actually answers), that work belongs in an
[enricher](enrichers.md) on its own schedule rather than in either mode of this
script.

### The output: Ansible inventory JSON

A connector prints what `inventory.py --list` prints for Ansible: host
variables under `_meta.hostvars`, and each group as a top-level key. Any
existing dynamic inventory script works unmodified (pair it with
`script_args: ["--list"]`):

```json
{
  "_meta": {
    "hostvars": {
      "motoko.section9.net": { "ansible_host": "10.9.1.1", "os": "OracleLinux" }
    }
  },
  "section9": {
    "hosts": ["motoko.section9.net"],
    "vars": { "ntp_server": "ntp.section9.net" }
  }
}
```

It is converted to the [Dataset](#the-dataset-shape):

- `_meta.hostvars` → `hostvars`. A missing `_meta` is accepted with a warning
  (hosts will have no variables); `_meta` without `hostvars` means the same,
  without the warning, as Ansible reads it.
- Every other top-level key becomes a group. Both the object form
  (`{hosts, children, vars}`) and the legacy list form (`"web": ["h1", "h2"]`)
  are accepted.
- A host named in a group but not in `_meta.hostvars` is a host with no
  variables, as `ansible-inventory --list` prints it, on a full sync. A host or
  group sync leaves such a host as the cache has it: an answer about hosts it
  was not asked for is not a claim that they have no variables.
- The implicit meta-groups `all` and `ungrouped` are skipped, but their hosts
  are kept; `vars` on them have no Dataset equivalent, and a warning says so.
- A key a group does not take beside `hosts`, `children` or `vars` (a
  `description`) is ignored with a warning, as Ansible ignores it.
- Malformed input is an **error that fails the sync**, naming the offending
  group — never a silent skip. That includes a group with none of `hosts`,
  `children` or `vars` (`"host": [...]` for `"hosts"`).
- The `{"hostvars", "groups"}` shape a script could print before 0.36 fails the
  sync saying what to print. Read as Ansible it would be two groups called
  `hostvars` and `groups` and no host variables: a sync that "works" and serves
  nothing useful.

---

## SSH connectors (`type: ssh`)

The native SSH connector connects to the fleet in parallel and builds the
Dataset from what each host answers. The connector says what runs: a `path`
(a file, and `location` says where it is) or a `script` written in the item,
the way a CI step is either a file or inline. Which hosts, as whom and how is
the source's.

```yaml
# connectors/ssh.yaml
connectors:
  # On each host: every file the path matches, run if executable, read if
  # not, each one a fact named after its file (Ansible's local facts).
  - name: ssh-facts
    type: ssh
    location: host
    path: /etc/ansible/facts.d/*.fact

  # Here: a script in a project, streamed to each host and run there.
  - name: ssh-osinfo
    type: ssh
    project: prj-toolbox                # location: instance is the default
    path: linux/osinfo.sh

  # Inline: the script itself, streamed the same way.
  - name: ssh-uptime
    type: ssh
    script: |
      printf '{"uptime_s": %s}\n' "$(cut -d. -f1 /proc/uptime)"

# sources/src-ssh-dc1.yaml
sources:
  - name: src-ssh-dc1
    connector: ssh-facts
    credentials: ["cred-ssh-infra"]
    sync_interval_seconds: 300
    ttl_seconds: 600
    config:
      hosts: "host1.example.com,host2.example.com"
```

**`location: host`** — `path` is absolute on the hosts: one file, or a pattern
(`*`, `?`, `[…]`) the host's shell expands. Every file it matches is run if it
is executable (with `script_args`, quoted) and read otherwise, the way Ansible
reads local facts, and becomes one fact named after its file without the
extension: `/etc/ansible/facts.d/rack.fact` becomes `rack`. A file that prints
or holds JSON is that JSON; INI becomes an object per section with lowercased
keys, as Ansible's reader gives; anything else becomes the string
`error loading fact - please check content`, so one bad file never costs the
host its other facts. The path may hold only letters, digits and `/ . _ - + @ : ,`
besides the pattern characters: it reaches the host's shell unquoted, so that
the pattern expands.

**`location: instance`** (the default when `project` is set; write it for an
absolute path on this instance's disk) — `path` is a script file on this
instance: in `project`'s checkout (read at every sync, so a push is live at the
next one), or absolute. It is sent over the ssh connection, and the host's
`/bin/sh` (whatever the account's login shell) writes it to a private
temporary file in `$TMPDIR` or `/tmp` (with `mktemp`, or without it on hosts
that have none), checks that all of it arrived — a session cut while sending
never runs part of a script — runs that file with the interpreter its `#!`
line names and that line's flags (`env` and its options skipped) — or, with no
`#!` line, the account's login shell, as an ssh command line is run — and
removes it when the run ends. A run the timeout cuts while the script hangs
gets no signal from sshd and may leave its file and process behind; the next
run removes such files once they are a day old. It runs as a file would:
`script_args` after it as `$1`, `$2`… (never taken for the interpreter's own
options), `$0`, `$BASH_SOURCE` and `__file__` naming it, and its standard input
empty, so nothing in it waits on the connection. Hosts that mount `/tmp`
noexec run it too: the interpreter reads the file, nothing executes it. What
it prints is the host's variables, one JSON object.

**`script`** — the script written in the item instead of a file: streamed and
run exactly as a file from here is (its `#!` line, `script_args`, a temporary
file removed after the run). It takes no `project` or `location`, and an ssh connector
has `path` or `script`, never both. A command with arguments is a script of one
line: `script: /usr/local/bin/disk-report --json`.

**From 0.35** — 0.35 wrote the command (or a label such as `gather_facts`) in
`path`, and each source's `config.gather_mode` said what it meant. 0.36 reads
that form only at startup: an ssh connector with neither `location`, `project`
nor `script` is converted in memory, with its sources, and each conversion is
logged (and printed by `--check-config`, which still exits 0). Facts become
`location: host` with `path: <fact_path>/*.fact` (a `$HOME` or relative
`fact_path` as `~/`, which a host path may start with); a command becomes
`script:`, its `script_args` appended to the line raw, as 0.35 appended them,
and with no `#!` line it runs in the account's login shell, as in 0.35; `gather_mode` and
`fact_path` go from the sources. The files are not touched, so the image can
roll back, and the config API refuses the old form, naming what to write: move
the files when convenient.

```yaml
# 0.35: a label as the path, the mode on every source
- name: ssh-gather-facts
  type: ssh
  path: gather_facts            # sources: gather_mode: facts, fact_path: /etc/ansible/facts.d
# 0.36
- name: ssh-gather-facts
  type: ssh
  location: host
  path: /etc/ansible/facts.d/*.fact     # 0.35 read *.json there too
```

```yaml
# 0.35: the script in path, gather_mode: script on the source
- name: ssh-script
  type: ssh
  path: |-
    . /etc/os-release
    ...
# 0.36: the same lines as script (or in a file, with path)
- name: ssh-script
  type: ssh
  script: |-
    . /etc/os-release
    ...
```

Then remove `gather_mode` and `fact_path` from the sources' `config`.

**Source `config` keys:**

| Key | Default | Meaning |
|---|---|---|
| `hosts` | — | Comma-separated hostnames to connect to |
| `port` | `22` | SSH port |
| `concurrency` | `50` | Max parallel connections (tokio semaphore) |
| `ssh_connect_timeout_seconds` | `30` | Per-host connection/exec timeout |
| `ssh_legacy_algorithms` | `false` | `"true"` additionally offers SHA-1 KEX/MACs (appended last, modern servers still pick modern ones) for OpenSSH 5.x-era hosts (EL6) that lack `hmac-sha2` |
| `ssh_known_hosts` | — | Path to an OpenSSH `known_hosts` file. When set, every server key is checked against it **before authentication**: an unknown or mismatched host is refused (it lands in `unreachable`, so its last known data is kept), and the refusal logs both fingerprints. A server presenting an OpenSSH host *certificate* is refused too — verification is defined over plain host keys. Unset = any server key is accepted, and every sync warns about it |

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

### Dynamic host lists (`hosts_from_source`)

Instead of a static `config.hosts`, an SSH source can take its hosts from
**another source's cached dataset** — the natural chain of "the inventory
source says WHAT exists, SSH says HOW it is doing":

```yaml
sources:
  - name: src-fleet-facts
    connector: "ssh-facts"
    credentials: ["cred-fleet-ssh"]
    sync_interval_seconds: 300
    ttl_seconds: 600
    hosts_from_source:
      source: "src-netbox"              # any source: script, static, even ssh
      match_pattern:                    # absent = every host in the dataset
        groups: ["linux", "proxmox_vms"]
        hosts: ["extra01.example.com"]
      connect_via: "ansible_host_then_hostname"
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
> connector's `timeout_seconds` (default 300, overridable per source) separately
> bounds the **whole sync** across all hosts. They are different knobs.

**Credentials:** expects `username` (or `ssh_username`) and an `ssh_key_path` /
`key_path` from `file_keys` — see [configuration](configuration.md).

---

## Static inventory sources (`connector: static_inventory`)

For classic Ansible **static YAML inventories** — an `inventory.yaml` with the
`all/children/hosts` tree plus optional `group_vars/` and `host_vars/`
directories next to it. No process is spawned and no `ansible-core` is
needed: the files are parsed natively.

Either Ansible layout works for those directories: `group_vars/web.yaml`, or
`group_vars/web/` holding any number of `*.yaml`/`*.yml` files that are merged
alphabetically — one file per concern is how a large inventory stays readable.
Nested directories below that are not descended into. Defining the same name
both ways fails the sync rather than picking one.

A group may be declared more than once — under two parents, or twice under the
same one. The declarations are **merged**: hosts, children and group vars all
accumulate, and a host inherits the vars of every ancestry its group is declared
under. Naming the same host in both declarations lists it once.

```yaml
sources:
  - name: src-inventory-linux
    description: "Linux static inventory"
    connector: "static_inventory"        # built in: no connector item
    project: "prj-inventories"           # git repo holding the inventory
    path: "inventory.yaml"               # the file inside the checkout
    sync_interval_seconds: 300
    ttl_seconds: 600
```

A built-in connector, so the source names the file itself: `project` + `path`, the
same pair a script connector uses for its script. Both are required. The path
resolves inside the checkout, so the project's periodic pull (or the on-demand
`POST /api/v1/projects/{id}/sync`) is what brings in new data — the next source
sync reads the updated files. Arguments, credentials and
`SOURCE_CONFIG` don't apply to this connector.

**What lands where:**

- Variables are emitted **where they are declared**, not resolved onto every
  host. A host's `hostvars` hold its own two sources only — its inline vars in
  the inventory file, then `host_vars/<host>` — and a group's vars stay on the
  group: its inline vars merged with `group_vars/<group>`.
- Groups keep their structure: direct `hosts`, `children`, and their own vars.
  `all` **is** emitted as a group, because that is where `group_vars/all`
  lands; `ungrouped` is not, since Ansible synthesises it for hosts that are in
  no group and emitting it would be inventing membership.
- **Precedence is the consumer's.** Ansible applies its own ordering when it
  reads the inventory, and it is the authority on its own rules, so this
  connector no longer reimplements them. The resolved result on each host is
  unchanged for the ordinary cases the old flattening covered.

  It was resolved here once. The cost is why it is not any more: copying a
  group's vars onto each member meant a 1097-host inventory whose
  `group_vars/all` is 55 KB carried that 55 KB a thousand times over — a 53 MB
  dataset, ~94% of it the same bytes repeated, which OOMKilled the server on
  sync. Anything reading `hostvars` directly rather than through Ansible — a
  declarative enricher, `output: json` — must resolve group vars itself.

**Deliberately unsupported (loud, never silent):**

- INI inventories — YAML only
- host range patterns (`web[01:20].example.com`) → the sync fails
- ansible-vault encrypted files or values → the sync fails naming the file
- Jinja templating: `{{ ... }}` values pass through as literal strings
  (templating belongs to the consumer, e.g. Ansible itself)
- YAML tags: `!unsafe` and other local tags are dropped and the value kept;
  `!vault` fails the sync

**Scalars read as YAML 1.2 core, as in 0.31:** only `true`/`false` are
booleans (`yes`, `on` stay text); `0644` and `08001` stay text, not numbers.
Ansible itself reads YAML 1.1, where `yes` is true and `0644` is 420: quote a
value whose type matters.
- `group_vars`/`host_vars` files that match nothing log a warning

---

## Remote sources — federation (`connector: remote`)

Another unified-api instance is the source. This is how multi-datacenter
setups compose: one instance per DC does the local work (SSH with LAN
latency, local credentials, local firewall rules), and a central instance
aggregates them — consumers only ever talk to the central.

```yaml
# on the central: sources/src-dc1.yaml
sources:
  - name: src-dc1
    description: "Datacenter A"
    connector: "remote"                # built in: no connector item
    credentials: ["cred-edge-dc1"]     # token credential = the remote API key
    sync_interval_seconds: 120
    ttl_seconds: 600
    config:
      url: "https://unified-api-dc1.example.com"
      source: "src-ssh"                # the source id ON THE REMOTE instance
      # http_timeout_seconds: "30"     # default 30
      # insecure_tls: "true"           # self-signed remotes; opt-in, never default
```

Both `config.url` and `config.source` are required, and `project` / `path` are
refused: nothing runs from a checkout.

On the edge, give the central a **restricted API key** (least privilege):

```yaml
# on the edge: api_keys/key-central.yaml
api_keys:
  - name: key-central
    description: "Central aggregator"
    env: "UNIFIED_API_KEY_CENTRAL"
    sources: ["src-ssh"]
```

**How it works:** `GET /dataset` on the remote returns exactly the Dataset a
connector must produce — the API itself is the federation protocol. A second
call to `/status` recovers the data's real age at the origin (dataset-level
and per-host), and the local cache entry is built with those ages: freshness
reporting stays truthful across hops instead of resetting to zero on every
transfer. If the `/status` call fails, the data still lands (with a warning)
and is treated as fresh — data beats metadata. Each sync also asks the remote
for its advertised ownership scope (`GET /scope`, best effort — an older
remote without the route just yields no claim), which is what lets a view
route by the edge's own claim: see
[federation → the scope contract](federation.md#the-scope-contract).

Full pulls are **conditional**: the remote's `/dataset` serves an `ETag`, the
connector revalidates with `If-None-Match`, and a `304` skips the transfer
and the re-parse entirely — the rest of the sync (ages, scope, cache apply)
proceeds from the remembered dataset as if the bytes had travelled. A central
polling an unchanged edge pays a handful of header bytes per tick instead of
the full dataset; `unified_api_remote_not_modified_total` counts the skips.
Host-scoped pulls stay unconditional (a partial response must never be
remembered as "the dataset"), and the memory is process-local — a restart
just pays one full transfer again.

Failure semantics follow the house rules: `401`/`403`/`404` produce errors
that say what to check; a WAN cut fails the sync loudly while the central
keeps serving the last good dataset from its cache (stale beats nothing).
Centrals can themselves be federated by another instance — regions → global
with no extra machinery. Combine with an output endpoint listing all the DC
sources to serve one merged world view.

---

## Inspecting connectors (`/api/v1/connectors`)

Which connectors exist, what each one runs, which sources use it and how those
sources are doing — admin-only, like projects, since a connector names the
script, its project and its arguments for every source of a system:

```bash
curl -s localhost:8182/api/v1/connectors -H "X-API-Key: $ADMIN_KEY"
curl -s localhost:8182/api/v1/connectors/d42 -H "X-API-Key: $ADMIN_KEY"
```

```json
{
  "connector_id": "d42",
  "name": "Device42 inventory script",
  "connector_type": "script",
  "built_in": false,
  "project_id": "prj-inventory",
  "location": null,
  "path": "dynamic/d42/inventory.py",
  "script": null,
  "script_args": ["--list"],
  "host_args": [],
  "timeout_seconds": 300,
  "used_by": [
    {"source_id": "src-d42-a", "name": "Device42 tenant A", "timeout_seconds": 300, "failing": false, "synced": true},
    {"source_id": "src-d42-b", "name": "Device42 tenant B", "timeout_seconds": 900, "failing": true, "synced": true}
  ],
  "health": {"sources": 2, "healthy": 1, "failing": 1, "never_synced": 0}
}
```

The list holds the connector items first, then the two built-ins
(`built_in: true`, `connector_type` `remote` / `static_inventory`, null
`project_id`, `path` and `timeout_seconds`), so every source's
`connector:` resolves to a row. `used_by[].timeout_seconds` is the bound that
source actually syncs under — its override, or the connector's — and `health`
counts its sources' sync health at a glance: one broken script shows up as a
connector whose sources all fail at once.
`GET /api/v1/sources/{id}` names the connector under `gathering.connector`.

---

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
> you can carry the rest through. (A *declarative* enricher — `source` +
> `fields` — has no such constraint: it writes only the fields it owns.)

---

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

---

## Testing your script

Wire it into a connector item pointing at your file (`config/connectors/mine.yaml`)
and a source naming it (`config/sources/src-mine.yaml`) — or an enricher or
endpoint item — then `cargo run` and drive it through the API:

```bash
curl -X POST localhost:8182/api/v1/sources/src-mine/sync
curl localhost:8182/api/v1/sources/src-mine/dataset
```

For automated tests, follow the patterns in `tests/` — the suite runs entirely
against the sample scripts under `tests/adapters/out/` (`connectors/`, `enrichers/`, `output/`).
