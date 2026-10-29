# Deployment

How the service is packaged and run: the container image, a complete worked configuration,
and what changes when the target is Kubernetes, ArgoCD or a CI pipeline.

- [Container image](#container-image)
- [A complete worked example](#a-complete-worked-example)
- [Running the example](#running-the-example)
- [CI/CD pipeline](#cicd-pipeline)
- [Kubernetes notes](#kubernetes-notes)
- [GitOps with ArgoCD](#gitops-with-argocd)
- [Scheduling and observability](#scheduling-and-observability)

---

## Container image

CI publishes `ghcr.io/opusprojects/unified-api` on every push to `main` (tagged
`latest` and with the commit SHA) and on every `vX.Y.Z` git tag (tagged `X.Y.Z`).
Production deployments should pin a version tag; `latest` always means the newest release (only tags publish images), never the tip of `main`. The image is a two-stage build (`docker/Dockerfile`: Rust builder →
`debian:trixie-slim`), runs as a non-root `unified` user, declares a
`HEALTHCHECK` (via python3) hitting `/healthz`, and bakes in the repo's
`config/` and the sample scripts under `tests/` as defaults. For connector
scripts it ships `python3` (plus a `python` symlink) with the commonly
imported libraries preinstalled from apt — `requests`, `PyYAML`, `jinja2` —
and `git` for project checkouts. Scripts needing anything beyond that
declare it in their project's `requirements.txt` and set `python_venv: true`
on the project — the app builds and maintains a virtualenv per project (see
[configuration](configuration.md#projects)); a derived image is only
needed for non-Python tooling.

```bash
docker run -p 8182:8182 \
  -v $(pwd)/config:/app/config:ro \
  -e UNIFIED_API_KEY=change-me \
  -e SECTION9_USERNAME=admin -e SECTION9_PASSWORD=... \
  ghcr.io/opusprojects/unified-api:latest
```

That's the zero-config demo. For a realistic deployment, walk through the
complete example below.

---

## A complete worked example

One `config/` directory exercising **every connector type, every credential
type and every secret-delivery mechanism**. Copy it, delete what you don't
need. The pieces reference each other by name, so the startup validation will
tell you if you break a reference while trimming.

```
config/
├── config.yaml
├── credentials/   cred-d42.yaml  cred-netbox.yaml  cred-fleet-ssh.yaml  cred-gitlab.yaml
├── projects/      prj-connectors.yaml  prj-inventories.yaml
├── connectors/    d42.yaml  netbox.yaml  ssh-facts.yaml  ssh-disk-report.yaml
├── sources/       src-d42.yaml  src-netbox.yaml  src-fleet-facts.yaml  src-fleet-disks.yaml  src-inventory.yaml
├── enrichers/     enrich-reachability.yaml
├── endpoints/     ep-awx-full.yaml
└── api_keys/      key-awx.yaml  key-forms.yaml
```

One item per file, as below; the folders are a convention for people, and the
top-level key in each file is what says what it holds (see
[configuration](configuration.md#the-directory)). The blocks below show each
folder's files one after another.

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

### credentials — all three types, both delivery mechanisms

Credentials never hold secrets; they say **where to read them**. Two
mechanisms: environment variables (`env_vars`, each named in full) or files
(`secret_file` for a JSON of values, `file_keys` for files a script consumes
by path, like SSH keys).

```yaml
# credentials/cred-d42.yaml — username_password from env vars: reads
# D42_USERNAME and D42_PASSWORD, the connector script receives
# CREDENTIAL_USERNAME / CREDENTIAL_PASSWORD
credentials:
  - name: cred-d42
    description: "Device42 API"
    type: "username_password"
    env_vars:
      username: "D42_USERNAME"
      password: "D42_PASSWORD"

# credentials/cred-netbox.yaml — token from env: reads NETBOX_TOKEN > script
# sees CREDENTIAL_TOKEN
credentials:
  - name: cred-netbox
    description: "NetBox API token"
    type: "token"
    env_vars:
      token: "NETBOX_TOKEN"

# credentials/cred-fleet-ssh.yaml — ssh_key: the username from env, the
# private key as a FILE the connector opens by path (file_keys entries are
# passed as <key>_path)
credentials:
  - name: cred-fleet-ssh
    description: "Fleet SSH"
    type: "ssh_key"
    env_vars:
      username: "FLEET_SSH_USERNAME"
    file_keys:
      ssh_key: "/run/secrets/fleet-ssh/id_ed25519"

# credentials/cred-gitlab.yaml — token from a JSON file instead of env:
# {"token": "glpat-..."} — handy when the platform delivers secrets as
# mounted files rather than env vars
credentials:
  - name: cred-gitlab
    description: "GitLab read token"
    type: "token"
    secret_file: "/run/secrets/gitlab.json"
    secret_keys:
      token: "token"
```

### projects — the three sync styles

```yaml
# projects/prj-connectors.yaml — auto: cloned at boot, re-pulled every 30 min
projects:
  - name: prj-connectors
    description: "Connector scripts"
    git_url: "https://gitlab.example.com/infra/connectors.git"
    credentials: ["cred-gitlab"]      # private repo > token over https
    sync_interval_seconds: 1800

# projects/prj-inventories.yaml — manual / pipeline-driven: existing checkout
# used as-is at boot (pair with a persistent volume); updates only via
# POST /api/v1/projects/prj-inventories/sync
projects:
  - name: prj-inventories
    description: "Static inventories"
    git_url: "https://gitlab.example.com/infra/inventories.git"
    credentials: ["cred-gitlab"]
    sync_on_boot: false
```

### connectors — how each kind of system is read

```yaml
# connectors/d42.yaml — an UNMODIFIED Ansible dynamic inventory script (the
# d42/VMware kind): needs --list and emits _meta-style JSON
connectors:
  - name: d42
    description: "Device42 dynamic inventory"
    project: "prj-connectors"
    path: "d42/d42_inventory.py"          # resolved inside the checkout
    script_args: ["--list"]

# connectors/netbox.yaml — a script that reads SOURCE_CONFIG and prints
# Ansible inventory JSON, as every script connector does
connectors:
  - name: netbox
    description: "NetBox"
    project: "prj-connectors"
    path: "netbox/fetch.py"

# connectors/ssh-facts.yaml — native SSH reading Ansible local facts: no
# script anywhere, the binary connects in parallel
connectors:
  - name: ssh-facts
    description: "Fleet facts over SSH"
    type: "ssh"
    location: "host"
    path: "/etc/ansible/facts.d/*.fact"   # each file a fact named after it

# connectors/ssh-disk-report.yaml — a script kept in a project, streamed to
# each host and run there; what it prints is that host's vars
connectors:
  - name: ssh-disk-report
    description: "Disk report over SSH"
    type: "ssh"
    project: "prj-inventory"
    path: "ssh/disk-report.sh"
    script_args: ["--json"]
```

### sources — every connector type

```yaml
# sources/src-d42.yaml
sources:
  - name: src-d42
    description: "Device42"
    connector: "d42"
    credentials: ["cred-d42"]
    sync_interval_seconds: 300
    ttl_seconds: 600

# sources/src-netbox.yaml — groups/production.yaml gives its production
# group a 15-minute TTL (see configuration.md, "Hosts and groups")
sources:
  - name: src-netbox
    description: "NetBox"
    connector: "netbox"
    credentials: ["cred-netbox"]
    sync_interval_seconds: 600
    ttl_seconds: 3600
    config:                                # free-form, script-specific
      api_url: "https://netbox.example.com"

# sources/src-fleet-facts.yaml — the host list is DYNAMIC: whatever
# src-inventory currently knows (chained sources)
sources:
  - name: src-fleet-facts
    description: "Fleet facts over SSH"
    connector: "ssh-facts"
    credentials: ["cred-fleet-ssh"]
    sync_interval_seconds: 300
    ttl_seconds: 600
    hosts_from_source:
      source: "src-inventory"
      match_pattern:
        groups: ["linux"]                  # absent = every host in the source
      connect_via: "ansible_host_then_hostname"
    config:
      concurrency: "50"
      ssh_connect_timeout_seconds: "30"

# sources/src-fleet-disks.yaml — a STATIC host list
sources:
  - name: src-fleet-disks
    description: "Fleet disk report"
    connector: "ssh-disk-report"
    credentials: ["cred-fleet-ssh"]
    ttl_seconds: 3600                      # no interval: manual sync only
    config:
      hosts: "web01.example.com,web02.example.com"

# sources/src-inventory.yaml — static Ansible YAML inventory parsed natively
# from a git checkout (inventory.yaml + group_vars/ + host_vars/) — no
# process, no ansible-core, and no connector item: static_inventory is built in
sources:
  - name: src-inventory
    description: "Static inventory"
    connector: "static_inventory"
    project: "prj-inventories"
    path: "inventory.yaml"
    sync_interval_seconds: 300
    ttl_seconds: 600
```

The one connector type not shown, `remote` (another unified-api instance), is built in
as well; see [federation](federation.md).

### enrichers and endpoints

```yaml
# enrichers/enrich-reachability.yaml — post-process a cached source (dataset
# on stdin, partial dataset on stdout)
enrichers:
  - name: enrich-reachability
    description: "Probe reachability"
    target: "src-fleet-facts"
    project: "prj-connectors"
    script_path: "enrichers/probe.py"      # resolved via project if set
    script_args: ["--timeout", "5"]
    sync_interval_seconds: 900
```

```yaml
# endpoints/ep-awx-full.yaml — merge cached sources through a transformer
# script; consumers POST here (AWX inventory source, AnsibleForms, ...)
endpoints:
  - name: ep-awx-full
    description: "Full AWX inventory"
    sources: ["src-d42", "src-fleet-facts", "src-inventory"]
    project: "prj-connectors"
    script_path: "outputs/ansible_inventory.py"
```

### api_keys — per-consumer permissions

```yaml
# api_keys/key-awx.yaml
api_keys:
  - name: key-awx
    description: "AWX"
    env_vars:
      key: "UNIFIED_API_KEY_AWX"
    role: "admin"

# api_keys/key-forms.yaml
api_keys:
  - name: key-forms
    description: "AnsibleForms"
    env_vars:
      key: "UNIFIED_API_KEY_FORMS"
    # restricted (the default): filtered lists, 403 elsewhere
    sources: ["src-fleet-facts"]
    endpoints: ["ep-awx-full"]
```

### What the deployment must inject

Everything secret, in one table — this is the contract between the config
above and whatever delivers secrets:

| Name | Kind | Consumed by |
|---|---|---|
| `UNIFIED_API_KEY_AWX`, `UNIFIED_API_KEY_FORMS` | env | API authentication (the `api_keys` items) |
| `D42_USERNAME`, `D42_PASSWORD` | env | `cred-d42` |
| `NETBOX_TOKEN` | env | `cred-netbox` |
| `FLEET_SSH_USERNAME` | env | `cred-fleet-ssh` |
| `/run/secrets/fleet-ssh/id_ed25519` | file (mode 0400) | `cred-fleet-ssh` → SSH connector |
| `/run/secrets/gitlab.json` | file | `cred-gitlab` → git clones |

---

## Running the example

The same configuration started three ways — Docker, Compose and Kubernetes — and a smoke test to confirm it came up.

### docker run

```bash
docker run -p 8182:8182 \
  -v $(pwd)/config:/app/config:ro \
  -v unified-api-state:/var/lib/unified-api \
  -v $(pwd)/secrets/id_ed25519:/run/secrets/fleet-ssh/id_ed25519:ro \
  -v $(pwd)/secrets/gitlab.json:/run/secrets/gitlab.json:ro \
  -e UNIFIED_API_KEY_AWX -e UNIFIED_API_KEY_FORMS \
  -e D42_USERNAME -e D42_PASSWORD -e NETBOX_TOKEN -e FLEET_SSH_USERNAME \
  ghcr.io/opusprojects/unified-api:0.10.3
```

(`-e VAR` without a value forwards it from your shell — an easy way to keep
secrets out of the command line.)

### docker compose

The compose file is a real, CI-linted file rather than a block to copy from
this page: [`deploy/compose/compose.yaml`](../deploy/compose/compose.yaml).
Put your `config/` directory, `.env` (the env table above; chmod 600,
gitignored) and `secrets/` next to it and `docker compose up -d`. Pin the
image tag to the release you deploy.

### Kubernetes

The config files become a ConfigMap (mounted at `CONFIG_DIR`), the env table
becomes a Secret, the file secrets become Secret volume mounts, and the
writable state gets a PVC. The complete manifests — Deployment with probes,
non-root and resources, Service, PVC — live in
[`deploy/k8s/`](../deploy/k8s/), linted with kubeconform in CI.

A ConfigMap is **flat**: `--from-file` does not recurse, and a key cannot hold a
`/`. The configuration is item files in folders (`sources/`, `connectors/`...),
but the loader does not care where a file sits — the top-level key inside says
what it holds — so flatten the tree into the keys, as below, or give each key a
`path:` under `items:` in the volume to rebuild the folders. A directory that
changes often is better pushed with the [configuration API](config-api.md) onto
a PVC.

```bash
# config/sources/src-a.yaml becomes the key sources-src-a.yaml
kubectl create configmap unified-api-config $(cd config && find . -name '*.yaml' \
  | sed 's|^\./||' | while read -r f; do printf -- '--from-file=%s=config/%s ' "${f//\//-}" "$f"; done)
kubectl create secret generic unified-api-env --from-env-file=.env
kubectl create secret generic unified-api-secrets \
  --from-file=ssh-private-key=secrets/id_ed25519 \
  --from-file=gitlab.json=secrets/gitlab.json
kubectl apply -k deploy/k8s/
```

How the `unified-api-env` / `unified-api-secrets` Secrets get their values is
the *secret variant* decision — plain Secret, Sealed Secrets, or External
Secrets Operator — covered with a full ESO example in
[GitOps with ArgoCD → Secrets: three variants](#secrets-three-variants) below.

### Smoke test

```bash
BASE=http://localhost:8182 ; KEY="$UNIFIED_API_KEY_AWX"
curl -s $BASE/healthz                                    # > ok
curl -s $BASE/readyz | jq                                # pending sources listed until first syncs
curl -s -H "x-api-key: $KEY" -X POST $BASE/api/v1/sources/src-inventory/sync | jq
curl -s -H "x-api-key: $KEY" $BASE/api/v1/sources | jq   # freshness per source
curl -s -H "x-api-key: $KEY" $BASE/api/v1/sources/src-inventory/dataset | jq '.hostvars | keys'
curl -s -H "x-api-key: $KEY" $BASE/api/v1/projects | jq  # checkout state (admin only)
curl -s -H "x-api-key: $KEY" -X POST $BASE/api/v1/endpoints/ep-awx-full | jq
curl -s $BASE/metrics | grep unified_api_sync_total      # public, no key
```

A failing credential fails the sync with a clear error (never a silent
skip); a script that prints the `{"hostvars", "groups"}` shape older versions
read fails the sync, saying to print Ansible inventory JSON instead.

> Deploying across several datacenters? The topology — one instance per DC,
> one central federating them — is [its own page](federation.md).

---

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

---

## Kubernetes notes

What changes when the deployment target is a cluster rather than a single host.

- **Probes:** liveness → `GET /healthz`; readiness → `GET /readyz` (503 until at
  least one configured source has synced, so pods join the Service only with data)
- **Config:** mount a ConfigMap at a path and point `CONFIG_DIR` at it. It is
  flat (see [Kubernetes](#kubernetes) above) and its files are symlinks into a
  hidden `..data` folder — the loader follows the links and skips the hidden
  folder, so a mounted ConfigMap loads as-is. It is also read-only, which
  matters for the [config API](config-api.md)
- **Secrets:** three ways in. Inject values as env vars matching each
  credential's `env_vars` (e.g. from a Secret via `envFrom`,
  populated by External Secrets Operator); mount files and reference them via
  `secret_file` / `file_keys`; or resolve natively from HashiCorp Vault — give
  a credential `vault:` and a `vault_path`, naming a `type: vault` credential
  (token or Kubernetes auth), see [configuration](configuration.md#credentials).
  Adoption is per credential, so the three coexist during a migration
- **API key:** set `UNIFIED_API_KEY` from a Secret; without it the API is open
- **Replicas:** the cache is per-process and not shared. Multiple replicas each
  sync independently — fine for read-scaling with scheduled syncs, but manual
  `PUT/DELETE /hosts` edits and on-demand syncs only touch the replica that served
  them. Run a single replica if consumers rely on those.
- **Metrics:** add a `ServiceMonitor` (or scrape annotations) pointing at
  `/metrics` on the HTTP port — the endpoint is public, no API key needed.

---

## GitOps with ArgoCD

The reference deployment is plain manifests in a git repo watched by an ArgoCD
`Application` with automated sync (`prune` + `selfHeal`), typically as one entry
in an app-of-apps. A working layout:

```
apps/unified-api/manifests/
├── namespace.yaml
├── serviceaccount.yaml
├── configmap.yaml        # config.yaml + the item files (flattened), mounted at CONFIG_DIR
├── <secret variant>      # see below
├── deployment.yaml       # start from deploy/k8s/deployment.yaml in this repo
├── service.yaml          # start from deploy/k8s/service.yaml
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
(named by each credential's `env_vars`) and mounted files
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
`envFrom` for `UNIFIED_API_KEY` and the variables `env_vars` names, plus a volume
mounting the SSH private key at the path `file_keys` points to (mode `0400`).
If you use ESO with ArgoCD auto-sync, add `ignoreDifferences` for the
`conversionStrategy`/`decodingStrategy`/`metadataPolicy` defaults the operator
writes back, or the app never reports Synced.

---

## Scheduling and observability

How often things run, what is logged, and the Prometheus metrics to alert on
have [their own page](observability.md).
