#!/usr/bin/env bash
# Regenerate per-release golden DuckDB fixtures under testdata/golden-dbs/.
#
# Default behavior (no tag arg): print a friendly note and exit 0. The
# migration test suite at server/ts-storage-duckdb/tests/migrations.rs
# uses **code-defined synthetic fixtures** rather than binary blobs as
# of v0.3.0, so regenerating fixtures is opt-in.
#
# Usage:
#   scripts/testdata/regenerate-golden-dbs.sh                   # status only
#   scripts/testdata/regenerate-golden-dbs.sh <tag>             # build + seed
#   scripts/testdata/regenerate-golden-dbs.sh <tag1> <tag2>...  # batch
#
# Effect: for each <tag>, builds the binary at that ref in a temporary
# worktree, runs it against a canonical seed dataset, and writes
#   testdata/golden-dbs/<tag>.duckdb
#
# NOTE: the seed-driver invocation below assumes a `--seed-golden-db
# <path>` CLI flag on the binary, which does NOT exist as of v0.3.0.
# Implement it (and remove this NOTE) before the first time you try to
# regenerate a fixture. The pattern is in the migration test file —
# the canonical row counts there define the contract.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="$REPO_ROOT/testdata/golden-dbs"

if [ $# -eq 0 ]; then
    cat <<'NOTE'
No tag specified.

Migration tests currently use code-defined synthetic fixtures, not
binary .duckdb files (see testdata/golden-dbs/README.md). Run this
script with one or more release tags only when a future migration
needs an opaque on-disk state that DDL alone cannot reproduce.

Example:
    scripts/testdata/regenerate-golden-dbs.sh v0.2.0
NOTE
    exit 0
fi

mkdir -p "$OUT_DIR"

for TAG in "$@"; do
    echo "==> Regenerating fixture for $TAG"
    WORKTREE="$(mktemp -d -t "ts-golden-${TAG}-XXXXXX")"
    trap 'rm -rf "$WORKTREE"' EXIT

    git -C "$REPO_ROOT" worktree add --detach "$WORKTREE" "$TAG"

    (
        cd "$WORKTREE/server"
        cargo build --release -p tokenscope
        # Bin name was `tokenscope` before 0.3.0 (PR#59 rebrand to `heron`);
        # the worktree's binary name is the one shipped by that tag.
        BIN="$(ls "$WORKTREE/server/target/release/" | grep -E '^(tokenscope|heron)$' | head -1)"
        if [ -z "$BIN" ]; then
            echo "Cannot locate built binary for $TAG" >&2
            exit 1
        fi
        "$WORKTREE/server/target/release/$BIN" \
            --seed-golden-db "$OUT_DIR/$TAG.duckdb"
    )

    git -C "$REPO_ROOT" worktree remove --force "$WORKTREE"
    trap - EXIT
    echo "==> Wrote $OUT_DIR/$TAG.duckdb"
done
