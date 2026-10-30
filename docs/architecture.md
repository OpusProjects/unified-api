# Architecture

Unified API is a **hexagonal monolith**: a single binary organized as ports & adapters.
There are no external data dependencies — no Redis, no database — the cache lives
in process memory (DashMap) and configuration comes from YAML files.

- [The hexagon](#the-hexagon)
- [Driving vs driven adapters](#driving-vs-driven-adapters)
- [Request flows](#request-flows)
- [Concurrency model](#concurrency-model)
- [Async trait objects](#async-trait-objects)
- [Shared with unified-gitops](#shared-with-unified-gitops)

---

## The hexagon

The shape in one picture: adapters on the outside, the application core in the middle, and the direction each arrow points.

```
            ┌───────────────────────── adapters (outside) ─────────────────────────┐
            │                                                                      │
 driving >  │  http/ (axum handlers, auth, routes, OpenAPI)     scheduler/         │
            │            │                                          │              │
            │            v                                          v              │
            │  ┌──────────────────── application/ ────────────────────┐            │
            │  │   sync.rs        enrich.rs        credentials.rs     │            │
            │  └──────────────────────────┬───────────────────────────┘            │
            │                             v                                        │
            │  ┌───────────────────── ports/ ─────────────────────────┐            │
            │  │  CachePort  ConnectorPort  EnricherPort              │            │
            │  │  OutputPort  SecretsPort                             │            │
            │  └──────────────────────────┬───────────────────────────┘            │
            │                             v                                        │
            │  ┌───────────────────── domain/ ────────────────────────┐            │
            │  │  Dataset  CacheEntry  Source  Credential  Enricher   │            │
            │  └──────────────────────────────────────────────────────┘            │
            │                                                                      │
 driven  >  │  cache/  connectors/  enrichers/  output/  secrets/                  │
            └──────────────────────────────────────────────────────────────────────┘
```

**Dependency direction: `adapters → application → ports → domain`, never the reverse.**

| Layer | Path | Contents | May depend on |
|---|---|---|---|
| Domain | `src/domain/` | Pure types + logic (`Dataset`, `CacheEntry` TTL/merge logic, config-shaped types) | `std`, `serde`, and the types shared through [auth](https://github.com/OpusProjects/auth) |
| Application | `src/application/` | The use cases: `sync_source`, `run_enricher`, `resolve_credentials` | domain, ports |
| Ports | `src/ports/` | Trait interfaces the use cases need | domain |
| Adapters | `src/adapters/` | Everything that touches the outside world, both directions | application, ports, domain |

The composition root — the one place concrete adapters are chosen — is `AppBuilder`
in `src/lib.rs` (plus `main.rs`, which reads env/config and hands them in).
`AppState` (`src/state.rs`) holds the ports as `Arc<dyn Trait>` plus the static config maps.

---

## Driving vs driven adapters

Adapters come in two kinds, distinguished by which way the call travels across the boundary.

- **Driving** (requests come *in*): `adapters/in/http/` (axum) and `adapters/in/scheduler/`
  (interval timers). Both are thin: they translate their trigger (an HTTP request, a
  tick) into a call to the same `application/` function and translate the outcome back
  (JSON response, log line). Use-case logic exists **once**, in `application/`.
- **Driven** (we reach *out*): under `adapters/out/` — the cache, the two connectors,
  the enricher and output executors, and secrets resolution.

---

## Request flows

**On-demand sync** — `POST /api/v1/sources/{id}/sync`:

1. `http::sync::sync_source` resolves the `Source` and builds a `SyncScope`
   (full / host / group) from query params
2. `application::sync::sync_source` resolves credentials via `SecretsPort`,
   runs the right `ConnectorPort` (script, SSH, static inventory or remote, per
   the `connector_type` the source resolved from its connector at load),
   and applies the resulting `Dataset` to the cache according to scope and
   `sync_mode` (replace / merge)
3. The handler maps the `SyncOutcome` to a `SyncResult` JSON body

**Scheduled sync** — `scheduler::start_sync_tasks` spawns one tokio task per source
with `sync_interval_seconds > 0`; each tick calls the *same*
`application::sync::sync_source` with `SyncScope::Full` and logs the outcome.
Enrichers with an interval get the same treatment via `application::enrich::run_enricher`.

---

## Concurrency model

Handlers and scheduler tasks run concurrently on the tokio runtime and share the
cache. Reads take a cloned snapshot (`CachePort::get`) — cheap, because the entry
holds its dataset behind `Arc`: the clone shares the data, and a writer that
catches up later copy-on-writes (`Arc::make_mut`) instead of disturbing readers
(see [caching.md](caching.md#the-read-path-shared-data-serialize-once-json)).
**All mutations go through the atomic operations** — `CachePort::update(key, f)` and
`merge_or_insert(key, dataset, ttl, f)` — which run the caller's closure under the
cache's own lock, so read-modify-write cycles cannot lose concurrent writes.
The get → modify → set pattern is forbidden (it operates on a clone and silently
drops whatever landed in between). Closures passed to these operations must be fast
and must never call back into the cache.

Long-running work (executing a connector or enricher script) is never done under the
lock: the enricher takes a read snapshot, runs the script, then merges atomically.

---

## Async trait objects

Ports need `dyn`-compatible async methods, so they return `Pin<Box<dyn Future + Send>>`
(see the comment in `ports/connector.rs`, and the `SecretsFuture` alias in
`ports/secrets.rs`). Adapters clone their inputs into the `async move` block.

---

## Shared with unified-gitops

Credential resolution, LDAP sign-in, API keys, browser sessions and git checkouts are not this repository's code:
they come from [auth](https://github.com/OpusProjects/auth), [git](https://github.com/OpusProjects/git) and [web](https://github.com/OpusProjects/web), pinned by tag in `Cargo.toml`.

| Crate | What it provides | Re-exported here as |
|---|---|---|
| `secrets-rs` | `Credential`, `SecretsPort`, the env and Vault adapters, the cache, `build_chain` | `domain::credential`, `ports::secrets`, `adapters::out::secrets` |
| `auth-rs` | `DirectoryPort`, `LdapDirectory`, `LdapConfig<R>`, the login use case and throttle | `ports::directory`, `adapters::out::directory`, `application::login` |
| `auth-rs` (`http`) | API key registry and middleware, session store, CSRF, the browser-form layer | `adapters::in::http::auth`, `adapters::in::http::session` |
| `git-rs` | `GitPort`, `CliGit` (clone and pull through the git binary, credentials off the command line) | `ports::git`, `adapters::out::git::cli` |
| `web-rs` | `ApiError`, `ErrorBody`, audit events, the web UI serving, route help from the OpenAPI document, the config store and API | `http::error`, `http::audit`, `http::ui_assets`, `http::spec`, `http::config`, `ports::config_store`, `adapters::out::config` |

The re-exports keep the layers readable: a port is still listed under `ports/`, an adapter under
`adapters/out/`. What stays here is this service's own: the load-time checks on a credential
(`CredentialRules`), the access rule type (sources and endpoints) and what it grants (`grant_for`), `Permissions`, how
api_keys.yaml becomes keys, the session cookie's name, and the login and Users pages.
A fix to the shared code is made in the auth repository and reaches this service with a new tag.

