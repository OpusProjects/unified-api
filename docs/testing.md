# Testing

The whole suite runs with one command and needs no external services — the
integration tests drive real code paths against the sample scripts under
`tests/`.

```bash
cargo test
```

This is one of the CI gates every PR must pass; run it locally before pushing,
together with the other two:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Prerequisites

- **A Rust toolchain** (edition 2024) — `cargo` is all you invoke.
- **Python 3 on `PATH`** — the integration tests spawn the sample scripts under
  `tests/adapters/out/{connectors,enrichers,output}/`, which start with
  `#!/usr/bin/env python3`. No packages are required; they use only the standard
  library. No databases, network access or real sources of truth are involved.

## What runs

`cargo test` compiles and runs both layers:

| Layer | Where | What it covers |
|---|---|---|
| **Unit tests** | `#[cfg(test)]` modules inside `src/` | Focused logic close to the code — config loading (`config.rs`), API-key auth (`adapters/in/http/auth.rs`), TTL/cache-entry rules (`domain/cache_entry.rs`), the DashMap cache (`adapters/out/cache/memory.rs`), secret resolution (`adapters/out/secrets/env.rs`), the SSH connector (`adapters/out/connectors/ssh.rs`) |
| **Integration tests** | `tests/*.rs` | The app assembled through `AppBuilder`, exercised end to end |

The integration files:

| File | Focus |
|---|---|
| `tests/api_integration_test.rs` | HTTP surface: routes, status codes, auth, endpoints driven over axum |
| `tests/connector_test.rs` | The process/SSH connector and output adapters against real scripts |
| `tests/sync_test.rs` | Sync and enrich use cases — scopes, sync modes, TTLs, timeouts |

Integration tests build a real `AppState` via `AppBuilder`, which defaults to the
`MockSecrets` adapter, and point sources at the sample scripts under `tests/`
(including `connectors/slow.py`, used to prove the execution timeout aborts a
hung run).

## Sample scripts

These Python scripts are **not tests** — they are stand-in external programs
(sample connectors, enrichers and outputs) that the Rust tests point the app at,
in place of real sources like Device42 or VMware. Each prints canned,
deterministic JSON so a test has a known-good result to assert against; the
`.rs` files hold the actual test logic. They live under `tests/adapters/out/`,
mirroring the `src/adapters/out/` ports they stand in for:

```
tests/
├── api_integration_test.rs
├── connector_test.rs
├── sync_test.rs
└── adapters/
    └── out/
        ├── connectors/  inventory.py, infra.py, slow.py  # sample source connectors
        ├── enrichers/   enricher.py                      # sample enrichers
        └── output/      ansible_inventory.py             # sample output transformers
```

Cargo compiles every top-level `tests/*.rs` file as its own test binary but does
**not** descend into subdirectories, so these sample folders are invisible to
the test harness — the same mechanism as the conventional `tests/common/`. The
default `config/` and the Docker image point at these same scripts, so they
double as the shipped zero-config demo.

## Running a subset

`cargo test` passes any filter straight through to the test binaries, matching on
test-name substrings:

```bash
cargo test sync_times_out          # one test by name
cargo test sync_                   # every test whose name contains "sync_"
cargo test --test sync_test        # just the tests/sync_test.rs file
cargo test --lib                   # only the in-src unit tests
cargo test -- --nocapture          # let tests print to stdout/stderr
cargo test -- --test-threads=1     # run serially instead of in parallel
```

## Adding tests

- **A new HTTP endpoint** gets an integration test in `tests/` — see the
  checklist in [CONTRIBUTING.md](../CONTRIBUTING.md#adding-an-http-endpoint).
- **A new connector/enricher/output contract** is best exercised the way the
  suite already does it: add a small sample script under the matching
  `tests/adapters/out/` folder (`connectors/`, `enrichers/` or `output/`) and
  wire it into a test, following the existing patterns. See
  [connectors.md](connectors.md) for the script contracts.
- **Pure logic** (domain rules, config parsing) belongs in a `#[cfg(test)]`
  module next to the code, matching the unit tests already in `src/`.

Keep tests free of real infrastructure: everything the suite needs is a sample
script and an in-memory cache, so `cargo test` stays fast and hermetic.
