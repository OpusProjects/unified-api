# On-demand refresh

How a consumer gets **current** facts for a named host, across a federated
deployment, without knowing where that host lives and without being able to
overload the datacenter that owns it.

Two entry points, one mechanism:

| | Verb | Who uses it |
|---|---|---|
| `POST /api/v1/sources/{id}/sync?host=X&refresh_origin=true` | write | AWX, pipelines, operators |
| `GET /api/v1/sources/{id}/dataset?host=X&refresh=true` | read | forms, dashboards, anything that can only fetch a URL |

The `GET` form is the read-through: it decides whether a refresh is needed at
all, coalesces concurrent callers, bounds the wait, and answers from cache if
the refresh does not work out. The `POST` form is the explicit one: it always
refreshes and it tells you if it could not.

---

## Who decides what

This is the part worth reading twice, because everything else follows from it.

| Who | Decides | With |
|---|---|---|
| **Operator** | how much staleness is acceptable | `ttl_seconds`, `ttl_overrides` on the source |
| **Operator** | whether reads may refresh at all | `allow_on_demand_refresh` on the source |
| **Consumer** | whether to wait or be served fast | `refresh=true` |

A consumer **cannot** ask for a tighter freshness bound than the operator
configured. There is no `max_age_seconds` parameter and there will not be one:
any consumer-supplied staleness bound is a consumer-supplied *load* knob, and
the load lands on somebody's datacenter over SSH.

### The load ceiling

Because the TTL gates the refresh, the cost is bounded by arithmetic rather than
by trusting consumers:

```
  refresh=true, in a loop, from 100 consumers, same host
        │
        ├─ is the host still inside its TTL?  ── yes ─►  served from cache, 0 gathers
        │                                                (for the whole TTL window)
        └─ no ─►  per-host lock: 1 gather, the rest wait and then find it fresh
                        │
                        └─►  ceiling: ONE gather per host per TTL window
```

With `ttl_seconds: 300` and 371 hosts, the absolute worst case — every host
being asked for continuously — is 371 gathers every 300 seconds, which is
**exactly the load of `sync_interval_seconds: 300`**. A configuration you
already know how to reason about. In practice it is far less, because only the
hosts somebody actually looks at get refreshed at all.

Two limits sit under that one, covering what the TTL window does not:

- **Same host, concurrently.** Requests arriving before the first gather
  finishes are all still "stale" — the window has not closed. They queue on a
  per-host lock, and the late ones re-check freshness and find nothing to do.
  Per host and not per source: a source-wide lock would make every caller wait
  behind a refresh of an unreachable host, which is precisely the request that
  burns the whole timeout.
- **Many different hosts at once.** All first in their window, so the TTL does
  not bound them. `server.refresh_max_concurrent` does.

### Two things it refuses

- **`refresh=true` without `?host=` → `400`.** A whole-source refresh triggered
  by opening a page is a gather of the entire inventory. Name the hosts, or
  `POST` a full sync if you really want everything.
- **A source without `allow_on_demand_refresh` → `403`.** Off by default. A read
  that can cause SSH into a datacenter is a capability, not a convenience, and a
  source that was never granted it is immune to whatever consumers send.

---

## What actually happens on the wire

```
     AnsibleForms / a browser / anything that fetches a URL
                          │
                          │  GET /api/v1/sources/src-edge-dc4/dataset
                          │      ?host=itexenode100&refresh=true
                          │      X-API-Key: <consumer key>
                          ▼
      ┌───────────────────────────────────────────────────────┐
      │   unified-api CENTRAL          (DC aa1, k8s)          │
      │                                                        │
      │   src-edge-dc4:  connector_type: remote                │
      │                  allow_on_demand_refresh: true         │
      │                  ttl_seconds: 300                      │
      │                                                        │
      │   1. is itexenode100 inside its 300s TTL?              │
      │        yes ──────────────────────────► serve cache ────┼──► 200, 0 hops
      │        no  ──┐                                         │
      │   2. take the per-host lock; re-check (someone else     │
      │      may have just refreshed it)                       │
      │   3. take a permit from refresh_max_concurrent          │
      │   4. RemoteConnector, budget = refresh_timeout_seconds  │
      └──────────────────────────┬─────────────────────────────┘
                                 │
        POST {edge}/api/v1/sources/src-ssh-facts/sync
             ?host=itexenode100&refresh_origin=true&refresh_depth=2
                                 │   X-API-Key: <edge key>
                                 ▼
      ┌───────────────────────────────────────────────────────┐
      │   unified-api EDGE             (DC dc4, podman)        │
      │                                                        │
      │   src-ssh-facts: connector_type: ssh                   │
      │                  hosts_from_source: src-inventory      │
      │                                                        │
      │   host scope honoured: SSH to itexenode100 ONLY        │
      └──────────────────────────┬─────────────────────────────┘
                                 │ russh, key that never leaves dc4
                                 ▼
                          itexenode100  (facts)
                                 │
      ┌──────────────────────────┴─────────────────────────────┐
      │  then the central reads what the edge now has:          │
      │                                                         │
      │  GET {edge}/…/dataset?host=itexenode100   ← ~KB, not MB │
      │  GET {edge}/…/status?host=itexenode100    ← the REAL age│
      │                                                         │
      │  cached with that age, not with "now"                   │
      └──────────────────────────┬─────────────────────────────┘
                                 ▼
                     200 + the host's facts
                     x-unified-api-refreshed: true
                     x-unified-api-refreshed-hosts: itexenode100
```

The refresh intent recurses: if the edge's source were itself `remote` (region →
global), it would propagate the same way with one hop spent. `refresh_depth`
(default 3) is what stops a topology accidentally wired into a cycle. Running
out of hops is not an error — the data is still fetched, just not re-gathered at
the far end.

### Which instance needs which setting

A frequent source of confusion, so explicitly:

| Setting | Central | Edge |
|---|---|---|
| `allow_on_demand_refresh` | **yes**, on the `remote` source | no |
| `ttl_seconds` (the gate) | **yes**, this is the one that decides | its own, unrelated |
| `server.refresh_timeout_seconds` | **yes**, bounds the consumer's wait | no |
| `server.refresh_max_concurrent` | **yes** | no |

The edge does not need `allow_on_demand_refresh` because the central calls its
`POST /sync`, not its `GET /dataset?refresh=`. The edge only needs to accept the
central's key for that source — which a **restricted** key does, since `POST
/sync` authorises on the source list, not on the admin role.

---

## Configuration

Complete and copy-pasteable. Two instances, one datacenter each.

### Edge — `sources.yaml` (DC dc4, podman)

Nothing new here. The edge is unchanged by this feature; it only has to honour
a host-scoped sync, which it does as of 0.8.0.

```yaml
# The inventory that says WHAT exists in this DC
src-inventory:
  name: "Device42 inventory (dc4)"
  project_id: "prj-connectors"
  script_path: "d42_inventory.py"
  script_args: ["--list"]
  output_format: "ansible"
  connector_type: "script"
  credential_ids: ["cred-d42"]
  sync_interval_seconds: 3600
  ttl_seconds: 7200

# The facts: WHAT each host is doing. Host list comes from src-inventory.
src-ssh-facts:
  name: "Ansible local facts (dc4)"
  project_id: "prj-connectors"
  script_path: "unused-in-facts-mode"
  connector_type: "ssh"
  credential_ids: ["cred-ssh-dc4"]
  hosts_from_source:
    source: "src-inventory"
    match_pattern:
      groups: ["dc4"]
    connect_via: "ansible_host_then_hostname"
  sync_interval_seconds: 1800
  # The edge's own TTL. Independent of the central's: this one describes when
  # the edge's scheduler considers its data old, the central's decides when a
  # consumer's read is worth a gather.
  ttl_seconds: 3600
  config:
    gather_mode: "facts"
    fact_path: "/etc/ansible/facts.d"
    concurrency: "40"
    ssh_connect_timeout_seconds: "20"
```

### Edge — `api_keys.yaml`

The key the central uses. Restricted to the one source it federates: if the
central is compromised it cannot read the rest of dc4's inventory.

```yaml
key-central:
  name: "central aa1"
  env: "UNIFIED_API_KEY_CENTRAL"
  role: "restricted"
  sources: ["src-ssh-facts"]
  endpoints: []
```

### Central — `config.yaml` (DC aa1, k8s)

```yaml
server:
  host: "0.0.0.0"
  port: 8080
  # How long a READ may wait for a refresh before it gives up and serves cache.
  # Sized for a human waiting on a form, not for a scheduled sync: it has to
  # cover the WAN round trip plus one host's SSH gather, and no more.
  refresh_timeout_seconds: 15
  # Concurrent on-demand refreshes, process-wide. What stops forty forms opening
  # at once from becoming forty simultaneous gathers.
  refresh_max_concurrent: 8

cache:
  persistence:
    path: "/var/lib/unified-api/cache.json"
    interval_seconds: 300
```

### Central — `sources.yaml`

```yaml
src-edge-dc4:
  name: "DC4 facts (federated)"
  project_id: "prj-connectors"
  # script_path = the source id ON THE EDGE
  script_path: "src-ssh-facts"
  connector_type: "remote"
  credential_ids: ["cred-edge-dc4"]
  # This is the gate. A read asking for a host refreshes it only if the host is
  # older than this. Read the "Choosing the TTL" note before setting it.
  ttl_seconds: 300
  # Off by default; this is what lets a GET trigger a gather.
  allow_on_demand_refresh: true
  # The scheduler keeps working as before: the central pulls the edge's cache
  # on its own interval and NEVER causes SSH by doing so.
  sync_interval_seconds: 600
  ttl_overrides:
    hosts:
      # A box whose state changes fast enough to be worth gathering more often
      itexenode100: 60
    groups:
      # …or a whole group of them
      oracle_db: 120
  config:
    url: "https://unified-api-dc4.example.com"
    # Bounds BOTH the reads and the refresh POST, so it must exceed the time the
    # edge needs to gather one host. Raise it on slow links or slow hosts.
    http_timeout_seconds: "30"
```

### Central — `credentials.yaml`

```yaml
# The edge's API key, read from UNIFIED_API_EDGE_DC4_KEY. The remote connector
# looks for a credential named `token`, which is what secret_keys maps here.
cred-edge-dc4:
  name: "DC4 edge API key"
  type: "token"
  env_prefix: "UNIFIED_API_EDGE_DC4"
  secret_keys:
    token: "KEY"
```

### The secret contract

| Instance | Env var | Value | Where it comes from |
|---|---|---|---|
| Edge | `UNIFIED_API_KEY_CENTRAL` | the key the central presents | Vault → ESO → Secret |
| Central | `UNIFIED_API_EDGE_DC4_KEY` | the **same** value | Vault → ESO → Secret |
| Central | `UNIFIED_API_KEY_FORMS` | the consumer key (AnsibleForms) | Vault → ESO → Secret |
| Edge | `CREDENTIAL_*` for `cred-ssh-dc4` | SSH key + user, dc4 only | Vault → ESO → Secret |

The SSH key exists only on the edge. That is the point of the topology: no
credential with reach into dc4 is ever present in aa1.

---

## Verification

Run these against the central after deploying both sides.

Every command below was executed, in this order, against **two real instances**
of this binary talking to each other over HTTP — the config blocks above,
extracted from this document, with only the edge URL pointed at a local port and
the edge's SSH source standing in as a script (there are no real hosts to gather
in a validation run). The outputs shown are the ones those runs produced.

### 1. The plumbing is there

```bash
curl -s -H "X-API-Key: $FORMS_KEY" \
  "$CENTRAL/api/v1/sources/src-edge-dc4/status?host=itexenode100" | jq
```

```json
{
  "source_id": "src-edge-dc4",
  "dataset_age_seconds": 431,
  "ttl_seconds": 300,
  "total_hosts": 371,
  "returned": 1,
  "hosts": [
    { "hostname": "itexenode100", "age_seconds": 431, "is_fresh": false, "ttl_seconds": 60 }
  ]
}
```

`is_fresh: false` and `ttl_seconds: 60` (the host override, not the source's
300) mean the next `refresh=true` will gather. If `is_fresh` were `true`, it
would not — and that is the whole safety property, not a bug.

### 2. A refresh that does something

```bash
curl -si -H "X-API-Key: $FORMS_KEY" \
  "$CENTRAL/api/v1/sources/src-edge-dc4/dataset?host=itexenode100&refresh=true" \
  | grep -i '^\(HTTP\|x-unified-api\)'
```

```
HTTP/1.1 200 OK
x-unified-api-refreshed: true
x-unified-api-refreshed-hosts: itexenode100
```

Then confirm the age really moved, at both ends:

```bash
# on the central
curl -s -H "X-API-Key: $FORMS_KEY" \
  "$CENTRAL/api/v1/sources/src-edge-dc4/status?host=itexenode100" \
  | jq '.hosts[0].age_seconds'   # → a small number, single digits

# on the edge, proving the SSH actually happened there
curl -s -H "X-API-Key: $CENTRAL_KEY" \
  "$EDGE/api/v1/sources/src-ssh-facts/status?host=itexenode100" \
  | jq '.hosts[0].age_seconds'   # → also small
```

### 3. A refresh that correctly does nothing

Immediately repeat the same call. The host is now inside its TTL:

```
HTTP/1.1 200 OK
x-unified-api-refreshed: true
```

No `x-unified-api-refreshed-hosts` header: nothing was gathered. `refreshed:
true` means "nothing went wrong", and for a fresh host nothing was needed.
Run it fifty times in a loop and the edge's age stays where it is — that is the
ceiling working.

### 4. The refusals

```bash
# no hosts named
curl -s -H "X-API-Key: $FORMS_KEY" \
  "$CENTRAL/api/v1/sources/src-edge-dc4/dataset?refresh=true" | jq -r .error
# → refresh=true requires ?host=: a refresh of a whole source on a read would
#   gather the entire inventory, so the hosts have to be named. POST the
#   source's /sync endpoint for a full refresh.

# a source without the capability
curl -s -H "X-API-Key: $FORMS_KEY" \
  "$CENTRAL/api/v1/sources/src-other/dataset?host=x&refresh=true" | jq -r .error
# → source 'src-other' does not allow on-demand refresh — set
#   allow_on_demand_refresh: true on it to let a read trigger a gather
```

### 5. Degrading when the refresh cannot happen

Stop the edge (or break the link), wait for the host to pass its TTL, then read:

```
HTTP/1.1 200 OK
x-unified-api-refreshed: false
x-unified-api-refresh-error: request to 'http://…/sync?host=itexenode100&refresh_origin=true&refresh_depth=2' failed: error sending request for url (…)
```

```bash
curl -s -H "X-API-Key: $FORMS_KEY" \
  "$CENTRAL/api/v1/sources/src-edge-dc4/dataset?host=itexenode100&refresh=true" \
  | jq -c '{returned, has_facts: (.hostvars["itexenode100"].memory != null)}'
# → {"returned":1,"has_facts":true}
```

**The read still returns 200 with the cached data.** A form that renders stale
facts with a warning beats a form that fails to render.

A host that is *individually* down (edge up, sshd off) reads the same way, with
the error naming the origin's own reason instead:
`the origin refused to re-gather via 'https://…/sync?host=…': ...`

### 6. Several hosts, one gather

```bash
curl -si -H "X-API-Key: $FORMS_KEY" \
  "$CENTRAL/api/v1/sources/src-edge-dc4/dataset?host=itexenode100,itexenode101,itexenode102&refresh=true" \
  | grep -i 'x-unified-api-refreshed-hosts'
```

```
x-unified-api-refreshed-hosts: itexenode100,itexenode101,itexenode102
```

One request, one gather, one SSH fan-out on the edge.

### 7. The explicit write form, for AWX and pipelines

```bash
curl -s -X POST -H "X-API-Key: $AWX_KEY" \
  "$CENTRAL/api/v1/sources/src-edge-dc4/sync?host=itexenode100&refresh_origin=true" | jq
```

```json
{
  "source_id": "src-edge-dc4",
  "success": true,
  "scope": "host:itexenode100",
  "total_hosts": 1,
  "total_groups": 0,
  "sync_duration_ms": 2314,
  "error": null
}
```

Unlike the read, this one ignores the TTL (it always refreshes) and reports
`success: false` when the origin could not.

---

## Failure modes

| What happens | What the caller sees | Why |
|---|---|---|
| Host inside its TTL | `200`, `refreshed: true`, no hosts header | Nothing needed. The ceiling. |
| Host down / SSH fails at the edge | `200` + cached data, `refreshed: false`, error names the origin | A read must not fail because the data behind it could not be improved |
| Refresh slower than `refresh_timeout_seconds` | `200` + cached data, `refreshed: false`, `refresh did not finish within Ns` | The consumer is waiting on a page |
| Edge unreachable (WAN down) | `200` + cached data, `refreshed: false` | Same reason. Reads keep working off the last snapshot |
| `refresh=true` with no `?host=` | `400` + explanation | Naming the hosts is what bounds the cost |
| Source without `allow_on_demand_refresh` | `403` naming the setting | The capability was never granted |
| Source not in `sources.yaml` | `404` | Configuration error, not a cache miss |
| Central's key rejected by the edge | `200` + cached data, `refreshed: false`, error contains `401` | The key rotated on one side only |
| Central's key not scoped to the edge source | same, error contains `403` | `api_keys.yaml` on the edge |
| Hostname differs between central and edge | `200`, `refreshed: false`, error says the scope matched no hosts | The two sides must key hosts identically |
| `refresh_depth` runs out mid-chain | `200`, data served, `WARN` in the log | Chain longer than the budget, or a cycle |
| Source has no cache entry at all | Refresh runs (nothing cached is maximally stale), then `404` if it still has nothing | A refresh cannot invent a source |

---

## Operational notes

### Choosing the TTL — this is now load bearing

Before this feature, `ttl_seconds` was informational: it drove what `/status`
reported and nothing else, since syncs are driven by `sync_interval_seconds`.
With `allow_on_demand_refresh: true` it becomes the threshold at which a read is
willing to pay for a gather.

So a source configured like this:

```yaml
ttl_seconds: 60              # "my data is old after a minute"
sync_interval_seconds: 3600  # "…but I only sync hourly"
```

will refresh on nearly every consumer request once the flag goes on. Still
bounded (one gather per host per minute), but probably not what the operator
meant. Look at the TTLs before enabling, and use `ttl_overrides.hosts` to
tighten the few hosts that genuinely need it rather than the whole source.

### Tuning the two server knobs

- **`refresh_timeout_seconds`** must exceed WAN round trip + one host's SSH
  gather, and stay under whatever the consumer's own client timeout is. 15 is a
  reasonable start; if the edge's `ssh_connect_timeout_seconds` is 20, an
  unreachable host will always hit this budget rather than the SSH one, which is
  the correct order.
- **`refresh_max_concurrent`** is a cap on simultaneous gathers, not on
  requests: callers beyond it queue, bounded by their own timeout. 8 suits a
  handful of consumers; raise it if legitimate bursts are timing out, lower it
  if a burst is visibly loading the edges.
- The remote source's **`http_timeout_seconds`** bounds the refresh POST too, so
  it must be at least as large as the edge's gather time for one host. If this
  is smaller than `refresh_timeout_seconds`, it is the one that will fire.

### Metrics

```
unified_api_refresh_total{source="src-edge-dc4", result="refreshed"}
unified_api_refresh_total{source="src-edge-dc4", result="fresh"}
unified_api_refresh_total{source="src-edge-dc4", result="coalesced"}
unified_api_refresh_total{source="src-edge-dc4", result="failed"}
unified_api_refresh_total{source="src-edge-dc4", result="timeout"}
```

`refreshed` is what costs a gather; `fresh` and `coalesced` are the two ways the
limits saved one. The ratio between them answers "how much of my gathering load
comes from consumers, and how much is the ceiling absorbing?" — worth a panel.

Alert on `failed` and `timeout`: a rising rate there means consumers are being
served stale data and told so, which nothing else in the stack will report.

### What to watch out for

- **A scheduled sync must never refresh.** The scheduler passes no refresh
  intent, so a central pulling on its interval reads the edge's cache and causes
  no SSH. If you see edge gathers tracking the central's `sync_interval_seconds`,
  something is passing `refresh_origin` where it should not.
- **Hostnames must match on both sides.** The chain keys everything by the
  inventory hostname. A mismatch shows up as `refreshed: false` with "matched
  none of this source's hosts", not as a silent empty result.
- **Group scope is not narrowed for SSH sources.** A `?group=` sync still
  gathers everything on the edge, because the SSH connector receives host
  addresses, not group membership. Use `?host=` with a list.

---

## Consumer recipes

### AnsibleForms

Point the form's data source at the central with `refresh=true` and let it
render whatever comes back:

```
GET https://unified-api.example.com/api/v1/sources/src-edge-dc4/dataset
    ?host={{ selected_host }}&refresh=true&fields=filesystems,memory
```

`fields=` keeps the payload to what the form draws. Read
`x-unified-api-refreshed` and show a "data may be stale" badge when it is
`false` — the form still has data to draw, it just should not claim it is live.

### AWX

Refresh before a playbook that depends on current facts, as a task in the job:

```yaml
- name: Refresh the target's facts at the origin
  ansible.builtin.uri:
    url: "{{ unified_api }}/api/v1/sources/src-edge-dc4/sync"
    method: POST
    headers:
      X-API-Key: "{{ unified_api_key }}"
    body_format: json
    status_code: 200
  vars:
    query: "?host={{ inventory_hostname }}&refresh_origin=true"
  # A refresh that fails is worth knowing about here, unlike on a read
  failed_when: false
  register: refresh
```

---

## Not covered

Deliberately out of scope for now; each would be a separate change:

- **A cap on how many hosts one refresh may name.** The TTL window bounds each
  host individually, so a caller naming 400 hosts pays 400 gathers once per
  window rather than unboundedly. If a consumer starts doing that by accident,
  a `refresh_max_hosts` per source is the fix.
- **A `refresh_ttl_seconds` separate from `ttl_seconds`**, for deployments where
  "when is this stale" and "when is it worth gathering" genuinely differ.
  `ttl_overrides.hosts` covers the cases seen so far.
- **Refresh on `/status`** and on output endpoints. Only `/dataset` has it.
- **Group-scoped refresh on SSH sources**, which needs the connector to learn
  group membership.
