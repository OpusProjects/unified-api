# Deployment

## Container image

CI publishes `ghcr.io/opusprojects/unified-api` on every push to `main` (tagged
`latest` and with the commit SHA) and on every `vX.Y.Z` git tag (tagged `X.Y.Z`).
Production deployments should pin a version tag; `latest` tracks `main`. The image is a two-stage build (Rust builder →
`debian:trixie-slim`), runs as a non-root `unified` user, declares a
`HEALTHCHECK` (via python3) hitting `/healthz`, and bakes in the repo's
`config/` and the sample scripts under `tests/` as defaults. For connector
scripts it ships `python3` (plus a `python` symlink) with the commonly
imported libraries preinstalled from apt — `requests`, `PyYAML`, `jinja2` —
and `git` for project checkouts. Scripts needing anything beyond that still
require baking a derived image (a per-project `requirements.txt` venv is on
the roadmap).

```bash
docker run -p 8182:8182 \
  -v $(pwd)/config:/app/config:ro \
  -e UNIFIED_API_KEY=change-me \
  -e SECTION9_USERNAME=admin -e SECTION9_PASSWORD=... \
  ghcr.io/opusprojects/unified-api:latest
```

That's the zero-config demo. For a realistic deployment, walk through the
complete example below.

## A complete worked example

One `config/` directory exercising **every connector type, every credential
type and every secret-delivery mechanism**. Copy it, delete what you don't
need. The pieces reference each other by id, so the startup validation will
tell you if you break a reference while trimming.

### config.yaml — server, cache persistence, project checkouts

```yaml
server:
  host: "0.0.0.0"
  port: 8182
  # cors_allowed_origins: ["https://forms.example.com"]  # only for browser consumers

# Survive restarts: snapshot the cache to disk and reload at boot.
# Both paths below live on one writable volume in k8s (see Running it).
cache:
  persistence:
    path: "/var/lib/unified-api/cache.json"
    interval_seconds: 60

projects:
  dir: "/var/lib/unified-api/projects"
```

### credentials.yaml — all three types, both delivery mechanisms

Credentials never hold secrets; they say **where to read them**. Two
mechanisms: environment variables (`env_prefix` + `secret_keys`) or files
(`secret_file` for a JSON of values, `file_keys` for files a script consumes
by path, like SSH keys).

```yaml
# username_password from env vars: reads D42_USERNAME and D42_PASSWORD,
# the connector script receives CREDENTIAL_USERNAME / CREDENTIAL_PASSWORD
cred-d42:
  name: "Device42 API"
  type: "username_password"
  env_prefix: "D42"
  secret_keys:
    username: "USERNAME"
    password: "PASSWORD"

# token from env: reads NETBOX_TOKEN → script sees CREDENTIAL_TOKEN
cred-netbox:
  name: "NetBox API token"
  type: "token"
  env_prefix: "NETBOX"
  secret_keys:
    token: "TOKEN"

# ssh_key: the username from env, the private key as a FILE the connector
# opens by path (file_keys entries are passed as <key>_path)
cred-fleet-ssh:
  name: "Fleet SSH"
  type: "ssh_key"
  env_prefix: "FLEET_SSH"
  secret_keys:
    username: "USERNAME"
  file_keys:
    ssh_key: "/run/secrets/fleet-ssh/id_ed25519"

# token from a JSON file instead of env: {"token": "glpat-..."} — handy when
# the platform delivers secrets as mounted files rather than env vars
cred-gitlab:
  name: "GitLab read token"
  type: "token"
  secret_file: "/run/secrets/gitlab.json"
  secret_keys:
    token: "token"
```

### projects.yaml — the three sync styles

```yaml
# Auto: cloned at boot, re-pulled every 30 min
prj-connectors:
  name: "Connector scripts"
  git_url: "https://gitlab.example.com/infra/connectors.git"
  credential_id: "cred-gitlab"        # private repo → token over https
  sync_interval_seconds: 1800

# Manual / pipeline-driven: existing checkout used as-is at boot (pair with a
# persistent volume); updates only via POST /api/v1/projects/prj-inventories/sync
prj-inventories:
  name: "Static inventories"
  git_url: "https://gitlab.example.com/infra/inventories.git"
  credential_id: "cred-gitlab"
  sync_on_boot: false
```

### sources.yaml — all three connector types

```yaml
# 1a. script connector running an UNMODIFIED Ansible dynamic inventory script
#     (the d42/VMware kind): needs --list and emits _meta-style JSON
src-d42:
  name: "Device42"
  project_id: "prj-connectors"
  script_path: "d42/d42_inventory.py"     # resolved inside the checkout
  script_args: ["--list"]
  output_format: "ansible"
  credential_ids: ["cred-d42"]
  sync_interval_seconds: 300
  ttl_seconds: 600

# 1b. script connector with a native-format script (reads SOURCE_CONFIG,
#     prints {"hostvars": ..., "groups": ...}) and per-group TTL overrides
src-netbox:
  name: "NetBox"
  project_id: "prj-connectors"
  script_path: "netbox/fetch.py"
  credential_ids: ["cred-netbox"]
  sync_interval_seconds: 600
  ttl_seconds: 3600
  ttl_overrides:
    groups:
      production: 900
  config:                                  # free-form, script-specific
    api_url: "https://netbox.example.com"

# 2. native SSH connector gathering Ansible local facts across a fleet —
#    no script anywhere, the binary connects in parallel. The host list is
#    DYNAMIC: whatever src-inventory currently knows (chained sources)
src-fleet-facts:
  name: "Fleet facts over SSH"
  connector_type: "ssh"
  project_id: "prj-connectors"             # required by schema; unused by ssh
  script_path: "gather_facts"              # label in facts mode
  credential_ids: ["cred-fleet-ssh"]
  sync_interval_seconds: 300
  ttl_seconds: 600
  hosts_from_source:
    source: "src-inventory"
    match_pattern:
      groups: ["linux"]                    # absent = every host in the source
    connect_via: "ansible_host_then_hostname"
  config:
    concurrency: "50"
    ssh_connect_timeout_seconds: "30"
    gather_mode: "facts"
    fact_path: "/etc/ansible/facts.d"

# 2b. same connector in script mode with a STATIC host list: run a remote
#     command per host and store its JSON output as that host's vars
src-fleet-disks:
  name: "Fleet disk report"
  connector_type: "ssh"
  project_id: "prj-connectors"
  script_path: "/usr/local/bin/disk-report"
  script_args: ["--json"]
  credential_ids: ["cred-fleet-ssh"]
  ttl_seconds: 3600                        # no interval: manual sync only
  config:
    hosts: "web01.example.com,web02.example.com"
    gather_mode: "script"

# 3. static Ansible YAML inventory parsed natively from a git checkout
#    (inventory.yaml + group_vars/ + host_vars/) — no process, no ansible-core
src-inventory:
  name: "Static inventory"
  connector_type: "static_inventory"
  project_id: "prj-inventories"
  script_path: "inventory.yaml"
  sync_interval_seconds: 300
  ttl_seconds: 600
```

### enrichers.yaml and endpoints.yaml

```yaml
# enrichers.yaml — post-process a cached source (dataset on stdin,
# partial dataset on stdout)
enrich-reachability:
  name: "Probe reachability"
  source_id: "src-fleet-facts"
  script_path: "enrichers/probe.py"        # resolved via project_id if set
  project_id: "prj-connectors"
  script_args: ["--timeout", "5"]
  sync_interval_seconds: 900
```

```yaml
# endpoints.yaml — merge cached sources through a transformer script;
# consumers POST here (AWX inventory source, AnsibleForms, ...)
ep-awx-full:
  name: "Full AWX inventory"
  source_ids: ["src-d42", "src-fleet-facts", "src-inventory"]
  project_id: "prj-connectors"
  script_path: "outputs/ansible_inventory.py"
```

### api_keys.yaml — per-consumer permissions

```yaml
key-awx:
  name: "AWX"
  env: "UNIFIED_API_KEY_AWX"
  role: "admin"

key-forms:
  name: "AnsibleForms"
  env: "UNIFIED_API_KEY_FORMS"
  # restricted (the default): filtered lists, 403 elsewhere
  sources: ["src-fleet-facts"]
  endpoints: ["ep-awx-full"]
```

### What the deployment must inject

Everything secret, in one table — this is the contract between the config
above and whatever delivers secrets:

| Name | Kind | Consumed by |
|---|---|---|
| `UNIFIED_API_KEY_AWX`, `UNIFIED_API_KEY_FORMS` | env | API authentication (`api_keys.yaml`) |
| `D42_USERNAME`, `D42_PASSWORD` | env | `cred-d42` |
| `NETBOX_TOKEN` | env | `cred-netbox` |
| `FLEET_SSH_USERNAME` | env | `cred-fleet-ssh` |
| `/run/secrets/fleet-ssh/id_ed25519` | file (mode 0400) | `cred-fleet-ssh` → SSH connector |
| `/run/secrets/gitlab.json` | file | `cred-gitlab` → git clones |

## Running the example

### docker run

```bash
docker run -p 8182:8182 \
  -v $(pwd)/config:/app/config:ro \
  -v unified-api-state:/var/lib/unified-api \
  -v $(pwd)/secrets/id_ed25519:/run/secrets/fleet-ssh/id_ed25519:ro \
  -v $(pwd)/secrets/gitlab.json:/run/secrets/gitlab.json:ro \
  -e UNIFIED_API_KEY_AWX -e UNIFIED_API_KEY_FORMS \
  -e D42_USERNAME -e D42_PASSWORD -e NETBOX_TOKEN -e FLEET_SSH_USERNAME \
  ghcr.io/opusprojects/unified-api:0.3.1
```

(`-e VAR` without a value forwards it from your shell — an easy way to keep
secrets out of the command line.)

### docker compose

```yaml
services:
  unified-api:
    image: ghcr.io/opusprojects/unified-api:0.3.1
    ports: ["8182:8182"]
    env_file: .env                    # the env table above; chmod 600, gitignored
    volumes:
      - ./config:/app/config:ro
      - state:/var/lib/unified-api
      - ./secrets/id_ed25519:/run/secrets/fleet-ssh/id_ed25519:ro
      - ./secrets/gitlab.json:/run/secrets/gitlab.json:ro
volumes:
  state:
```

### Kubernetes

The config files become a ConfigMap (mounted at `CONFIG_DIR`), the env table
becomes a Secret, the file secrets become Secret volume mounts, and the
writable state gets a PVC:

```yaml
# Deployment (fragments)
env:
  - name: CONFIG_DIR
    value: "/etc/unified-api"
envFrom:
  - secretRef:
      name: unified-api-env          # every env var from the table, in one go
volumeMounts:
  - {name: config, mountPath: /etc/unified-api, readOnly: true}
  - {name: state, mountPath: /var/lib/unified-api}
  - {name: fleet-ssh, mountPath: /run/secrets/fleet-ssh, readOnly: true}
  - {name: gitlab, mountPath: /run/secrets/gitlab.json, subPath: gitlab.json, readOnly: true}
volumes:
  - {name: config, configMap: {name: unified-api-config}}
  - {name: state, persistentVolumeClaim: {claimName: unified-api-state}}
  - name: fleet-ssh
    secret:
      secretName: unified-api-secrets
      items: [{key: ssh-private-key, path: id_ed25519, mode: 0400}]
  - name: gitlab
    secret: {secretName: unified-api-secrets}
```

How the `unified-api-env` / `unified-api-secrets` Secrets get their values is
the *secret variant* decision — plain Secret, Sealed Secrets, or External
Secrets Operator — covered with a full ESO example in
[GitOps with ArgoCD → Secrets: three variants](#secrets-three-variants) below.

### Smoke test

```bash
BASE=http://localhost:8182 ; KEY="$UNIFIED_API_KEY_AWX"
curl -s $BASE/healthz                                    # → ok
curl -s $BASE/readyz | jq                                # pending sources listed until first syncs
curl -s -H "x-api-key: $KEY" -X POST $BASE/api/v1/sources/src-inventory/sync | jq
curl -s -H "x-api-key: $KEY" $BASE/api/v1/sources | jq   # freshness per source
curl -s -H "x-api-key: $KEY" $BASE/api/v1/sources/src-inventory/dataset | jq '.hostvars | keys'
curl -s -H "x-api-key: $KEY" $BASE/api/v1/projects | jq  # checkout state (admin only)
curl -s -H "x-api-key: $KEY" -X POST $BASE/api/v1/endpoints/ep-awx-full | jq
curl -s $BASE/metrics | grep unified_api_sync_total      # public, no key
```

A failing credential fails the sync with a clear error (never a silent
skip); a script that prints Ansible JSON while the source says `native`
logs a WARN telling you to set `output_format: "ansible"`.

## Federation across datacenters

For hosts spread over multiple datacenters, don't SSH across the WAN from
one central instance (firewall openings into every DC, one key with global
reach, WAN latency on every handshake). Deploy **one instance per DC** doing
the local work, and **one central** that federates them with
`connector_type: "remote"` — consumers only ever talk to the central:

```
          DC MADRID                                      DC FRANKFURT
┌─────────────────────────────┐               ┌─────────────────────────────┐
│      local fleet (LAN)      │               │      local fleet (LAN)      │
│   web01 · web02 · db01 · …  │               │   app01 · app02 · db02 · …  │
│         ▲                   │               │         ▲                   │
│         │ parallel SSH      │               │         │ parallel SSH      │
│         │ (russh, key that  │               │         │ (russh, key that  │
│         │  never leaves MAD)│               │         │  never leaves FRA)│
│  ┌──────┴───────────────┐   │               │  ┌──────┴───────────────┐   │
│  │  unified-api-mad     │   │               │  │  unified-api-fra     │   │
│  │  ▸ src-fleet  (ssh)  │   │               │  │  ▸ src-fleet  (ssh)  │   │
│  │  ▸ src-d42 (script)  │   │               │  │  ▸ src-netbox        │   │
│  │  cache ⇄ PVC         │   │               │  │  cache ⇄ PVC         │   │
│  │  key-central ······· │◄──┼── restricted  │  │  key-central ······· │   │
│  │   (src-fleet only)   │   │   per edge    │  │   (src-fleet only)   │   │
│  └──────────┬───────────┘   │               │  └──────────┬───────────┘   │
└─────────────┼───────────────┘               └─────────────┼───────────────┘
              │                                             │
              │        HTTPS · GET /dataset + /status       │
              │        restricted X-API-Key                 │
              │        the data's REAL age travels along    │
              └──────────────────────┬──────────────────────┘
                                     ▼
                     ┌───────────────────────────────┐
                     │      unified-api (CENTRAL)    │
                     │   ▸ src-madrid    (remote) ─┐ │
                     │   ▸ src-frankfurt (remote) ─┤ │
                     │   cache ⇄ PVC               │ │
                     │   ep-global ◄───────────────┘ │
                     │   (merged world inventory)    │
                     └───────────────┬───────────────┘
                                     │ POST /api/v1/endpoints/ep-global
                                     ▼
                     AWX / Ascender · AnsibleForms · curl
```

Arrows point in the direction the CONNECTION is initiated (the central
pulls the edges, consumers pull the central) — the only firewall openings
are HTTPS from the central to each edge.

The wire protocol is the API itself: `GET /dataset` returns exactly the
Dataset shape a connector must produce, and `/status` provides the data's
real age so freshness reporting stays truthful across hops.

### Edge configuration (each DC)

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

### Central configuration

```yaml
# central: credentials.yaml — one token credential per DC
cred-edge-mad:
  name: "Edge Madrid API key"
  type: "token"
  env_prefix: "EDGE_MAD"
  secret_keys:
    token: "TOKEN"          # reads env EDGE_MAD_TOKEN
```

```yaml
# central: sources.yaml — one remote source per DC
src-madrid:
  name: "DC Madrid"
  connector_type: "remote"
  project_id: "prj-unused"        # required by schema; unused by remote
  script_path: "src-fleet"        # the source id ON THE EDGE
  credential_ids: ["cred-edge-mad"]
  sync_interval_seconds: 120      # how often the central re-pulls the edge
  ttl_seconds: 600
  config:
    url: "https://unified-api-mad.example.com"
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
  source_ids: ["src-madrid"]      # add one id per DC
  script_path: "tests/adapters/out/output/ansible_inventory.py"
```

Secrets the central's deployment must inject: `EDGE_MAD_TOKEN` (the value of
the edge's `UNIFIED_API_KEY_CENTRAL`) — one env var per DC — plus the
central's own API keys for its consumers.

### Verifying a federation

```bash
# 1. the edge has data of its own
curl -s -H "x-api-key: $EDGE_KEY" https://unified-api-mad…/api/v1/sources/src-fleet/status \
  | jq .dataset_age_seconds        # e.g. 42 — remember this number

# 2. sync the central and read the same source through it
curl -s -X POST -H "x-api-key: $CENTRAL_KEY" https://central…/api/v1/sources/src-madrid/sync | jq .total_hosts
curl -s -H "x-api-key: $CENTRAL_KEY" https://central…/api/v1/sources/src-madrid/status \
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

### Operational notes

- **A WAN cut does not lose data**: the central's cached copy keeps being
  served (stale beats nothing) and its `unified_api_sync_total{result="error"}`
  metric flags the broken link — alert on that.
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

## CI/CD pipeline

`.github/workflows/build.yaml`, two jobs:

1. **test** — on every push and PR: `cargo fmt --check`, `cargo clippy --all-targets
   -- -D warnings`, `cargo test` (with `Swatinem/rust-cache`)
1. **audit** — scans `Cargo.lock` against the RUSTSEC advisory database for
   known-vulnerable dependency versions; ignored advisories are documented in
   `.cargo/audit.toml`
2. **build-image** — after tests pass: on PRs the image is **built but not
   pushed** (catches Dockerfile breakage before merge); pushes to `main` and
   `v*` tags also publish to GHCR (`sha` + `latest` from `main`, semver from tags)
1. **release** — on `v*` tags only, after the image is published: creates a
   GitHub Release whose notes are that version's section from `CHANGELOG.md`

The CI badge in the README tracks this workflow.

## Kubernetes notes

- **Probes:** liveness → `GET /healthz`; readiness → `GET /readyz` (503 until at
  least one configured source has synced, so pods join the Service only with data)
- **Config:** mount a ConfigMap at a path and point `CONFIG_DIR` at it
- **Secrets:** the app never talks to a secrets backend directly today. Inject
  values as env vars matching each credential's `env_prefix`/`secret_keys` (e.g.
  from a Secret via `envFrom`, populated by External Secrets Operator), or mount
  files and reference them via `secret_file` / `file_keys`. Native HashiCorp Vault
  resolution is on the roadmap — it will slot in as another `SecretsPort` adapter
- **API key:** set `UNIFIED_API_KEY` from a Secret; without it the API is open
- **Replicas:** the cache is per-process and not shared. Multiple replicas each
  sync independently — fine for read-scaling with scheduled syncs, but manual
  `PUT/DELETE /hosts` edits and on-demand syncs only touch the replica that served
  them. Run a single replica if consumers rely on those.
- **Metrics:** add a `ServiceMonitor` (or scrape annotations) pointing at
  `/metrics` on the HTTP port — the endpoint is public, no API key needed.

## GitOps with ArgoCD

The reference deployment is plain manifests in a git repo watched by an ArgoCD
`Application` with automated sync (`prune` + `selfHeal`), typically as one entry
in an app-of-apps. A working layout:

```
apps/unified-api/manifests/
├── namespace.yaml
├── serviceaccount.yaml
├── configmap.yaml        # config/*.yaml files, mounted at CONFIG_DIR
├── <secret variant>      # see below
├── deployment.yaml       # pinned image tag, probes, non-root
├── service.yaml
├── servicemonitor.yaml   # Prometheus scrape of /metrics
└── ingress.yaml
```

Practical notes:

- **Pin the image** to a release tag (`ghcr.io/opusprojects/unified-api:X.Y.Z`)
  with `imagePullPolicy: IfNotPresent`. Upgrades are then a one-line git commit,
  and ArgoCD rolls the Deployment. `latest` + `Always` gives you unreviewed
  upgrades on every pod restart.
- **Sync waves** help ordering: namespace/serviceaccount first, ConfigMap and
  the secret before the Deployment, so the pod never starts against half the
  config.
- **Config changes**: editing the ConfigMap alone does not restart pods. Either
  bump a pod-template annotation (e.g. a config checksum) in the same commit or
  restart the rollout after sync.

### Secrets: three variants

The app never talks to a secrets backend itself — it only reads env vars
(matching each credential's `env_prefix`/`secret_keys`) and mounted files
(`file_keys`, and `UNIFIED_API_KEY` for the API itself). Any of these produces
the same `Secret` the Deployment references; they differ in where the actual
values live:

1. **Plain Secret** — create it out-of-band
   (`kubectl create secret generic unified-api-secrets --from-literal=api-key=…`)
   and keep it out of git. Simplest, but not GitOps: the secret is invisible to
   ArgoCD, must be documented somewhere, and has to be recreated by hand on
   cluster rebuild. Fine for dev clusters and first bring-up.
2. **Sealed Secrets** — encrypt the Secret with `kubeseal` against the cluster's
   sealed-secrets controller and commit the resulting `SealedSecret` manifest.
   Fully GitOps with no external dependency, but ciphertexts are bound to that
   controller's key pair: lose the key (cluster rebuild without backup) and every
   sealed secret must be re-sealed. Rotation means re-sealing and committing.
3. **External Secrets Operator** — commit an `ExternalSecret` that references
   paths in an external backend (Vault, AWS Secrets Manager, …) via a
   `(Cluster)SecretStore`; ESO materializes and refreshes the `Secret` on an
   interval. Git holds only references, values live and rotate in the backend.
   Costs you running ESO and the backend, and pods start only after ESO has
   synced the Secret at least once.

An ESO example wiring both the API key and the SSH connector credential:

```yaml
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata:
  name: unified-api-secrets
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: vault-unified-api
    kind: ClusterSecretStore
  target:
    name: unified-api-secrets
  data:
    - secretKey: api-key
      remoteRef: {key: unified-api/config, property: api_key}
    - secretKey: ssh-username
      remoteRef: {key: ssh/unified-api-connector, property: username}
    - secretKey: ssh-private-key
      remoteRef: {key: ssh/unified-api-connector, property: private_key}
```

The Deployment consumes it the same way under all three variants: `env` /
`envFrom` for `UNIFIED_API_KEY` and the `secret_keys` values, plus a volume
mounting the SSH private key at the path `file_keys` points to (mode `0400`).
If you use ESO with ArgoCD auto-sync, add `ignoreDifferences` for the
`conversionStrategy`/`decodingStrategy`/`metadataPolicy` defaults the operator
writes back, or the app never reports Synced.

## Scheduling behavior

Background sync tasks start at boot for every source with
`sync_interval_seconds > 0` (tokio `interval`, first tick immediately). Enrichers
with an interval likewise. A failed run logs the error and waits for the next tick —
there is no retry/backoff beyond the interval itself. Every script execution is
bounded by its `timeout_seconds` (default 300), so a hung connector or enricher
cannot wedge its scheduler task. Shutdown is graceful for
in-flight HTTP requests (SIGTERM/Ctrl-C); scheduler tasks stop with the process.

## Observability

Structured logs via `tracing` to stdout; tune with `RUST_LOG` (e.g.
`unified_api=debug`). Every HTTP request is logged at INFO with method, path,
status and latency (a `tower-http` trace layer); set `tower_http=debug` for more
detail. Sync and enrich outcomes are logged with source ids, host counts and
durations.

**Prometheus metrics** are exposed at `GET /metrics` (public, like the health
probes — scrapers don't carry the API key):

| Metric | Labels | Meaning |
|---|---|---|
| `unified_api_sync_total` | `source`, `result` | Sync runs, success vs error |
| `unified_api_sync_duration_seconds` | `source` | Sync duration histogram |
| `unified_api_enrich_total` | `source`, `result` | Enricher runs |
| `unified_api_enrich_duration_seconds` | `source` | Enricher duration histogram |
| `unified_api_endpoint_total` | `endpoint`, `result` | Output endpoint runs |
| `unified_api_endpoint_duration_seconds` | `endpoint` | Endpoint duration histogram |

Timed-out and failed runs count as `result="error"`, so alerting on the error
rate catches hung connectors too.
