# Releasing

Releases are cut by [release-please](https://github.com/googleapis/release-please) in GitHub Actions: no version number,
changelog line or tag is ever typed by hand, and nothing is built or published from a laptop.

- [How a release happens](#how-a-release-happens)
- [Versions](#versions)
- [What goes in the CHANGELOG](#what-goes-in-the-changelog)
- [Changing the wording before a release](#changing-the-wording-before-a-release)
- [Correcting a merged title](#correcting-a-merged-title)
- [Forcing a specific version](#forcing-a-specific-version)
- [When the release PR has no checks](#when-the-release-pr-has-no-checks)

---

## How a release happens

Every merge into `main` updates one open pull request, and merging that pull request is the release.

```
feature PR ──squash──▶ main ──▶ release-please updates the open "chore: release vX.Y.Z" PR
                                              │
                           you merge it ──────┘
                                              ▼
                     image X.Y.Z, <sha>, latest ──▶ then tag vX.Y.Z + GitHub Release
```

1. Every pull request merged into `main` has a Conventional Commit title
   (`feat: ...`, `fix: ...`); see [CONTRIBUTING.md](../CONTRIBUTING.md).
2. After each merge, the **Release** workflow (`.github/workflows/release.yml`)
   runs release-please. It keeps **one** pull request open, titled
   `chore: release vX.Y.Z`, that bumps `Cargo.toml` and `Cargo.lock`, plus the sample project's
   `branch:` tag in `config/projects/prj-connectors-infra.yaml`, adds the
   section for that version at the top of `CHANGELOG.md`, and updates
   `.release-please-manifest.json`.
3. That open pull request is the "unreleased" list: its diff shows exactly what
   the next release contains. It is opened by `github-actions[bot]`, and the
   same workflow then runs CI on it (see [below](#when-the-release-pr-has-no-checks)).
4. **Merging it is the release.** Squash-merge it once its checks are green; the
   workflow then, in this order:
   - creates the GitHub Release titled `vX.Y.Z`, with the CHANGELOG section as its notes,
     as a **draft**: nobody sees it, and it has no tag yet;
   - builds the image from the release commit and publishes
     `ghcr.io/opusprojects/unified-api` as `X.Y.Z`, the commit SHA and `latest`;
   - publishes the draft, which creates the tag `vX.Y.Z` and makes it the latest release.

   So a tag or a release always has its image. If the image cannot be built, the release
   stays a draft; fix the cause and re-run the failed **Publish image, then the release**
   job, which builds again and publishes the draft. Never publish the draft by hand
   before the image exists.

Never create a tag or a GitHub Release by hand: the next release PR would not
know about it, and a hand-made release is authored by a person rather than the
bot, which cannot be corrected later without deleting it.

---

## Versions

Versions follow [Semantic Versioning](https://semver.org/) with a `v` on the tag, and the project is pre-1.0.

| Merged title | Next version | Example |
|---|---|---|
| `fix:` or `perf:` | PATCH | `0.6.0` → `0.6.1` |
| `feat:` | MINOR | `0.6.0` → `0.7.0` |
| `feat!:`, `fix!:`, or a `BREAKING CHANGE:` footer | MINOR while pre-1.0 | `0.6.0` → `0.7.0` |
| `refactor:`, `docs:`, `test:`, `build:`, `ci:`, `chore:` | no release on its own | — |

A pull request with only hidden types merged since the last release opens no
release PR; it ships with the next `feat:` or `fix:`.

---

## What goes in the CHANGELOG

The CHANGELOG is written from the squashed titles, so a pull request's title is its changelog line.

| Type | CHANGELOG section |
|---|---|
| `feat` | Added |
| `fix` | Fixed |
| `perf` | Changed |
| `revert` | Reverted |
| a `!` or `BREAKING CHANGE:` | ⚠ BREAKING CHANGES |
| `refactor`, `docs`, `test`, `build`, `ci`, `chore`, `style` | not listed |

Write the title for the person reading the release notes: what changes for them,
not how the code got there. `fix: a sync that times out keeps the last good
data` is a changelog line; `fix: review findings` is not.

---

## Changing the wording before a release

The release PR's CHANGELOG section can be edited like any other file before it is merged.

Edit `CHANGELOG.md` on its branch (`release-please--branches--main--components--unified-api`) and push.
That edit is lost when release-please rebuilds the branch after the next merge
into `main`, so make it just before merging the release. Once the release exists,
its GitHub Release notes can also be edited directly.

---

## Correcting a merged title

A merged pull request with the wrong title is corrected at its source, in its description.

A `feat:` that should have been a `fix:` (which would bump MINOR instead of
PATCH), or a title that reads badly in the CHANGELOG, is fixed by adding this to
the merged pull request's description; the next run of the Release workflow
reads it instead of the title:

```
BEGIN_COMMIT_OVERRIDE
fix: the title it should have had
END_COMMIT_OVERRIDE
```

Re-run the Release workflow from the Actions tab to apply it without waiting for the next merge.

---

## Forcing a specific version

A version other than the computed one, such as `1.0.0`, is set in `release-please-config.json`.

Set `"release-as"` at the top level in a pull request with a `feat:` or `fix:`
title and merge it; release-please then proposes exactly that version. Remove
the setting again in the next pull request after the release, or every later
release is proposed as that same version.

```json
  "release-type": "rust",
  "release-as": "1.0.0",
```

---

## When the release PR's checks wait for approval

This repository asks for approval before running workflows for a first-time contributor, and
`github-actions[bot]` is one until a pull request of its has been merged here. On a release PR
that is a CI run marked **Approval required**, which also hides the checks the dispatched run
reported, so the PR cannot be merged. Approve it once (the PR's Checks tab → **Approve and run
workflows**, or `gh api -X POST repos/OpusProjects/unified-api/actions/runs/<run id>/approve`);
from the first merged release PR on, it no longer happens.

---

## When the release PR has no checks

The release PR is pushed with `GITHUB_TOKEN`, and GitHub starts no workflow from such a push.

`release.yml` therefore dispatches the CI workflow (`build.yaml`) on the release
branch right after release-please updates it, and those runs report on the PR's
head commit, which is what branch protection requires. If the checks are ever
missing, open Actions → the CI workflow → **Run workflow** on the
`release-please--branches--main--components--unified-api` branch, or run:

```bash
gh workflow run build.yaml --ref release-please--branches--main--components--unified-api
```
