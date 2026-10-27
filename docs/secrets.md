# Secrets

How credentials resolve, rotate, and reach the scripts. The one principle
everything follows: **configuration never holds a secret** — a `credentials` item
describes *where* to read each value, and the infrastructure (env vars,
mounted files, Vault) delivers it. Field reference lives in
[configuration → credentials](configuration.md#credentials).

- [The three backends](#the-three-backends)
- [Resolution order](#resolution-order)
- [What scripts receive](#what-scripts-receive)
- [The resolution cache and rotation](#the-resolution-cache-and-rotation)
- [Native Vault](#native-vault)
- [Failure semantics](#failure-semantics)
- [API keys are separate](#api-keys-are-separate)

---

## The three backends

A credential names one delivery mechanism for its values; the three coexist
in one deployment, so migrating between them is per credential, never
all-or-nothing.

| Backend | Declared by | Delivery |
|---|---|---|
| Environment variables | `env_vars` (field → the full variable name) | `SECTION9_USERNAME` etc., injected by the platform (k8s Secret via `envFrom`, `.env`, ESO) |
| JSON file | `secret_file` + `secret_keys` | A mounted file of values, re-read per resolution |
| Vault (KV v2) | `vault_path` (+ the `secrets.vault:` block) | Read over HTTPS at resolution time |

`file_keys` is orthogonal to all three: files a script consumes **by path**
(SSH keys, certificates) — the path is delivered, never the content.

---

## Resolution order

Per credential, exactly one value source wins — there is no fallback chain
between backends, because a secret silently coming from somewhere unexpected
is worse than an error.

1. `vault_path` set → the value comes from Vault (env/file settings on the
   same credential are not consulted).
2. Otherwise `env_vars` → environment variables, or `secret_file` → the
   JSON file.
3. `file_keys` entries are appended as `<key>_path` values in every case.

`env_vars` maps our names to full variable names: `{username: D42_USERNAME}`.
`secret_keys` maps our names to the fields of a store — the JSON file's or the
Vault secret's. The older `env_prefix` + `secret_keys` env form
(`D42` + `{username: USERNAME}`) still works but is deprecated: startup and
`--check-config` print the `env_vars` to write, and a later release refuses it.

---

## What scripts receive

A connector sees only the credentials its source declares — and nothing else:
the child environment is scrubbed, so no script can read the API keys or
another source's values. Each field arrives under its credential, as
`CREDENTIAL_<ID>_<KEY>` (`cred-vcenter-aa1`'s `username` →
`CREDENTIAL_VCENTER_AA1_USERNAME`: the name upper-cased, a leading `cred-`
dropped, anything else not a letter or digit as `_`), and also as
`CREDENTIAL_<KEY>` when no other of the source's credentials has that field.
A source with one credential per site reads each site's under its id; two
`username`s are never silently reduced to one. A field already named after its
credential (`avamar_aa1_url` on `cred-avamar-aa1`) is not prefixed twice, so
configurations written that way by hand deliver exactly what they did.

`env_vars: {username: D42_USERNAME}` on `cred-d42` reads `D42_USERNAME` and
delivers `CREDENTIAL_D42_USERNAME` and `CREDENTIAL_USERNAME`; a `file_keys`
entry `ssh_key: /run/secrets/id_rsa` delivers `CREDENTIAL_SSH_KEY_PATH`. The SSH connector and git checkouts
consume the same shapes (`username`, `ssh_key_path`, `token`) — git auth in
particular never puts a secret on a command line. See
[connectors](connectors.md) for the full script environment.

---

## The resolution cache and rotation

Resolution runs on every sync of every source — free against env vars, a
request storm against a networked backend — so successful resolutions are
reused for `secrets.cache_ttl_seconds` (default 60; `0` disables).

The TTL **is** the rotation latency, stated plainly: a value rotated in the
environment, on disk, or in Vault is picked up within `cache_ttl_seconds`,
not on the very next sync. Errors are never cached — a transient backend blip
is retried on the next resolution instead of being remembered for the TTL.

---

## Native Vault

Give a credential a `vault_path` and configure the `secrets.vault:` block;
everything without a `vault_path` keeps resolving exactly as before.

```yaml
secrets:
  cache_ttl_seconds: 60
  vault:
    address: "https://vault.example.com:8200"
    mount: "secret"                # KV v2 mount (default "secret")
    token_env: "VAULT_TOKEN"       # token auth: re-read per resolution,
                                   # so rotating the token needs no restart
    # kubernetes_role: "unified-api"   # OR Kubernetes auth: the service-account
    # jwt_path: "/var/run/secrets/kubernetes.io/serviceaccount/token"
    #                                  # JWT is exchanged for a client token,
    #                                  # cached and renewed at 80% of its lease
    timeout_seconds: 10
```

The adapter speaks **KV v2 only** (`/v1/<mount>/data/<path>`) and says so if
a v1-shaped answer comes back. A `vault_path` with no `secrets.vault:` block
fails validation at startup. The resolution cache above is what keeps the
sync schedule from becoming a request storm against Vault.

---

## Failure semantics

A credential that fails to resolve **halts the sync naming the credential** —
never a silent skip that would let a connector run half-authenticated and
fail later with a confusing error.

The failure lands in the source's `sync_health` (and its Prometheus gauges)
like any other sync error, the failing task backs off, and — because errors
are not cached — the next attempt re-asks the backend.

---

## API keys are separate

The keys consumers present to *this* API are `api_keys` items, follow the
same never-in-YAML principle (each definition names the env var holding its
secret), and are documented with the [API](api.md#authentication).

Their rotation is deliberately external: swap the env var's value and restart.
How the env vars and files themselves get into the deployment — plain Secret,
Sealed Secrets, External Secrets Operator — is the
[deployment page's](deployment.md#secrets-three-variants) decision.
