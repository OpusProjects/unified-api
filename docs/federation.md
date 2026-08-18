# Federation across datacenters

One instance per datacenter doing the local gathering, one central that
federates them — so consumers only ever talk to the central, and no SSH key or
firewall opening needs global reach.

This page is the topology. The `connector_type: remote` script contract it rests
on lives in [connectors](connectors.md), and [views](views.md) are how a consumer
stops needing to know which instance a host sits behind.

- [Edge configuration (each DC)](#edge-configuration-each-dc)
- [Central configuration](#central-configuration)
- [The scope contract](#the-scope-contract)
- [Verifying a federation](#verifying-a-federation)
- [Operational notes](#operational-notes)

For hosts spread over multiple datacenters, don't SSH across the WAN from
one central instance (firewall openings into every DC, one key with global
reach, WAN latency on every handshake). Deploy **one instance per DC** doing
the local work, and **one central** that federates them with
`connector_type: "remote"` — consumers only ever talk to the central:

```
          DATACENTER A                                  DATACENTER B
┌─────────────────────────────┐               ┌─────────────────────────────┐
│      local fleet (LAN)      │               │      local fleet (LAN)      │
│  web01 . web02 . db01 . ... │               │  app01 . app02 . db02 . ... │
│         ^                   │               │         ^                   │
│         │ parallel SSH      │               │         │ parallel SSH      │
│         │ (russh, key that  │               │         │ (russh, key that  │
│         │  never leaves dc1)│               │         │  never leaves dc2)│
│  ┌──────┴───────────────┐   │               │  ┌──────┴───────────────┐   │
│  │  unified-api-dc1     │   │               │  │  unified-api-dc2     │   │
│  │  > src-fleet  (ssh)  │   │               │  │  > src-fleet  (ssh)  │   │
│  │  > src-d42 (script)  │   │               │  │  > src-netbox        │   │
│  │  cache <-> PVC       │   │               │  │  cache <-> PVC       │   │
│  │  key-central ....... │<──┼── restricted  │  │  key-central ....... │   │
│  │   (src-fleet only)   │   │   per edge    │  │   (src-fleet only)   │   │
│  └──────────┬───────────┘   │               │  └──────────┬───────────┘   │
└─────────────┼───────────────┘               └─────────────┼───────────────┘
              │                                             │
              │        HTTPS . GET /dataset + /status       │
              │        restricted X-API-Key                 │
              │        the data's REAL age travels along    │
              └──────────────────────┬──────────────────────┘
                                     v
                     ┌───────────────────────────────┐
                     │      unified-api (CENTRAL)    │
                     │   > src-dc1 (remote) ───┐     │
                     │   > src-dc2 (remote) ───┤     │
                     │   cache <-> PVC         │     │
                     │   ep-global <───────────┘     │
                     │   (merged world inventory)    │
                     └───────────────┬───────────────┘
                                     │ POST /api/v1/endpoints/ep-global
                                     v
                    AWX / Ascender . AnsibleForms . curl
```

Arrows point in the direction the CONNECTION is initiated (the central
pulls the edges, consumers pull the central) — the only firewall openings
are HTTPS from the central to each edge.

The wire protocol is the API itself: `GET /dataset` returns exactly the
Dataset shape a connector must produce, and `/status` provides the data's
real age so freshness reporting stays truthful across hops.

---

## Edge configuration (each DC)

A completely normal instance — its sources are whatever that DC needs (see
the worked example above). The only federation-specific piece is a
**restricted API key** for the central:

```yaml
# edge: api_keys.yaml
key-central:
  name: "Central aggregator"
  env: "UNIFIED_API_KEY_CENTRAL"
  # restricted (default role): the central can read THIS source and nothing else
  sources: ["src-fleet"]
```

The deployment injects `UNIFIED_API_KEY_CENTRAL` on the edge (same secret
mechanisms as everything else). Generate one distinct key per edge.

---

## Central configuration

What the central instance needs in order to treat each edge as a remote source it can read.

```yaml
# central: credentials.yaml — one token credential per DC
cred-edge-dc1:
  name: "Edge datacenter A API key"
  type: "token"
  env_prefix: "EDGE_DC1"
  secret_keys:
    token: "TOKEN"          # reads env EDGE_DC1_TOKEN
```

```yaml
# central: sources.yaml — one remote source per DC
src-dc1:
  name: "Datacenter A"
  connector_type: "remote"
  project_id: "prj-unused"        # required by schema; unused by remote
  script_path: "src-fleet"        # the source id ON THE EDGE
  credential_ids: ["cred-edge-dc1"]
  sync_interval_seconds: 120      # how often the central re-pulls the edge
  ttl_seconds: 600
  config:
    url: "https://unified-api-dc1.example.com"
    # http_timeout_seconds: "30"  # default 30
    # insecure_tls: "true"        # only for self-signed edges; opt-in
```

```yaml
# central: projects.yaml — the stub the schema requires
prj-unused:
  name: "unused"
  git_url: "https://example.invalid/unused.git"
  sync_on_boot: false
```

```yaml
# central: endpoints.yaml — one merged world view for consumers
ep-global:
  name: "Global inventory"
  source_ids: ["src-dc1"]      # add one id per DC
  script_path: "tests/adapters/out/output/ansible_inventory.py"
```

Secrets the central's deployment must inject: `EDGE_DC1_TOKEN` (the value of
the edge's `UNIFIED_API_KEY_CENTRAL`) — one env var per DC — plus the
central's own API keys for its consumers.

---

## The scope contract

Beyond the dataset itself, an edge can tell the central **which hosts it owns** —
so a central [view](views.md) routes per-host reads without duplicating the edge's config.

Every source serves `GET /api/v1/sources/{id}/scope`: the ownership claim
derived from **configuration, never cache contents** — an explicit
`advertise_scope` block (`groups` + `hosts`), or the `hosts_from_source`
pattern an SSH source already gathers by. On the edge that means the claim is
maintained exactly once, where the gathering is configured:

```yaml
# edge: sources.yaml — the source states what it owns
src-fleet:
  # ... connector config ...
  advertise_scope:
    groups: ["dc1"]            # optional when hosts_from_source already says it
```

The central's remote syncs fetch the edge's `/scope` alongside the dataset,
best effort, and remember the **last-known claim**: an unreachable edge keeps
routing the way it last claimed (stale routing beats no routing). A view
member opts into that claim with `owns.advertised: true` — the full resolution
order (live claim → last-known → declared fallback → nothing, never
everything) lives in [views → advertised ownership](views.md#advertised-ownership).

**Mixed-version rollouts degrade cleanly.** An edge too old to serve `/scope`
answers `404`; the central treats that as "no claim" and the member falls back
to its declared `groups`/`hosts`, exactly as before the feature existed. So
the safe order is: keep the declared fallback while edges upgrade, watch each
member's `ownership_mode` in `GET /status` flip from `fallback` to
`advertised`, and only then delete the fallback — from that point the edge's
config is the single truth, and adding a datacenter's worth of hosts to an
edge never needs a central change again.

---

## Verifying a federation

A federation is working when the central sees what the edge sees; these calls check that end to end.

```bash
# 1. the edge has data of its own
curl -s -H "x-api-key: $EDGE_KEY" https://unified-api-dc1.../api/v1/sources/src-fleet/status \
  | jq .dataset_age_seconds        # e.g. 42 — remember this number

# 2. sync the central and read the same source through it
curl -s -X POST -H "x-api-key: $CENTRAL_KEY" https://central.../api/v1/sources/src-dc1/sync | jq .total_hosts
curl -s -H "x-api-key: $CENTRAL_KEY" https://central.../api/v1/sources/src-dc1/status \
  | jq .dataset_age_seconds        # must be >= the edge's number, NOT 0
```

That second check is the point of the native connector: the central reports
the **origin's** age (dataset-level and per-host). If it says `0` right
after a sync of old edge data, something is off.

Failure modes, all loud:

| Symptom in the sync error | Meaning |
|---|---|
| `answered 401` | The token credential isn't the edge's API key |
| `answered 403` | The edge key exists but isn't allowed that source id |
| `answered 404` | Wrong remote source id, or the edge hasn't synced it yet |
| `request … failed` (network) | WAN/DNS/TLS problem — the central keeps serving its last good copy |
| WARN `could not read remote ages` | Data arrived fine; only the age lookup failed (treated as fresh) |

---

## Operational notes

How the arrangement behaves once it is running, and what it costs to change its shape.

- **A WAN cut does not lose data**: the central's cached copy keeps being
  served (stale beats nothing) and its `unified_api_sync_total{result="error"}`
  metric flags the broken link — alert on that.
- **An unchanged edge costs almost nothing to poll**: full pulls revalidate
  with the edge's `ETag` and a `304` skips the transfer — see
  [connectors → remote](connectors.md#remote-sources--federation-connector_type-remote).
  `unified_api_remote_not_modified_total` counts the skips, so a low ratio of
  skips to syncs on a source that rarely changes is worth a look.
- **Adding a DC** = deploy an edge (same manifests, different config), give
  it a `key-central`, add one credential + one remote source on the central,
  and append its id to `ep-global`. No consumer changes.
- **Rotation**: swap the edge's `UNIFIED_API_KEY_CENTRAL` value and the
  central's `EDGE_*_TOKEN` at the same time; both are env vars, both
  instances restart independently.
- **TTL sizing**: the central's `ttl_seconds` should be ≥ the edge's sync
  interval + the central's own — freshness at the central reflects the
  ORIGIN's age, so an edge that stops syncing will (correctly) show as stale
  at the central even while the transfer keeps succeeding.
- Centrals can be federated by another instance in turn (regions → global),
  and the same pattern aggregates non-geographic pairs: dev + prod, homelab
  + work.
