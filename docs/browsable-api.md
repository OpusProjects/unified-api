# Browsable API

The API, navigable in a browser. Every read route already returns JSON; a
**browsable API** adds two things on top of it: a link graph, so each response
names the resources next to it, and an HTML rendering of that same JSON, so a
person can click through it instead of composing `curl` commands.

Modelled on AWX's `/api/v2/`, which is the reference implementation of this
idea and the reason it is wanted here.

- [What AWX actually does](#what-awx-actually-does)
- [Two layers, not one](#two-layers-not-one)
- [Layer 1: the link graph](#layer-1-the-link-graph)
- [Layer 2: HTML by content negotiation](#layer-2-html-by-content-negotiation)
- [Collection flat, detail rich](#collection-flat-detail-rich)
- [Authentication: the cookie problem](#authentication-the-cookie-problem)
- [Writes: forms and CSRF](#writes-forms-and-csrf)
- [Escaping: why Swagger is safe and this is not](#escaping-why-swagger-is-safe-and-this-is-not)
- [New API surface](#new-api-surface)
- [Configuration](#configuration)
- [Rollout](#rollout)
- [What it does not do](#what-it-does-not-do)

---

## What AWX actually does

Two mechanisms that are easy to conflate:

1. **`related`.** Every AWX object carries a map of URLs to its neighbours, and
   `/api/v2/` is an index of every collection. This is what makes the API
   navigable. It works in `curl | jq` with no HTML anywhere.
2. **The HTML renderer.** Content negotiation: same route, same data, and when
   the client sends `Accept: text/html` the response is a page instead of a
   JSON body. It pretty-prints the JSON and turns anything that looks like one
   of its own routes into an `<a>`.

The second is a presentation layer over the first. Build them in that order:
after (1) the API is navigable and (2) is cosmetic; after (2) alone there is a
nicer way to look at a dead end.

## Two layers, not one

```
   handler          returns a typed struct, serialized by serde
      │             (unchanged — handlers know nothing about HTML)
      v
   related          a `related: {...}` map on the response structs
      │
      v
   negotiation      Accept: application/json ──> the bytes, as today
   middleware       Accept: text/html        ──> render(matched_path, json)
      │
      v
   renderer         a per-route view where one exists,
                    the generic JSON-tree view everywhere else
```

The negotiation middleware keeps the request's path and method *before* calling
the inner service, and afterwards resolves them against the OpenAPI document to
find the route TEMPLATE (`spec::lookup`). The template, never the raw URI: it is
a bounded set of known patterns, and it is also the key to everything the page
knows about itself — its documentation, its `Allow` line, and which actions it
offers.

A route with no registered renderer still renders: the generic view walks the
`serde_json::Value` and prints it as an indented tree. That is the property
worth protecting, because it means a route added next year is browsable the day
it is added, with no second place to register it.

### Where it sits in the stack

Outside `require_api_key`, inside `CompressionLayer`:

- **Outside auth** so it sees the `401` and can answer a browser with the login
  page instead of `{"error": "missing API key"}`.
- **Inside compression** so the HTML is gzipped like everything else.
- Applied to `api_routes` only, so `/metrics` and the health probes keep
  returning exactly what a scraper and a kubelet expect.

`Vary: Accept` on every response that went through it. Without it a cache in
front of the API can serve a stored HTML page to a consumer that asked for
JSON, and that consumer is AWX.

### Two routes it must not touch

- **`GET /api/v1/endpoints/{id}`** returns whatever the transformer emitted:
  JSON, CSV, or sniffed `text/plain` (`endpoints.rs:209-213`). It is the
  product, not a resource of ours, and a browser asking for an inventory must
  get the inventory. Excluded by route TEMPLATE, not by content-type: `output:
  ansible` emits `application/json` and looked exactly like one of our own
  responses. The endpoints *list* page is ours and still renders.
- **`GET /api/v1/config/{file}`** returns raw YAML. It renders inside a `<pre>`,
  escaped, with no linkification and no tree walk.

Non-2xx keeps the JSON error body under `Accept: application/json` and becomes a
page under `text/html`, with the `401` special-cased into the login form.

## Layer 1: the link graph

A `related` map on the read responses, plus a new index at `GET /api/v1/`.

```json
{
  "id": "src-ssh-dc1",
  "hosts": 412,
  "related": {
    "dataset": "/api/v1/sources/src-ssh-dc1/dataset",
    "groups":  "/api/v1/sources/src-ssh-dc1/groups",
    "hosts":   "/api/v1/sources/src-ssh-dc1/hosts",
    "status":  "/api/v1/sources/src-ssh-dc1/status",
    "scope":   "/api/v1/sources/src-ssh-dc1/scope",
    "sync":    "/api/v1/sources/src-ssh-dc1/sync"
  }
}
```

Rules that keep it from rotting:

- **Not on `/dataset`.** The plain dataset response is served from the entry's
  pre-serialized byte buffer, and its ETag is the hash of exactly those bytes
  (`sources.rs:386`, `sources.rs:411`). Injecting a `related` map there would
  mean re-serializing the hot path per request and invalidating a validator that
  currently survives a restart, to decorate the one response that is pure data
  and is consumed by machines. The dataset stays untouched; `related` lives on
  the metadata responses (`/sources`, `/status`, `/groups`, `/hosts`, `/scope`,
  `/enrichers`, `/endpoints`, `/projects`), which are small, computed per
  request already, and carry no ETag today.
- **Paths, not absolute URLs.** The service does not reliably know its own
  external origin (ingress, port-forward, federation through another instance),
  and a wrong absolute URL is worse than a relative one. The HTML layer resolves
  them against the current origin; `jq` users prepend their own base.
- **Only links the caller may follow.** `related` is filtered by the request's
  `Permissions` (`adapters/in/http/auth.rs:22`). A restricted key browsing a
  source it owns must not be handed a link to one it does not — a 403 on click
  is a worse experience than the absence of the link, and the absence leaks
  less.
- **A view says it is a view.** `CachedSourceInfo` already carries
  `kind: "source" | "view"` (`sources.rs:187`), so the marker exists; what is
  added is the link set. A view gets no `sync` and no host write links, because
  it refuses them (`hosts.rs:38`), and its `members` link points at `/status`,
  which is where `ViewMemberStatus` already lives (`views.rs:36`). There is no
  route that serves a view's *definition* and this does not add one.

Additive as far as the KEYS go: a new key in a JSON object breaks no consumer
that reads by key, which is what ours do. It is not additive at the level of
ROWS — see the gap below, where `/sources` gains entries — and that distinction
is the one that matters to a consumer looping over the list.

## Layer 2: HTML by content negotiation

No template engine. The views are half a dozen pages of structure, and the
project's dependency policy (every entry in `Cargo.toml` justified, several of
them noting they add no code to the build) does not want a rendering framework
for that. Instead, one small module whose type system carries the safety:

```rust
// Html can only be built by escaping, or by an explicit, greppable raw().
pub struct Html(String);

impl Html {
    pub fn text(s: &str) -> Html { /* & < > " ' escaped */ }
    pub fn raw(s: String) -> Html { /* the audit list is `grep raw(` */ }
}
```

Escaping is then not a discipline anyone has to remember: a `&str` cannot reach
the page without going through `text`, because a `&str` is not an `Html`. The
unsafe path exists, is one identifier long, and shows up in a grep. That is the
whole argument for doing it by hand rather than reaching for askama, which buys
the same property at the cost of a build-time template step.

Styling: one inline `<style>` block, no JavaScript, no external assets. A CSP of
`default-src 'none'; style-src 'unsafe-inline'; form-action 'self'` then fits on
one line and forbids everything the pages do not use, including the inline
`<script>` an injected var would have to become.

## Collection flat, detail rich

The rule that makes the size question disappear, taken from AWX:

```
  /api/v1/sources/src-d42/hosts          <- collection: one line per host,
                                            vars flattened to a single-line
                                            preview, truncated, click to open

  /api/v1/sources/src-d42/hosts/web01    <- detail: one host, vars printed as
                                            a real tree, long values collapsed
```

A collection page renders **structure, not content**: the id, a handful of
scalar columns, and a preview string that is cut at a fixed width. It never
pretty-prints nested vars, so its cost per row is constant and a page of 200
rows is a page of 200 rows whatever the hosts contain.

The detail page is the only place that prints a host's vars in full, and it is
the only place that can be large — bounded by one host, which is a size a
browser handles. Values over a threshold render collapsed behind a
`<details>` (an element, not JavaScript).

Paging is the existing `?limit=` / `?offset=` on `/dataset`
(`sources.rs:284-309`), defaulted by the HTML layer to 200 with previous and
next links. No new query parameters and no second paging implementation: the
HTML page is a view of a request the API already serves.

Two details the data shapes force:

- **Sort before rendering.** `Dataset.hostvars` and `Dataset.groups` are
  `HashMap` (`domain/dataset.rs:7-22`), so iteration order changes between
  requests. JSON consumers do not care; a table whose rows reshuffle on every
  refresh is unusable. The HTML layer sorts by key, always. (View responses
  already sort groups — `BTreeMap` — but not hostvars.)
- **`all` and `ungrouped` do not exist.** They are stripped during ingestion as
  Ansible meta-groups (`domain/dataset.rs:32`). The group tree renders what is
  there and does not synthesise a root.

One asymmetry to render honestly: on a source, a `?host=` that matches nothing
is an empty result; on a view, a named host no member claims is a `404`
(`views.rs:93`). The detail page says "no member of this view claims
&lt;host&gt;" rather than a bare not-found, because on a view that sentence
names the actual misconfiguration.

## Authentication: the cookie problem

Swagger's **Authorize** button works because Swagger never navigates. It holds
the key in JavaScript and attaches it as a header to the `fetch` calls it makes
itself. A browsable API is the opposite: every click is a document navigation,
and a document navigation carries no custom headers. This is why AWX has a
login form and a session cookie rather than a header box.

```
  POST /api/v1/login   key=...   ──> validate against ApiKeyRegistry
                                     (the same keys, the same constant-time
                                      compare — no second credential store)
                                 <── Set-Cookie: uapi_session=<opaque>
                                     HttpOnly; Secure; SameSite=Strict; Path=/
```

- The cookie value is a random token from `getrandom` (already in the tree via
  rustls), **never the key itself**: a key in a cookie is a key in a browser
  profile, a backup and every screenshot of devtools.
- Sessions live in a `DashMap<token, (key_name, Permissions, expires_at)>` on
  `AppState`. In memory on purpose: a restart invalidating every session is the
  correct behaviour for an operator console, and it keeps the promise of no
  external data dependency.
- `presented_token` (`auth.rs:169`) gains the cookie as a third accepted
  credential after `X-API-Key` and `Bearer`. Everything downstream is unchanged
  — the middleware still builds an `AuthContext`, handlers still enforce
  `allows_source` per id. **The UI inherits the permission model for free**, and
  a restricted key browsing the UI sees exactly the sources its token allows.
- A reload that removes a key must invalidate its sessions, or the console
  outlives the revocation. The registry replace sweeps the session map by key
  name.

If no keys are configured at all, the API is open (`auth.rs:213`) and so is the
UI, with the same loud warning at startup. No special case.

## Writes: forms and CSRF

The point of the console is to be able to press **sync** — a read-only version
would have been a report. So the detail pages carry forms:

| Page | Action | Route |
|---|---|---|
| source detail | Sync now | `POST /api/v1/sources/{id}/sync` |
| source detail | Evict cache | `DELETE /api/v1/sources/{id}` |
| host detail | Delete host | `DELETE /api/v1/sources/{id}/hosts/{host}` |
| enricher detail | Run now | `POST /api/v1/enrichers/{id}/run` |
| project detail | Sync now | `POST /api/v1/projects/{id}/sync` |

Two consequences, both mandatory:

**CSRF.** A cookie that authenticates on its own means any page on the internet
can make the browser issue that `POST`. `SameSite=Strict` blocks the
cross-origin navigation case, which is most of it, but it is one attribute
between an admin session and a `DELETE`. So: a CSRF token minted with the
session, embedded as a hidden field in every form, and required on any
cookie-authenticated request that is not a `GET`. **Header-authenticated
requests skip the check entirely** — they carry no ambient credential, so there
is nothing to forge, and AWX's API consumers would break instantly otherwise.

**HTML methods.** A browser form does `GET` and `POST` only. The `DELETE`
routes are reached with a `POST` carrying `_method=DELETE`, translated by the
negotiation middleware for cookie-authenticated form posts and nowhere else.
The JSON API keeps its real verbs.

After a successful action: `303 See Other` back to the detail page, so a reload
does not re-fire the sync.

## Escaping: why Swagger is safe and this is not

Worth stating plainly, because the two look equivalent and are not.

Swagger UI puts a response into the DOM with `textContent`. A hostvar
containing `<script>alert(1)</script>` is *displayed* as those characters; the
browser never parses it as markup. Swagger is safe by construction, not by
review.

Server-rendered HTML is the other case. `format!("<td>{}</td>", value)` with the
same var produces a real `<script>` tag in a real page. And the value did not
come from an attacker in the abstract — it came from a Device42 description
field, a VMware annotation, a script's stdout. Someone types angle brackets into
a CMDB note without malice and the next operator who opens that source in a
browser executes it, holding an admin session cookie.

Three controls, all cheap, all listed above:

1. The `Html` newtype: a `&str` cannot become markup without `text()`.
2. Linkify **only** strings matching our own route patterns. Never an arbitrary
   `http://` found in a var, or a hostvar can plant a link to anywhere and a
   `javascript:` URL is one careless regex away.
3. The CSP, as the belt for whatever the first two miss.

Tests that must exist before this ships: a source whose hostvars contain
`<script>`, `"><img onerror=`, and a `javascript:` URL, asserted to render as
inert text on the collection page, the detail page and the generic view.

## New API surface

The tree needs two routes it does not have, and it exposes one gap worth fixing
on its own merits.

- **`GET /api/v1/sources/{id}/hosts/{hostname}`** — a single host with its vars
  and its group memberships. Today that path only answers `PUT` and `DELETE`
  (`routes.rs:48`); reading one host means `/dataset?host=...`, which returns a
  paginated envelope with a one-entry `hostvars` map. The detail page needs a
  resource, and consumers that fetch one host want it too.
- **`GET /api/v1/`** — the index: every collection, plus counts, plus links.

### The gap: a source that never synced is invisible

`GET /api/v1/sources` lists **cache entries**, plus every configured view
(`sources.rs:264-274`). A source that is configured but has never completed a
sync has no cache entry, so it does not appear at all — the only places it
surfaces today are `/readyz`'s `sources_pending` and its own
`/sources/{id}/scope`, which is config-derived and answers `200` for exactly
that case (`scope.rs:72`).

For a console that is backwards. The source you most need to look at is the one
that is not working, and the one that is not working is the one with no cache
entry. `sync_health` exists precisely to record why a source that never synced
failed (`docs/architecture.md`), and the list route cannot show it because it
never lists the source.

So `GET /api/v1/sources` gains every **configured** source, with the
cache-derived fields expressed as absent rather than zero:

```json
{ "source_id": "src-d42", "kind": "source", "cached": false,
  "is_fresh": false, "age_seconds": null, "total_hosts": null,
  "sync_health": { "consecutive_failures": 3, "last_error": "..." } }
```

`age_seconds` and `total_hosts` become `Option`, because `0` is a lie: it reads
as "synced just now, empty" when the truth is "never synced". That is the one
part of this work that is **not** purely additive for a consumer parsing the
field as a number, so it belongs in the same MINOR and in the CHANGELOG under
`**Breaking (anyone reading age_seconds off /sources):**`.

Both new routes are ordinary handlers registered in `openapi.rs` like the rest,
and both are JSON first. The HTML layer never invents a resource the JSON API
does not serve; if a page would need one, the route comes first.

## Configuration

```yaml
server:
  ui:
    enabled: true            # default false
    session_ttl_seconds: 3600
```

Both are **restart-only**: `enabled` decides whether the middlewares and the
login routes are in the router, and the router is built once — the same reason
the bound socket is. `UiConfig` therefore lives on `AppState` beside
`projects_dir`, is a field on `RestartOnlySettings`, and appears in
`changed_keys()` under `server.ui.enabled`, so a reload that flips it reports
`restart_required` rather than silently doing nothing.

Nothing here needs CORS. The UI is served by this router, from this origin, so
`cors_allowed_origins` stays empty and stays the default it is today.

Off by default. A deployment that wants machine-to-machine only should not grow
a login page because it upgraded, and it keeps the surface of a new HTML
renderer opt-in for the first releases.

When `ui.enabled`, `/` redirects to `/api/v1/` instead of `/swagger-ui/`, which
is what someone who opened the address in a browser wanted. Swagger stays where
it is.

## Rollout

Shipped as **one release**, not the three this section originally planned. The
three turned out not to be separable: the action buttons are narrowed by the
`related` map, the help panel and the breadcrumbs both need the spec lookup
that the link graph introduced, and a release where `related` advertises a
`sync` the pages cannot yet perform is a release with a link that 405s. Cutting
it into three would have meant publishing two intermediate states that are
worse than either end.

What bounds the risk instead is `server.ui.enabled`, which is off by default.
An upgrade with the flag unset gets the router it had — the middlewares are not
layered, the login routes are not registered — so the only surface an existing
deployment gains is the JSON half: the index, `related`, the detail route, and
the `/sources` membership change.

## What the implementation taught us

Three things were not in this document before they were found by using it.

**Every path that becomes a link must be checked against the spec.** The
assumption that a route of ours can be visited was wrong in four separate
places — the breadcrumb trail, the values inside the JSON, the theme picker on
an action-result page, and the "too large" page's own way out (whose link
carried a query string the route lookup did not strip). All four produced a
405 from a click the page itself offered. `spec::is_gettable` is the single
answer; nothing may build an `href` without it.

**The route template cannot decide what an object accepts.** A view and a
source share `/api/v1/sources/{id}`, so deriving the action buttons from the
route gave a view a Sync and an Evict button that could only ever answer 400.
The buttons are narrowed by the object's own `related` map, which a view builds
without the write entries. That also means the rule generalises: anything
read-only gets no buttons without a second place remembering why.

**A response that looks like ours may not be ours.** An output endpoint with
`output: ansible` emits `application/json`, so the content-type check rendered
an inventory as a decorated page. Anything sending `Accept: text/html` — a
`wget` with a browser user agent, a client library with a generous default —
would have silently received a page instead of an inventory. The carve-out is
by route template, not by what the body happens to look like.

## What it does not do

- **It is not a SPA.** No bundler, no `node_modules`, no second build stage, no
  duplicated knowledge of the data shapes that drifts. The binary stays one
  binary.
- **It is not a config editor.** `/api/v1/config` stays JSON and admin-only
  (`docs/config-api.md`). A YAML textarea that can break a running deployment on
  a stray indent is a different feature with a different risk profile.
- **It does not replace Swagger**, which documents the contract. This browses
  the data. Both stay.
- **No graphs, no dashboard.** That is what `/metrics` and Grafana are for, and
  a second-rate copy of them inside the API would still be second-rate.
