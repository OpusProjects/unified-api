# Views

One id over several sources. A **view** is a read-only composite: it presents
its members as one source, routes a per-host read to whichever member *owns*
that host, and delegates an on-demand refresh to that member. It gathers
nothing itself.

Consumers get one address for "the facts" and stop needing to know which
datacenter — and therefore which source — a host lives in.

---

## The problem it solves

[Federation](on-demand-refresh.md) solved half of this. `connector_type: remote`
means a central needs no credentials and no SSH path into a datacenter: the edge
that owns the hosts does the gathering, and the central holds its dataset.

But the **consumer** still had to know the topology, because the facts of a DC4
host lived under a different source id than those of an aa1 host:

```
              before                                    after

  consumer ──► src-ssh-pq-facts   (aa1)      consumer ──► vw-facts-all
           └─► src-edge-pq-dc4    (dc4)                        │
           └─► src-edge-cdi-dc01                       ┌───────┼───────┬────────┐
           └─► src-edge-cdi-dc04                       ▼       ▼       ▼        ▼
           └─► ...                                   aa1     dc4     dc01     dc04

  every consumer learns the split,          the view owns the routing table;
  and relearns it when an edge is added     consumers never see it change
```

That was not hypothetical: 28 AnsibleForms forms query one facts source, 17 of
them for a specific host, and one has a DC4 hostname written into its URL. The
moment the central's SSH source is scoped to aa1, those stop returning anything.

### Why not an output endpoint

An [output endpoint](api.md#output-endpoints) does merge sources and *is*
reachable per host (`?host=` reaches the script as `ENDPOINT_PARAMS`). Two
measured problems ruled it out:

- The script is fed **whole datasets on stdin**, always. On the aa1 + dc4 pair
  that is 11.2 MB + 6.2 MB through a pipe, plus spawning and parsing, on every
  call. The equivalent filtered read is **99 KB in 0.73 s**, done in Rust.
- **Endpoints have no refresh.** Routing consumers through one throws away the
  on-demand path entirely.

Right shape, wrong mechanism. A view filters in Rust and keeps refresh.

---

## Configuration — `views.yaml`

```yaml
vw-facts-all:
  name: "Facts, both datacenters"
  members:
    - source: "src-ssh-aa1"           # where the facts come from
      owns:
        source: "src-d42"             # inventory the pattern resolves against
        groups: ["datacenter_aa1"]
    - source: "src-edge-dc4"          # a remote (federated) member works too
      owns:
        source: "src-d42"
        groups: ["datacenter_dc4"]
        hosts: ["appliance01.dc4.example"]   # claimed literally as well
  # ttl_seconds: 30                   # optional; absent = inherit the owner's
```

| Field | Meaning |
|---|---|
| `members` | Ordered. The **first** member that claims a host wins it |
| `members[].source` | Source id this member serves data from |
| `members[].owns.source` | Source whose cached dataset the groups resolve against |
| `members[].owns.groups` | Groups of that source whose members this member owns |
| `members[].owns.hosts` | Hosts claimed literally, whether or not the inventory knows them |
| `ttl_seconds` | The view's own freshness policy. Absent = each host inherits its owning member's TTL |

An `owns` with neither `groups` nor `hosts` is a **catch-all**: everything its
ownership source knows. Useful as a last member ("whatever the others did not
claim"), dangerous as a first one — hence the ordering rule being explicit.

Unknown keys inside a view are a hard startup error, unlike the rest of the
config. Ownership is the routing table: `grups:` instead of `groups:` would
otherwise deserialize into an empty pattern, and an empty pattern claims
everything.

---

## Where it answers

A view is served on the **source routes**, in the same shapes. That is what
makes migrating free: a consumer changes one id and its parsing is untouched.

| Call | Behaviour |
|---|---|
| `GET /api/v1/sources/{view}/dataset` | Union of the members, in the raw `Dataset` shape |
| `GET .../dataset?host=X` | Resolve the owner, serve from that member's cache |
| `GET .../dataset?host=X&refresh=true` | Resolve the owner, delegate the refresh to it — for a `remote` member that propagates to the edge and gathers that one host |
| `GET .../dataset?group=g` | The merged group's hosts, each from its owner |
| `GET .../status` | Per host, the **owning member's** age and TTL, plus a `members` array |
| `GET .../groups`, `.../hosts` | The merged namespace and the union of hosts |
| `GET /api/v1/sources` | The view is listed with `kind: "view"` |
| `POST .../sync` | **400.** A view gathers nothing; the refusal names the members |
| `DELETE .../{view}`, host `PUT`/`DELETE` | **400.** A view holds no cache entry and is read-only |

Sync is refused rather than given an invented meaning. The tempting reading —
"sync every member" — would let a request aimed at one consumer's view quietly
re-gather somebody else's datacenter.

### Groups merge into one namespace

Same-named groups union their hosts and children; group vars collide by the same
first-member-wins rule as ownership. Membership is **not** filtered to what each
member owns: a group is a statement about the topology, and an aa1 member
listing DC4 hosts is true whether or not the view serves their facts from there.

### Auth

A view is granted exactly like a source, by its id under `sources:` in
`api_keys.yaml`. A key granted the view needs **no** access to the members: the
view is the contract, the members are internal topology.

```yaml
key-forms:
  name: "AnsibleForms"
  env: "UNIFIED_API_KEY_FORMS"
  sources: ["vw-facts-all"]        # not the members
```

---

## Ownership is declared, not inferred

The obvious implementation — "which member has X in its cached dataset" — is
wrong in two ways, both found in production:

- **Facts sources sync daily on purpose.** The bulk gather is a floor;
  freshness comes from on-demand. So a host provisioned this morning is in no
  cache until tomorrow — which is exactly the case on-demand refresh exists
  for. Cache-membership routing would refuse to route it.
- **Some hosts never enter a cache at all.** 28 appliances in the measured
  deployment take no SSH with that key (Device42, PureStorage, Zscaler
  connectors, InsightIQ). They are permanently the edge's responsibility and
  permanently absent from its data.

This was observed live: `itexenode100.dc4.pqe` was *not* in the central's
cached dataset, and `?refresh=true` still brought it in, because the whole
chain runs off the resolved host list rather than the cache.

But the resolved host list is not uniformly available either. An `ssh` member
with `hosts_from_source` can be resolved locally; a **`remote` member has no
`hosts_from_source` at the central** — the central's only local knowledge of
what that edge owns is its cached dataset, which is the thing that lags.

Declared ownership is the only rule that is local, fresh, and identical for both
member kinds. It costs a duplicated truth (the edge says "I am datacenter_dc4"
in its own config; the view repeats it), which is acceptable for a first
version. The no-drift variant is for the edge to advertise its scope over the
API and the view to read it.

```
  GET /sources/vw-facts-all/dataset?host=bwkftp101.dc4.pqe&refresh=true
        │
        ├─ 1. who claims it?  ── src-d42's datacenter_dc4 group ─► member src-edge-dc4
        │                          (the inventory, synced every 2 h)
        │
        ├─ 2. is it stale?    ── view ttl_seconds, or the member's ─► yes
        │
        ├─ 3. delegate         ─► refresh_hosts(src-edge-dc4, [bwkftp101])
        │                          └─► remote connector, refresh_origin ─► the edge
        │                                └─► edge SSHes that one host (281 ms)
        │
        └─ 4. read             ─► serve from src-edge-dc4's cache, now current
```

### A host nobody claims is a 404

Never a silent empty result, and never a default member. A default member turns
a config error into empty data nobody investigates. The 404 names the host, the
members, and the two things that cause it (the group is not listed, or the
inventory source has not synced). It also increments
`unified_api_view_unclaimed_hosts_total{view="..."}` so the same mistake is
visible on a dashboard before somebody reports it.

A host that **is** claimed but that its owner has no data for is different: it
routes normally (so a refresh can go and get it) and simply does not appear in
the data, exactly as it would from the member itself.

---

## TTL: the refresh gate, not a freshness label

`refresh=true` only gathers hosts that are **older than the TTL**. So whichever
TTL a view ends up with is not decoration — it decides whether a read pays for
an SSH session.

| `ttl_seconds` on the view | What governs |
|---|---|
| absent | the owning member's TTL — the member keeps deciding |
| set | the view's value, for every member's hosts |

A member's per-host and per-group `ttl_overrides` still win either way: *an
override beats the default*, here as everywhere else in the codebase. A view
cannot silently cancel the five-minute TTL somebody put on a critical host.

Declaring a view TTL shorter than a member's means two doors onto the same data
with different refresh rules. That is fine as a deliberate consumer-facing
policy — it is just worth knowing, rather than discovering by comparison.

---

## Reading `/status`

A view's status carries an extra `members` array, because the two ways a view
can answer "nothing" need different fixes:

```json
{
  "source_id": "vw-facts-all",
  "dataset_age_seconds": 473,
  "dataset_is_fresh": false,
  "ttl_seconds": 30,
  "total_hosts": 750,
  "returned": 1,
  "hosts": [
    {"hostname": "bwkftp101.dc4.pqe", "age_seconds": 12, "is_fresh": true, "ttl_seconds": 30}
  ],
  "members": [
    {"source_id": "src-ssh-aa1", "cached": true, "ownership_cached": true,
     "age_seconds": 473, "is_fresh": false, "ttl_seconds": 30, "total_hosts": 405,
     "sync_health": {"last_success_age_seconds": 473, "consecutive_failures": 0}},
    {"source_id": "src-edge-dc4", "cached": true, "ownership_cached": true,
     "age_seconds": 17, "is_fresh": true, "ttl_seconds": 30, "total_hosts": 345}
  ]
}
```

- `cached: false` — that member has never synced, so its hosts are missing from
  the data but still routable.
- `ownership_cached: false` — the *inventory* that member resolves ownership
  against has never synced, so its group patterns cannot be expanded and it
  claims nothing beyond hosts named literally. This is the state where a view
  404s everything, and it is the field that says so.
- The view's own `sync_health` is always absent: a view never syncs. The
  members' health is where "why is this stale" is answerable.
- `dataset_age_seconds` is the **stalest** member's, and `dataset_is_fresh` is
  true only when every member is cached and inside its TTL. A view is no more
  current than the least current thing it serves.

---

## What it does not do

**It saves no SSH.** While an aa1 member still gathers DC4 hosts, it keeps
gathering them; the view only changes who answers the read. The load saving is a
separate one-line change — `match_pattern: {groups: ["datacenter_aa1"]}` on the
aa1 member's `hosts_from_source`.

**Views do not nest.** A member must be a source. Config validation says so.

**Endpoints and enrichers cannot target a view.** Both work on cache entries and
a view has none; validation refuses them with a message naming the members.

---

## Caveats worth knowing

- **The ETag is generation-based, not content-based.** A source's plain
  `/dataset` derives a strong ETag from bytes it has already serialized; a view
  serializes on the fly, so its validator is the cache generation plus the
  query. Any write anywhere invalidates it — pessimistic (a needless
  re-transfer) but never stale, and it does not survive a restart.
- **The plain `/dataset` union is not free.** It borrows hostvars rather than
  copying them, but it does serialize the whole merged inventory on every
  request that is not a 304. Per-host reads, which is what consumers actually
  do, cost a hash lookup.
