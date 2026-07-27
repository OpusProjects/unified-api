#!/usr/bin/env bash
#
# Fail if a pull request edits an already-released CHANGELOG section.
#
# Everything from the first "## [x.y.z]" heading downwards is published
# history: those entries describe versions that are tagged, released and
# pulled by consumers, so a change there rewrites the record of something
# already shipped.
#
# This exists because the mistake is silent. Rebasing a feature branch across
# a release renames "## [Unreleased]" to "## [x.y.z]", so git happily applies
# the branch's bullet into whatever section now occupies those lines — the
# freshly released one. No conflict, no warning, and the next release ships
# with empty notes while an old one grows an entry it never contained.
#
# Release PRs legitimately change this region (renaming Unreleased to the new
# version, adding its link ref), so the workflow skips this check on
# release/* branches.
#
# Usage: check-changelog.sh [base-ref]      (default: origin/main)

set -euo pipefail

base="${1:-origin/main}"

# The published part of the file: from the first versioned heading to the end.
# "## [Unreleased]" is deliberately excluded — that is the part a PR may edit.
frozen() {
    awk '/^## \[[0-9]/ { seen = 1 } seen'
}

if ! diff -u \
    --label "$base:CHANGELOG.md (released sections)" \
    --label "HEAD:CHANGELOG.md (released sections)" \
    <(git show "$base:CHANGELOG.md" | frozen) \
    <(frozen <CHANGELOG.md); then
    cat >&2 <<'MSG'

error: this PR modifies an already-released CHANGELOG section.

Released sections are a record of what shipped; only "## [Unreleased]" is
open for editing. If a rebase moved your entry, cut it from the released
section and paste it under "## [Unreleased]".

(Cutting a release is the exception — that work belongs on a release/*
branch, where this check does not run.)
MSG
    exit 1
fi

echo "CHANGELOG: released sections unchanged"
