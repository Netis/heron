# Golden DuckDB fixtures

Per-release `.duckdb` files used to lock in schema-migration behavior:
load a frozen historical DB → run current `DuckDbBackend::init()` →
assert auto-migration succeeds and downstream queries return expected
rows. Catches the "PR#48 class" of bugs where a future refactor breaks
the upgrade path without breaking any fresh-install test.

## Current status

This directory is intentionally **empty as of v0.3.0**. The migration
test suite at `server/ts-storage-duckdb/tests/migrations.rs` uses
**code-defined synthetic fixtures** (legacy DDL embedded as Rust
constants) rather than binary `.duckdb` files. That approach:

- Ships zero binary blobs in the repo
- Stays deterministic across DuckDB version bumps (we control the SQL)
- Removes drift risk between fixture and reality
- Runs hermetically in CI with no fixture-rebuild step

Add a binary fixture here **only when the synthetic approach proves
insufficient** — for example, when a future migration depends on
opaque on-disk encoding that can't be recreated by re-issuing DDL
(WAL state, internal page layout, etc.).

## How to add a binary fixture (when needed)

1. Decide the release tag to capture (e.g., `v0.2.0`).
2. From a clean checkout of that tag, build and run the binary with
   the `regenerate-golden-dbs.sh` script:

   ```sh
   ./scripts/testdata/regenerate-golden-dbs.sh v0.2.0
   ```

   The script checks out the tag in a worktree, builds, runs against
   a canonical seed dataset, and writes
   `testdata/golden-dbs/v0.2.0.duckdb` here.

3. Add a corresponding test in
   `server/ts-storage-duckdb/tests/migrations.rs` that loads
   `testdata/golden-dbs/v0.2.0.duckdb`, runs current `init()`, and
   asserts post-migration invariants.

## Naming

`v<release-tag>.duckdb`, one file per release tag captured. Per-minor
or per-major is fine too, decided at point of need — but the test
file paths must match exactly.

## Size budget

Fixtures should stay under ~100 KB each (canonical seed dataset:
10 calls, 5 turns, 50 metrics rows). If the file is larger, the seed
script is generating more data than necessary.
