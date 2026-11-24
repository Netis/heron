# verification

Machine-readable artifacts for the risk-driven coverage policy. See
[`docs/design/11-verification.md`](../docs/design/11-verification.md) for the
rationale and the tier model.

| File | Role |
|---|---|
| `coverage-policy.json` | SSOT: crate → tier, floors, changed-code thresholds, exclusions |
| `coverage-baseline.json` | Committed per-crate measurement; the no-decrease ratchet |
| `scenarios.json` | Tier 0/1 behavior matrix |
| `waivers.json` | Expiring, owner-tagged exceptions |

All JSON (not YAML) so the CI checkers stay stdlib-only — no PyYAML/tomllib.

## Local commands

```bash
just coverage all                 # instrument, run tests, floors, changed-code gate
just coverage rust                # just the Rust instrumentation (slow)
just coverage ts                  # console coverage via bun
just coverage report              # floors vs baseline
just coverage diff origin/main    # changed-code gate only
just mutation diff origin/main    # Tier 0 mutation on the current diff
```

Equivalently, `bash scripts/ci/coverage.sh …`.

## Adding a scenario

1. Add an object to `scenarios.json` with `id`, `tier`, `priority`, `module`,
   `requirement`, `invariant`, `verification` (any of
   `unit`/`integration`/`e2e`/`fault`).
2. Tag the test(s) that verify it:

   ```rust
   // @scenario MY-SCENARIO-001 integration
   #[tokio::test]
   async fn the_thing_stays_true() { ... }
   ```

3. `python3 scripts/ci/check_scenarios.py` must pass. A P0 must declare at
   least one behavior-level kind (`integration`/`e2e`/`fault`), not unit only.

## Adding a waiver

A waiver is not a permanent exclusion — it needs a reason, an owner, an expiry
and a tracking pointer, and CI fails once it expires:

```json
{
  "module": "h-storage-clickhouse",
  "metric": "scenario",
  "reason": "needs a live ClickHouse instance; staging covers it",
  "owner": "platform",
  "expires": "2026-12-31",
  "tracking": "#<issue>"
}
```

For a scenario gap use `"scenario": "<ID>"` with `"metric": "scenario"`; for a
tier floor use `"module"` + `"metric": "line"|"region"|"function"`, or
`"metric": "tier"` to excuse every floor metric for that crate at once (the
form used for crates still ratcheting — one reason + expiry per crate, not one
entry per metric). `coverage.yml` runs with `--enforce-tiers`, so a crate at or
above its floor cannot regress below it and a below-floor crate needs an
unexpired waiver.

## Reading a failure

- **`unclassified workspace crate`** — add the crate to `coverage-policy.json`.
- **`coverage dropped`** — the PR removed more coverage than `slack` allows;
  either restore tests or (with justification) update the baseline deliberately.
- **`changed-code coverage: FAIL`** — the PR's new lines are under-tested.
- **`no verified test for kind …`** — a scenario is missing a tagged test, or a
  waiver has expired.

## Regenerating the baseline

Only after a deliberate, reviewed coverage change:

```bash
bash scripts/ci/coverage.sh rust
python3 scripts/ci/check_coverage_policy.py \
    --write-baseline server/target/coverage/llvm-cov.json
```

## Enabling the gates (one-time / ops)

Splitting "built" from "enforcing" — these steps are outside the repo:

- **`ci.yml` static gates** (policy classification, waiver expiry, scenario
  completeness, checker self-tests) run on every PR with no setup.
- **`coverage.yml`** must be added to branch protection as a **required**
  status check (GitHub → Settings → Branches → `main` → Require status checks →
  add `rust` and `console`, or collapse them into one aggregate job). Nothing in
  the repo can turn a required check on.
- **`mutation.yml`** is scheduled. Promote `cargo-mutants --in-diff` to a PR
  check only after its false-positive rate is measured on real diffs (start
  `continue-on-error`).
- **Baseline ratchet.** The no-decrease check compares against the committed
  `coverage-baseline.json`. Until a bot job refreshes it on `main`, refresh it
  in the same PR that deliberately changes coverage:

  ```bash
  bash scripts/ci/coverage.sh rust
  python3 scripts/ci/check_coverage_policy.py --write-baseline server/target/coverage/llvm-cov.json
  ```

- **Flake note.** One instrumented `coverage.yml`-style run failed once and
  passed on retry; it did not reproduce across 13 subsequent instrumented
  workspace runs (~34 s each). The known wall-clock-sensitive tests
  (`h-storage/buffer.rs` interval flushes, `app/heron/tests/signal_shutdown.rs`)
  have 3–10× margins. If a PR's coverage check flakes, rerun before treating it
  as a real regression.
