# Web UI

The image serves a web UI on the same address as the API: open the instance's
address in a browser, sign in, and see its sources, endpoints, projects,
credentials, federation topology, metrics and configuration, with a host
search across all of it.

The UI is its own project, [unified-ui](https://github.com/OpusProjects/unified-ui),
with its own versions. Each unified-api release builds the unified-ui version
it was tested with into its image (`UI_REF` in `docker/Dockerfile`), and serves
the built files itself: no proxy in front, no second container.

## Turning it on

```yaml
# config.yaml
server:
  ui:
    enabled: true
    assets_dir: "/usr/share/unified-api/ui"   # where the image puts the build
```

The bundled `config/config.yaml` already has this block, so the image serves
the UI out of the box. A deployment with its own configuration adds it.

- `assets_dir` needs `enabled: true`: the UI signs in through the API's login
  page and its session cookie, which exist only with it. Validation refuses
  the one without the other.
- `assets_dir` is restart-only: the routes are built once at startup. A
  reload that changes it says so in `restart_required`.
- A directory with no `index.html` is logged as a warning at startup; the API
  works as before and only the UI is missing.

## What is served where

| Path | With a UI | Without |
|---|---|---|
| `/` | the UI | redirect to `/swagger-ui/` |
| `/sources`, `/topology`, … (any page of the UI) | the UI | 404 |
| `/assets/*` | the UI's files, cached for good (their names change with their content) | 404 |
| `/metrics` | the UI's Metrics page for a browser (`Accept: text/html`); the Prometheus text for everyone else | the Prometheus text |
| `/api/...`, `/swagger-ui/`, `/api-docs/` | unchanged; an unknown API path is still a JSON 404, never the UI | unchanged |

The UI's own files are public: they hold no data. Every call the UI makes for
data goes through the same authentication as any other client.

## Signing in

The UI has no login of its own. A call without a session gets a 401, and the
UI sends the browser to the API's login page (`/api/v1/login`, an API key or
LDAP: see [ldap-login.md](ldap-login.md)), with `?next=` set to the page it
was on. After signing in the browser returns there.

A login with no `next` (someone opening `/api/v1/login` directly) lands on
`server.ui.home`, which defaults to `/` when `assets_dir` is set and to the
API's own pages (`/api/v1/`) when it is not. Signing out from the UI returns to
the login page with `next=/`, so signing straight back in returns to the UI.

What a person can see and do is what their key, or their LDAP access rule,
allows: a restricted key sees only its sources and endpoints, and the
admin-only pages (projects, credentials, configuration) are left out of the
menu.

## Running the UI from its own repository

For working on the UI, unified-ui runs it on Vite against any unified-api
(`npm run dev`), and has a demo image with nginx in front of the API. Both are
described in its README; neither is needed to use the UI.
