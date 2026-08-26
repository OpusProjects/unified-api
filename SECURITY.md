# Security Policy

## Supported versions

Only the latest release line receives security fixes.

| Version | Supported |
|---|---|
| 0.x (latest release) | Yes |
| older tags | No |

## Reporting a vulnerability

Please **do not open a public issue** for security problems.

Use GitHub's private vulnerability reporting instead:
[Report a vulnerability](https://github.com/OpusProjects/unified-api/security/advisories/new)
— it opens a private thread with the maintainers.

Include what you can: affected endpoint or component, reproduction steps, and
impact. You should hear back within a week. Once a fix ships, the advisory is
published and credited unless you prefer otherwise.

## Scope notes

- Unified API is designed to run on a **trusted internal network**; treat
  network exposure and key handling as part of your deployment's threat model.
- Authentication is **API keys** declared in `api_keys.yaml`: each key is
  `role: admin` (everything) or restricted to explicit source/endpoint id
  lists, and its secret is read from the environment variable the definition
  names — never from the YAML. The legacy `UNIFIED_API_KEY` env var still
  works as one extra admin key. Keys are compared in constant time. With **no
  keys configured at all, authentication is off** and every caller is treated
  as admin — the startup log says so loudly; do not run that way outside
  local development.
- `/healthz`, `/readyz` and the Swagger UI are always public. `/metrics` is
  public by default, and its exposition names every source id and host count —
  a description of your inventory topology. On a shared network, set
  `server.metrics_require_auth: true`.
- The **configuration API** (`config_api.enabled`, off by default) makes the
  configuration directory writable over HTTP for admin keys — `api_keys.yaml`
  included, which is the same authority as editing the directory the container
  mounts. With it on, treat every admin key as root on the instance. Two
  guardrails are enforced before a reload commits: a change that would leave
  the API with no keys at all is refused, as is one naming a key env var that
  is not set. All config writes and reloads land in the audit log.
- Connector, enricher and output scripts run **with the daemon's privileges**.
  Config files, the scripts they point at, and the git projects the service
  clones and executes from are trusted input — protect who can write the
  config and who can push to the project repositories.
- Credentials are never stored by the service; they are resolved at sync time
  from environment variables, files, or Vault (KV v2, token or Kubernetes
  auth), passed to scripts via their environment, and held only in a short
  in-memory resolution cache (`secrets.cache_ttl_seconds`, default 60).
- There is **no rate limiting** on authentication attempts — a deliberate
  trade for the trusted-network posture. Front the service with a proxy if
  your deployment needs it.
