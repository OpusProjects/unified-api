# Projects

Git repositories that carry the code the app executes: connector scripts,
enrichers, output transformers, static inventories. A **project** is a
checkout the app clones, keeps up to date, and resolves script paths into —
so rolling a new script version is a `git push`, not an image rebuild. Field
reference lives in [configuration → projects.yaml](configuration.md#projectsyaml).

- [The lifecycle](#the-lifecycle)
- [Boot never blocks on git](#boot-never-blocks-on-git)
- [How script paths resolve](#how-script-paths-resolve)
- [Python virtualenvs](#python-virtualenvs)
- [Keeping checkouts up to date](#keeping-checkouts-up-to-date)
- [Private repositories](#private-repositories)
- [Health and routes](#health-and-routes)

---

## The lifecycle

One directory per project id under `projects.dir` (default `./projects`),
created by a shallow clone and updated by fetch + hard reset — local drift in
a checkout is deliberately discarded, because the repository is the truth.

Three sync styles compose from two knobs:

| Style | Config | Behavior |
|---|---|---|
| Automatic | `sync_interval_seconds: N` | update at boot + every N seconds |
| Boot only | interval 0/absent | clone/update at boot, then frozen |
| Manual / pipeline-driven | `sync_on_boot: false`, interval 0/absent | an existing checkout is used **as-is**; updates only via `POST /api/v1/projects/{id}/sync` |

With `sync_on_boot: false` a *missing* checkout is still cloned — without the
scripts there is nothing to execute — so first bring-up needs no special
casing. For the manual style, keep `projects.dir` on a persistent volume: the
checkout IS the durable state.

Every git operation is bounded by the project's `timeout_seconds` (default
300); a timed-out clone or pull kills the git child rather than abandoning
it, fails the sync with a clear error, and lands in the project's health.

---

## Boot never blocks on git

The listener binds and serves **before** the project clones, so one
unreachable remote can delay that project's scripts — never `/healthz` or a
Kubernetes startup probe.

The clones run concurrently in a background task (boot waits for the slowest,
not the sum, each bounded by its `timeout_seconds`), and the source sync
schedulers start only after they have had their bounded chance, so a source's
first sync does not race its own script's clone. `/readyz` stays red until
the first source has synced, exactly as without projects.

---

## How script paths resolve

A *relative* `script_path` whose file exists inside the checkout runs from
there (`<projects.dir>/<project_id>/<script_path>`); otherwise the configured
path is kept as-is, so image-baked and mounted scripts keep working.

Resolution happens at **every execution**, not once at boot: a script that
first appears after startup — a slow clone, a pipeline's first push to a new
project — is used on the very next run, no restart. A checkout that exists
but lacks the named file logs a warning and keeps the configured path. SSH
sources are never resolved; their `script_path` is a remote command. Sources
always declare `project_id`; enrichers and endpoints may add an optional one
to resolve their scripts the same way.

---

## Python virtualenvs

With `python_venv: true`, a `requirements.txt` in the checkout gets a real
virtualenv the app builds and maintains — the escape from "one PyPI package
means a derived image".

- Built after the clone and refreshed after any pull whose `requirements.txt`
  changed; an unchanged pull costs two file reads, not a pip run.
- Stored **outside** the checkout (`<projects.dir>/.venvs/<project>`), where
  the hard reset cannot wipe it.
- When this project's scripts run, the venv's `bin/` leads their PATH — a
  `#!/usr/bin/env python3` shebang resolves to the venv's interpreter, and
  non-Python scripts in the same project are untouched.
- A failing install (typo'd package, unreachable index) **fails the project
  sync**, visible in its `sync_health` and bounded by `timeout_seconds` —
  instead of surfacing later as one import error per source.

---

## Keeping checkouts up to date

Scripts are read from disk on every execution and paths resolve per run, so
an updated checkout takes effect on the next sync/enrich/endpoint run — no
restart, ever.

The periodic pull (`sync_interval_seconds`, or a cron `schedule` in UTC —
mutually exclusive; the boot clone happens either way, cron only paces the
re-pulls) runs with the scheduler's full failure machinery: startup jitter
for intervals, exponential backoff on failure, panic supervision, shutdown
draining. The on-demand route is how a pipeline in the
scripts repository rolls a new version deliberately:

```bash
curl -s -H "x-api-key: $ADMIN_KEY" -X POST $BASE/api/v1/projects/prj-connectors/sync
```

---

## Private repositories

A `credential_id` on the project authenticates the clone and every pull —
without ever putting the secret on a command line (argv is world-readable in
`/proc` while git runs).

A `token` (or username/password) credential becomes a Basic-auth header passed
through `GIT_CONFIG_*` environment variables; an `ssh_key` credential sets
`GIT_SSH_COMMAND` pointing at the key file. See [secrets](configuration.md#credentialsyaml)
for how credentials resolve.

---

## Health and routes

A checkout stuck on a stale commit looks healthy from the outside —
`checkout_present` stays true while every pull fails — which is exactly what
the health block exists to expose.

| Route | Meaning |
|---|---|
| `GET /api/v1/projects` | Configured projects with `checkout_present`, sync settings and `sync_health` (admin-only) |
| `POST /api/v1/projects/{id}/sync` | Clone/update to the branch tip now; `502` with the git error on failure (admin-only) |

Every sync — boot, interval, on demand — records last attempt / last success
/ last error / consecutive failures per project, and `/metrics` exposes
`unified_api_project_sync_consecutive_failures{project}` and
`unified_api_project_sync_last_success_age_seconds{project}` at scrape time —
see [observability](observability.md).
