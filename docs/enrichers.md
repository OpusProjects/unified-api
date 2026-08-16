# Enrichers

Post-processors over data already in the cache. An enricher never gathers —
it refines what a connector brought: resolve DNS, probe reachability, copy a
storage mapping onto compute hosts, tag or drop entries. This page is the full
treatment; the script I/O contract also appears in [connectors](connectors.md#enrichers).

- [Two modes](#two-modes)
- [The script contract](#the-script-contract)
- [When enrichers run](#when-enrichers-run)
- [Freshness semantics](#freshness-semantics)
- [Health and observability](#health-and-observability)
- [Routes and permissions](#routes-and-permissions)

---

## Two modes

Both modes write into the cache entry of their `target_id`; they differ in
where the new values come from and how much code you owe.

**Script-based** — a process receives the target's current dataset on stdin
and prints a *partial* dataset of changes. Use it when the enrichment needs
logic or the outside world (a DNS lookup, an HTTP probe):

```yaml
enrich-reachability:
  name: "Probe reachability"
  target_id: "src-fleet-facts"
  script_path: "enrichers/probe.py"
  project_id: "prj-connectors"      # optional: resolve the script (and its
                                    # virtualenv) inside this checkout
  script_args: ["--timeout", "5"]
  sync_interval_seconds: 900
  timeout_seconds: 300
```

**Declarative merge** — no script at all: copy the named top-level hostvars
`fields` from one cached source onto the hosts of another, for every host
present in both. Use it for the common "join two sources by hostname" case:

```yaml
enrich-storage:
  name: "Attach storage volumes"
  target_id: "src-d42"              # who receives the fields
  source_id: "src-purestorage"      # who provides them
  fields: ["volumes", "array"]
  sync_interval_seconds: 900
```

A declarative enricher writes **only** the fields it names — never the rest of
the host's map — so two of them on one target cannot erase each other's keys.
Config validation requires one mode or the other, and refuses a view as a
target: a view has no cache entry to write into; enrich the member instead.

---

## The script contract

The script is spawned with a scrubbed environment (the same policy as
connectors — no API keys, no other sources' credentials) and bounded by
`timeout_seconds` (default 300; exceeding it kills the process).

| Channel | Content |
|---|---|
| stdin | The target's **current dataset** as JSON (`hostvars` + `groups`) |
| CLI arguments | `script_args`, verbatim — no shell |
| `SOURCE_CONFIG` env | The enricher's `config` map as JSON |
| stdout | A *partial* Dataset: only what changed |

The partial output merges as follows: `hostvars` entries replace that host's
map, `groups` entries replace their counterparts, and `remove_hosts` deletes
hostnames (from groups too). The merge is atomic, and hosts the script does
not mention are untouched — concurrent writes to them are never lost.

> **Return each host's full variable map, not only your own keys.** Because a
> returned `hostvars` entry *replaces* the map, any key you omit is dropped —
> including keys another enricher owns. The dataset arrives on stdin precisely
> so you can carry the rest through. See
> [troubleshooting](troubleshooting.md#a-script-enrichers-keys-keep-disappearing).

With `project_id` set, the script path resolves inside that project's checkout
at every execution, and if the project has a [virtualenv](configuration.md#projectsyaml)
its `bin/` leads the script's PATH — pip-installed imports work.

---

## When enrichers run

Three triggers, all funneling through the same use case so behavior cannot
diverge between them.

1. **On a schedule** — `sync_interval_seconds`, or a cron `schedule` (UTC,
   exact times, no jitter), runs a background task with the scheduler's full
   failure machinery: startup jitter for intervals, exponential backoff on
   failure (occurrences pass 1, 2, 4, up to 8 apart), panic supervision, and
   shutdown draining. The two cadences are mutually exclusive per enricher.
2. **On demand** — `POST /api/v1/enrichers/{id}/run` answers with the outcome
   (hosts updated/removed, duration, error).
3. **After every sync of the target** — a `replace`-mode sync overwrites what
   enrichers had added, so every sync re-applies its target's enrichers
   before it returns. The interval remains the backstop for write paths that
   bypass a sync (host PUT, snapshot reload).

When several enrichers share a target they run **sorted by id**, one after
another. Additive merges cannot lose each other's keys, but if two claim the
*same* key on the same host, the winner is the stable id order — a documented
rule rather than whichever task finished last.

---

## Freshness semantics

Enrichment must never disguise stale data as fresh, because on-demand refresh
trusts the per-host timestamps to decide what needs re-gathering.

A host's timestamp records when it was last *collected* by a connector. An
enricher touching a host leaves that timestamp alone — enriching a stale host
leaves it stale, and a read asking for a refresh still gets one. Only a host
the enricher *introduces* is stamped now, because that is when it became
known. Returning hosts is therefore not a way to suppress refreshes.

---

## Health and observability

A permanently failing enricher used to be a `warn!` line per interval; every
run now lands in the enricher health registry, whoever triggered it.

- `GET /api/v1/enrichers` lists each enricher with `target_ready` (is the
  target cached at all) and a `sync_health` block — last attempt, last
  success, last error, consecutive failures — once it has run in this process.
- **A target missing from the cache counts as a failure**: an enricher whose
  target never syncs is exactly as broken as one whose script crashes, and it
  backs off the same way.
- `/metrics` carries `unified_api_enrich_total{source, result}` and a duration
  histogram per run, plus scrape-time gauges
  `unified_api_enricher_consecutive_failures{enricher}` and
  `unified_api_enricher_last_success_age_seconds{enricher}` — see
  [observability](observability.md).

---

## Routes and permissions

An enricher writes into its target's cache entry, so the permission that
governs is the **target's** — there is no separate enricher grant to manage.

| Route | Meaning |
|---|---|
| `GET /api/v1/enrichers` | List (filtered to targets the key may read), with readiness and health |
| `POST /api/v1/enrichers/{id}/run` | Run now; `404` for an unknown id **or** an uncached target — the body names which |

Configuration field reference lives in
[configuration → enrichers.yaml](configuration.md#enrichersyaml).
