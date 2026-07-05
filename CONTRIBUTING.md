# Contributing to Unified API

Thanks for helping out! This is a small project with a few firm conventions —
following them keeps the history clean and the architecture intact.

## Workflow: one logical change per PR

`main` moves only through squash-merged pull requests, one **logical change** per PR —
a fix, a dependency bump, a doc edit each get their own. This keeps every change
independently reviewable, bisectable, and revertable.

```bash
git checkout main && git pull
git checkout -b <type>/<short-name>       # e.g. fix/atomic-cache-updates
# ...make exactly one change, test it...
git commit -am "Short imperative title"
git push -u origin <type>/<short-name>
gh pr create --base main
```

### Commit / PR message style

- Title: short, imperative, sentence case, no prefixes — `Add SSH connector with
  parallel execution`, `Fix lost updates in cache`, not `feat: ...`
- Body (when the change needs explaining): plain prose, *what* and *why*, wrapped
  at ~72 columns. The problem first, then the change.

## CI gates

Every PR must pass, in this order (run them locally before pushing):

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Architecture rules

The layout is hexagonal — see [docs/architecture.md](docs/architecture.md). The two
rules that matter when writing code:

1. **Use-case logic lives in `src/application/` only.** HTTP handlers and the
   scheduler are thin translators; if you find yourself orchestrating
   connector-cache-secrets work in a handler, move it to an application function.
2. **Cache mutations must use the atomic `CachePort::update` / `merge_or_insert`
   operations.** Never `get` → modify → `set`: the cache returns clones, and that
   pattern silently loses concurrent writes.

Dependency direction is `adapters → application → ports → domain`, never the reverse.
`domain/` stays pure: `std` + `serde` only.

## Code comments

The codebase carries **teaching comments** that explain Rust concepts to the
maintainers. They are intentional — do not strip them when refactoring; move them
with the code they explain. When you add a non-obvious Rust construct, a comment in
the same spirit is welcome. Beyond those, comment only when the *why* is non-obvious.

## Adding an HTTP endpoint

1. Handler in the matching file under `src/adapters/in/http/` (one file per resource)
2. Route in `src/adapters/in/http/routes.rs`
3. Register the handler in `paths(...)` — and any response structs in
   `components(schemas(...))` — in `src/adapters/in/http/openapi.rs`, or it won't
   appear in Swagger
4. Integration test in `tests/`

## Releases

1. One release PR: bump `version` in `Cargo.toml` (run `cargo build` so
   `Cargo.lock` follows) and move the `Unreleased` entries in
   [CHANGELOG.md](CHANGELOG.md) under the new version with the date.
2. After the squash merge: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. CI runs the gates, publishes `ghcr.io/opusprojects/unified-api:X.Y.Z`, and
   creates a GitHub Release with that version's CHANGELOG section as the notes.
   `latest` keeps tracking `main`; consumers should pin the version tag.

## Running the test suite

```bash
cargo test                # unit + integration; integration tests use the fixtures under tests/
```

The sample scripts under `tests/adapters/out/` (`connectors/`, `enrichers/`,
`output/`) are plain Python — no external services are needed to run the full
suite.
