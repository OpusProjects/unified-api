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

Commit and PR titles follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):
release-please reads them to pick the next version and to write the CHANGELOG.

- Title: `<type>: short imperative summary`, lowercase after the colon —
  `feat: add SSH connector with parallel execution`, `fix: lost updates in
  cache`. Types in use: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`,
  `build`, `chore`.
- A breaking change adds `!` after the type (`feat!: ...`) or a
  `BREAKING CHANGE:` footer in the body.
- The squashed title is the CHANGELOG line and decides the next version, so
  write it for someone reading the release notes: what changes for them. See
  [docs/releasing.md](docs/releasing.md).
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
`domain/` stays pure: `std` + `serde` only, plus the types it shares with unified-gitops from
[auth](https://github.com/OpusProjects/auth) (`Credential`, `LdapConfig`).

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

Releases are cut by release-please; no one bumps a version, edits the CHANGELOG
or pushes a tag by hand. Every merge into `main` updates one open
`chore: release vX.Y.Z` pull request, and merging it creates the tag, the GitHub
Release and the `ghcr.io/opusprojects/unified-api:X.Y.Z` image. Only releases publish
images: `latest` always means the newest release, never the tip of `main` —
consumers should still pin the version tag. See
[docs/releasing.md](docs/releasing.md).

## Running the test suite

```bash
cargo test                # unit + integration; integration tests use the fixtures under tests/
```

The sample scripts under `tests/adapters/out/` (`connectors/`, `enrichers/`,
`output/`) are plain Python — no external services are needed to run the full
suite. See [docs/testing.md](docs/testing.md) for what the suite covers, running
a subset, and where new tests belong.
