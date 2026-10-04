# Verification Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add risk-driven verification coverage to Heron — measurement, per-tier
floors, a changed-code gate, a scenario matrix, mutation testing, expiring
waivers, and independent agent-generated tests — without making a repo-wide
line-coverage number the target.

**Architecture:** A committed policy
(`verification/coverage-policy.json`) maps every workspace crate to a
criticality tier with per-metric floors. `cargo-llvm-cov` and `bun test
--coverage` produce lcov; stdlib Python checkers under `scripts/ci/` enforce the
policy, the changed-code floor, scenario completeness, and waiver expiry. A
separate `coverage.yml` workflow owns the heavy instrumentation; cheap static
gates live in the existing `ci.yml` lint phase.

**Tech Stack:** Rust + `cargo-llvm-cov` + `cargo-mutants`; Bun + `bun test
--coverage-reporter=lcov`; Python 3 stdlib only (no pip deps on the runner).

**Design:** [11 — Verification Coverage](../../design/11-verification.md)

> **Status:** Implemented. Machine artifacts live in `verification/` and
> `scripts/ci/`. Policy/scenario JSON (not YAML) is the checked format so
> the runner stays dependency-free (no PyYAML/tomllib).

---

## File map

**New — policy + data:**

| File | Responsibility |
|---|---|
| `verification/coverage-policy.json` | tiers, floors, changed-code thresholds, exclusions |
| `verification/coverage-baseline.json` | committed per-crate measured baseline |
| `verification/waivers.json` | expiring coverage/scenario waivers |
| `verification/scenarios.json` | Tier 0/1 scenario matrix |

**New — tooling:**

| File | Responsibility |
|---|---|
| `scripts/ci/coverage.sh` | orchestrates llvm-cov / bun coverage; modes `rust`, `ts`, `all` |
| `scripts/ci/diff_coverage.py` | lcov + `git diff -U0` → changed-line/region coverage verdict |
| `scripts/ci/check_coverage_policy.py` | schema, crate classification, floors vs baseline, waiver expiry |
| `scripts/ci/check_scenarios.py` | scenario/tag completeness |
| `scripts/ci/mutation.sh` | `cargo-mutants` full (tier0) and `--in-diff` (PR) modes |
| `scripts/ci/tests/` | stdlib self-tests for the three Python checkers |
| `verification/README.md` | how to run locally, how to add a scenario/waiver |

**New — CI:**

| File | Responsibility |
|---|---|
| `.github/workflows/coverage.yml` | instrumentation, diff gate, floors, nightly branch + mutation |

**Modify:**

| File | Change |
|---|---|
| `docs/design/README.md` | add row 11 |
| `justfile` | `just coverage …` router |
| `scripts/routers/shared/` | `coverage.sh` router (if separate script preferred) |
| `.github/workflows/ci.yml` | run `check_coverage_policy.py` + `check_scenarios.py`; console coverage |
| `.gitignore` | ignore `target/llvm-cov*`, `lcov.info`, `cobertura.xml`, `mutants.out/` |
| `scripts/lint/check-leakage.sh` / allowed paths | ensure new verification files are covered by leakage gate (no action beyond confirming) |
| olympus `pr-review` prompt + agent workflows | test-quality section; Test Agent / Mutation Agent |

---

## Phase 0 — Measurement first

### Task 0.1: Pin tooling and prove a local report

**Files:** none (environment only)

- [ ] Verify `cargo llvm-cov --version` (0.9.0+) and `bun --version` on the CI
  pool; install `cargo-mutants` (`cargo install cargo-mutants --locked`).
- [ ] Run a first, capped measurement locally to learn runtime and output
  shape:

  ```bash
  cd server
  cargo llvm-cov --workspace --lcov --output-path /tmp/heron-lcov.info
  cargo llvm-cov report --summary-only
  ```

  Record wall-clock. If the workspace build exceeds the runner budget, note
  which crates dominate — the plan already splits the job, but this data
  decides whether to shard by crate in Phase 2.
- [ ] Confirm region/function are present in the JSON export
  (`cargo llvm-cov --json`). Confirm `--branch` needs nightly:
  `cargo +nightly llvm-cov report --branch` (expect it to work with a nightly
  install, fail on stable).

### Task 0.2: Console coverage

**Files:** `console/package.json`

- [ ] Add scripts:

  ```json
  "test:coverage": "bun test --coverage --coverage-reporter=lcov --coverage-reporter=text"
  ```

- [ ] Run it and confirm `console/coverage/lcov.info` is produced.

---

## Phase 1 — Policy, baseline, static gates

### Task 1.1: Author the policy file

**Files:** Create `verification/coverage-policy.json`

- [ ] Write the policy exactly as specified in the design doc §1–§3: tiers,
  floors, `changed_code`, `modules` (all crates from `server/Cargo.toml` plus
  `console`), `exclusions` (`h-ebpf-prog`, `h-ebpf-common`), `waivers_file`.
- [ ] Include a deliberate placeholder baseline block; it is filled by Task 1.3.

### Task 1.2: Policy checker + self-test

**Files:**
- Create `scripts/ci/check_coverage_policy.py`
- Create `scripts/ci/tests/test_check_coverage_policy.py`

- [ ] Write the checker (stdlib `yaml` is **not** stdlib — use a tiny parser or
  require PyYAML? The runner has PyYAML? Check. If not, add a minimal YAML
  reader or store policy as JSON). **Decision: store machine policy as
  `verification/coverage-policy.json`** to stay dependency-free, and keep a
  human-readable `coverage-policy.json` only if PyYAML is available. Prefer
  JSON for the checked artifact.

  Re-scope Task 1.1/1.2 to JSON:
  - `verification/coverage-policy.json`
  - checker loads with `json` (stdlib).

- [ ] Checker responsibilities:
  1. `--static`: every crate in `server/Cargo.toml` `[workspace] members`
     (expanded) is classified or excluded; every tier referenced exists; every
     waiver has `reason`, `owner`, `expires` (ISO date), and is unexpired.
  2. `--report <json>`: compare each crate's measured line/region/function to
     its tier floor **and** to `coverage-baseline.json` (no decrease beyond a
     small epsilon).
  3. Exit non-zero with a file/line-readable diff of failures.
- [ ] Self-test with synthesized policy/report JSON covering: unclassified
  crate, expired waiver, missing owner, floor violation, no-decrease violation,
  pass. Mirror the style of `scripts/staging/tests/test_tara_invariants.py`.
- [ ] Wire the self-test into `ci.yml`.

### Task 1.3: Record and commit the baseline

**Files:** Create `verification/coverage-baseline.json`

- [ ] Convert the Phase 0 `--json` report into the baseline shape:
  `{ "<crate>": { "line": …, "region": …, "function": … }, … }`.
- [ ] Set each tier's floor to `min(policy_floor, floor(baseline - slack))`
  for the first commit, and open a follow-up issue to ratchet toward the design
  targets. Document the current value in a comment / `baseline_commit`.
- [ ] `check_coverage_policy.py --report` must pass against this baseline.

### Task 1.4: Wire static gates into `ci.yml`

**Files:** Modify `.github/workflows/ci.yml`

- [ ] Add, next to the existing lint steps (no compile):

  ```yaml
  - name: lint — coverage policy + waivers
    run: python3 scripts/ci/check_coverage_policy.py --static
  - name: lint — coverage policy checker self-test
    run: python3 scripts/ci/tests/test_check_coverage_policy.py
  - name: lint — scenario matrix completeness
    run: python3 scripts/ci/check_scenarios.py
  - name: lint — scenario checker self-test
    run: python3 scripts/ci/tests/test_check_scenarios.py
  ```

- [ ] The scenario checker lands in Phase 3; add a stub that passes now and is
  replaced then, or land Phase 3 before wiring. Recommended: land Phase 3's
  checker first if ordering matters.

### Task 1.5: `just coverage` router

**Files:** Modify `justfile` (+ `scripts/routers/shared/coverage.sh` if the
router pattern is kept)

- [ ] Add `coverage` action: `rust`, `ts`, `all`, `report`, `diff <base>`,
  `mutation`.
- [ ] Update `just help` Testing block.

---

## Phase 2 — Changed-code coverage (primary PR gate)

### Task 2.1: lcov diff checker

**Files:**
- Create `scripts/ci/diff_coverage.py`
- Create `scripts/ci/tests/test_diff_coverage.py`

- [ ] Implement:
  - parse lcov (`SF:`, `DA:<line>,<count>`, `BRDA:`, `FNDA:`) into per-file
    line/region/function maps;
  - `git diff --unified=0 --no-color <merge-base>...HEAD -- 'server/**/*.rs'`
    (and later `'console/src/**/*.ts'`) → added line ranges;
  - intersect with DA records; compute changed-line coverage;
  - apply the owning tier's `changed_code` thresholds (Tier 3 excluded);
  - emit a Markdown summary suitable for a PR comment and exit non-zero on
    failure.
- [ ] Self-test with synthetic lcov + a synthetic patch (use a temp git repo,
  mirroring `scripts/lint/test-check-leakage.sh`).
- [ ] Make the base ref configurable (`--base`, default `origin/$GITHUB_BASE_REF`).

### Task 2.2: Coverage workflow

**Files:** Create `.github/workflows/coverage.yml`

- [ ] Trigger: `pull_request` (paths `server/**`, `verification/**`,
  `.github/workflows/coverage.yml`), `schedule` (nightly), `workflow_dispatch`.
- [ ] Same fork-PR safety `if` as `ci.yml` (self-hosted runner).
- [ ] Steps: checkout (no LFS needed), cache `~/.cargo` + `server/target`
  + `console/node_modules`; install toolchain (reuse the nightly guard);
  `cargo llvm-cov --workspace --json --lcov`; `bun test --coverage-reporter=lcov`;
  upload lcov/JSON as artifacts.
- [ ] Gates:
  - `check_coverage_policy.py --report` (floors + no-decrease);
  - `diff_coverage.py` (PR only).
- [ ] Optional PR comment with the changed-code summary (respect the
  no-secrets/leakage rule — no host paths).
- [ ] Add `coverage` to branch-protection required checks (documented in the
  design doc / ops notes). Keep it out of the release gate until calibrated.

### Task 2.3: Baseline refresh on main

**Files:** Modify `.github/workflows/coverage.yml`

- [ ] On `push` to `main`, after a passing report, write
  `verification/coverage-baseline.json` and commit it with the repo's bot
  conventions (or open a bot PR if direct commits to `main` are protected).
  The baseline only ratchets up; the no-decrease check runs pre-merge.

---

## Phase 3 — Scenario coverage

### Task 3.1: Seed the matrix

**Files:** Create `verification/scenarios.json`

- [ ] Add the seed scenarios from the design doc §4, plus a first sweep for
  every other Tier 0/1 module so `check_scenarios.py` has full coverage of the
  tier map. Keep `verification` as `[unit, integration, e2e, fault]`.
- [ ] For each existing test that already verifies a scenario, add a
  `// @scenario <ID>` tag. Do **not** invent tests here; tag first, then the
  checker reveals genuine gaps (e.g. `CLICKHOUSE-DELETE-001`).

### Task 3.2: Scenario checker + self-test

**Files:**
- Create `scripts/ci/check_scenarios.py`
- Create `scripts/ci/tests/test_check_scenarios.py`

- [ ] Scan `server/**/*.rs` and `console/src/**/*.{ts,tsx}` for
  `@scenario <ID>`; parse the matrix; assert the four invariants in design §4.
- [ ] Support a waiver lookup so a documented gap (`CLICKHOUSE-DELETE-001`)
  passes via `verification/waivers.json` with owner+expiry rather than a hard
  failure.
- [ ] Self-test: missing verification kind, orphan tag, P0 gap, waiver
  accepted, expired waiver rejected.

### Task 3.3: Close the highest-value gaps

**Files:** per-gap tests

- [ ] `h-storage-clickhouse` retention-delete integration test (Tier 0, P0) —
  the largest known hole. Use the existing mock/HTTP pattern from
  `h-storage-aglake` tests if a live instance is unavailable, or add the test
  to staging and waive the repo-level cell with an expiry.
- [ ] Boundary/negative tests for `h-protocol` framing and `@scenario` tags.
- [ ] Concurrency scenario for the aglake fan-out semaphore if not already
  tagged.

### Task 3.4: Enforce in `ci.yml`

**Files:** Modify `.github/workflows/ci.yml`

- [ ] Replace the Phase 1 stub with the real `check_scenarios.py` + self-test.

---

## Phase 4 — Mutation testing

### Task 4.1: Mutation runner

**Files:** Create `scripts/ci/mutation.sh`

- [ ] Modes:
  - `diff`: `git diff > /tmp/pr.patch`; `cargo mutants --in-diff /tmp/pr.patch
    --in-place -p <tier0 crates touched> --timeout <n> --jobs <n>`; compute
    score from `mutants.out/outcomes.json`.
  - `full`: run each Tier 0 crate; emit score per crate.
- [ ] Exclude `h-ebpf-prog` and generated code; cap runtime with `--timeout`.

### Task 4.2: Calibrate then gate

**Files:** Modify `.github/workflows/coverage.yml`, create
`.github/workflows/mutation.yml`

- [ ] Run full Tier 0 mutation by hand; record achieved scores in the policy
  (initial mutation floor = achieved, rounded down). Enforce `--in-diff` on PRs
  once the false-failure rate is acceptable (start `continue-on-error: true`,
  then flip).
- [ ] Add nightly `mutation.yml`: full Tier 0 run; on a drop below the floor,
  file a scrubbed, deduplicated issue (reuse the longevity-soak issue pattern
  and the `check-leakage` scrubbing rules).

---

## Phase 5 — System coverage + agent independence

### Task 5.1: System coverage in the matrix

**Files:** Modify `verification/scenarios.json`, `docs/design/11-verification.md`

- [ ] For each Tier 0 scenario, record which system suite covers it
  (`pipeline_e2e`, `corpus_golden`, `staging-soak`, `ebpf-soak`,
  `longevity-soak`, fault-injection). This makes empty system-level cells
  visible rather than implied.

### Task 5.2: Agent test independence

**Files:** olympus `pr-review` prompt + agent workflow (cross-repo)

- [ ] Add a **Test quality** section to the `pr-review` prompt:
  - reject tests that merely mirror the implementation or assert nothing;
  - require a `@scenario` tag for Tier 0/1 changes with new behavior;
  - flag "coverage once, never asserted" patterns and missing boundary/negative
    cases for Tier 0;
  - flag an agent task that says "raise coverage".
- [ ] Add a **Test Agent** flow (modelled on `issue-implement.yml`) that takes
  a scenario ID + design doc and writes failing tests *before* implementation.
  Tests authored here are independent of the implementation agent by
  construction.
- [ ] Add a **Mutation Agent** follow-up that turns surviving Tier 0 mutants
  into issues that the Test Agent can consume.

### Task 5.3: Docs + leakage

**Files:** `docs/design/README.md`, `verification/README.md`,
`scripts/lint/check-leakage.sh` allowlist review, `CLAUDE.md`

- [ ] Add row 11 to `docs/design/README.md`.
- [ ] Write `verification/README.md` (local commands, adding a scenario,
  adding a waiver, reading the report).
- [ ] Add a short *Verification coverage* subsection under *Quality & release
  pipeline* in `CLAUDE.md` linking to the design doc.
- [ ] Confirm the leakage gate covers `verification/**` and the new scripts
  (no private paths/IPs in committed policy or reports). Never commit a raw
  report containing absolute machine paths — normalize to repo-relative.

---

## Verification

- [ ] `python3 scripts/ci/check_coverage_policy.py --static` passes and fails
  correctly on a deliberately unclassified crate.
- [ ] `python3 scripts/ci/check_scenarios.py` passes; removing a P0 test tag
  makes it fail (verified in the self-test).
- [ ] `just coverage diff main` on a branch with a new untested function fails
  the changed-code gate.
- [ ] `coverage.yml` completes within the runner budget on a representative PR;
  record the wall-clock in the PR.
- [ ] `cargo mutants --in-diff` produces a score on a Tier 0 diff.
- [ ] No file under `verification/` contains a host path, private IP, or secret.

---

## Implementation status (as built)

**Landed**

- `verification/coverage-policy.json`, `coverage-baseline.json`,
  `scenarios.json`, `waivers.json`, `README.md`.
- `scripts/ci/check_coverage_policy.py` (+13 tests),
  `check_scenarios.py` (+9 tests), `diff_coverage.py` (+9 tests) — all stdlib.
- `scripts/ci/coverage.sh`, `scripts/ci/mutation.sh`; `just coverage` /
  `just mutation` routers.
- `.github/workflows/coverage.yml` (instrument, floors/no-decrease,
  changed-code gate, nightly branch report) and `.github/workflows/mutation.yml`
  (weekly full Tier 0); static gates wired into `ci.yml`.
- 18 Tier 0/1 scenarios with 20 in-code `@scenario <ID> <kind>` tags across the
  existing suites. `CLICKHOUSE-DELETE-001` is now verified by
  `server/h-storage-clickhouse/src/retention_tests.rs` (a scripted ClickHouse
  HTTP mock: 5 tests covering per-table sweeps, predicates, label escaping,
  `OPTIMIZE FINAL`, and error propagation) plus 3 `cutoff_micros` unit tests —
  so the waiver was retired. `h-storage/src/convert.rs` gained 6 tests for the
  previously-untested `headers_to_json` / `parse_json_string_list`.
- **Follow-up (bold pass):** the mock was factored into
  `h-storage-clickhouse/src/test_mock.rs` and reused for
  `query_tests.rs` — 9 tests locking the read-path invariants on every list
  query (**no `JOIN`**, **pagination id tie-break**, ClickHouse literal
  escaping, `fromUnixTimestamp64Micro` bounds, sort allow-list). `h-api`'s
  existing DuckDB test binary gained a **whole-router smoke**
  (`all_get_routes_answer_without_500`) that drives every GET endpoint through
  the real router, which also exercises the DuckDB query layer and
  `h-capture::interfaces` transitively.
- **Follow-up (bold pass 2 — aglake):** the same mock pattern now covers the
  aglake read face. `h-storage-aglake/src/search_mock.rs` answers the real
  `POST /api/v1/search` SPL client (open-auth mode) and `query_tests.rs` (5
  tests) drives every list / lookup / aggregate method, asserting the page-count
  companion, sort tie-break, `_raw` fetch discipline, and no unbounded window
  leaking out of a windowed list.
- **Follow-up (bold pass 3 — clickhouse deep):** the mock was extended to
  capture the URL-query statement of streaming `insert()`, and
  `query_tests.rs` grew to call **every** read method plus the write paths
  (via `it.rs` fixtures) and `init()` DDL. This covers the row-decode mappings,
  the `rows.rs` `From` impls, and `schema.rs`.
- **Follow-up (bold pass 4 — heron orchestration):** a latent defect was
  exposed. `pipeline_e2e` (and much of `h-turn/tests/integration.rs`)
  referenced legacy fixtures that are gitignored and never generated, so they
  had been **silently skipping** — the graceful `else { skip }` hid dead tests.
  Repointing the E2E suite at the committed git-LFS corpus made it run, which
  revealed it then **hung**: `Pipeline::build` spawns the pair sweeper as an
  intentional forever-task, and the test awaited *all* stage handles. The drain
  now awaits only finite stages and aborts the sweeper before reopening the DB.
  Added a broad all-corpus replay (11 fixtures → spans/traces/metrics) and CLI
  smoke for `config validate` / `doctor` / `aglake-props`. `pipeline.rs`
  0% → 72%; `cmd/doctor` 0→62%, `cmd/validate` 0→52%, `cmd/aglake_props`
  0→64%; `heron` line **25.98% → 49.13%**, fn **23.18% → 44.37%**.
- **Coverage gains** (first baseline run → after): `h-storage-clickhouse` line
  **2.03% → 52.24%**, fn **8.03% → 48.43%** (retention.rs ~91%, schema.rs ~79%,
  rows.rs ~95%, metrics ~55%, services ~41%); `h-api` 66.42% → 79.49% line,
  55.66% → 74.11% fn; `h-storage-duckdb` 78.02% → 82.79% line, 41.39% → 45.09%
  fn; `h-storage-aglake` **49.08% → 61.00% line, 53.46% → 65.74% fn**
  (distincts 0→98%, read 45→87%, metrics 43→73%, services 31→65%, turns 35→66%);
  `heron` **25.98% → 49.13% line, 23.18% → 44.37% fn**; `h-storage` 86.87% →
  87.14% line; `h-capture` 85.74% → 87.11% line. **Repo-wide line 73.38% →
  79.32%, region 68.52% → 75.13%, function 73.18% → 80.56%.** Every delta
  traced to a new test; `coverage-baseline.json` is refreshed from the measured
  run. (One coverage job run flaked under instrumentation and passed on retry —
  noted as a follow-up.)

**Known-dead tests surfaced — fixed**

- `h-turn/tests/integration.rs` referenced legacy fixtures
  (`claude-cli-messages.pcap`, `codex-cli-messages-multi.pcap`, …) that are
  gitignored and never generated, so those tests silently self-skipped in CI.
  The suite now resolves fixtures from the committed git-LFS `corpus/` first
  (LFS-aware), the single-turn tests were repointed to their corpus equivalents
  (`claude-cli-anthropic-stream`, `codex-responses`, `openclaw-openai-chat`,
  `openclaw-anthropic-parallel`, `hermes-openai-chat`) with assertions retuned
  to the corpus ground truth, and renamed to match. The four tests whose
  captures genuinely have no committed equivalent (multi-turn claude/codex,
  gemini) are now `#[ignore = …]` — visible debt, not silent skips. Net: 10
  integration tests now run (shard-parity, flow-shard reorder, tool-id
  canonicalization, parallel-tool_use accumulator), 4 explicitly ignored.
- Same class still unaudited elsewhere: any test using a bare
  `else { skip }` on a fixture path is a candidate; the corpus-first resolver
  in `corpus_golden.rs` / `pipeline_e2e.rs` / `integration.rs` is the pattern.
- Docs: `docs/design/11-verification.md`, `docs/design/README.md`, `CLAUDE.md`,
  `verification/README.md`, `.gitignore`, `console/package.json`.

- **Mutation as a PR signal — calibrated and armed (advisory).**
  `mutation.sh diff` now emits a **workspace-relative** patch: cargo-mutants
  matches `--in-diff` paths against the `server/` workspace root, so
  repo-relative `server/...` paths silently select **zero** mutants (found while
  calibrating — a `--relative` from `server/` is required). Verified end to end:
  a two-line Tier 0 change selected 4 mutants, all caught in ~10 s.
  `coverage.yml` runs it on PRs with `continue-on-error: true`; promote to
  blocking once the false-positive rate is measured on real diffs.

**Deferred (requires capacity / cross-repo access)**

- **Tier ratchet is ON.** `coverage.yml` runs
  `check_coverage_policy.py --report … --enforce-tiers`. The five crates that
  meet their tier floors (`h-turn`, `h-protocol`, `h-llm`, `h-metrics`,
  `h-pcap-extract`, plus `h-common`) are now hard-enforced — they cannot
  regress below target. The nine crates still ratcheting carry a crate-level
  `metric: "tier"` waiver in `verification/waivers.json` (owner + expiry),
  so the debt is visible and must be renewed or repaid — not one waiver per
  metric (which would be a dumpster).
- **Mutation calibration — started.** `cargo-mutants` 27.1 is installed and
  `h-storage`'s pure Tier 0 modules are calibrated: `dialect.rs` 24/24 caught
  (100% after fixing 3 missed mutants — the `sql_in_list` wrapper and the
  non-empty `tool_surface` clause were untested despite ~100% line coverage);
  `convert.rs` 18 caught / 1 unviable / 3 documented equivalent mutants, 0
  missed. A `server/.cargo/mutants.toml` records the equivalents with reasons.
  `mutation.sh` now uses `--in-place` (mandatory: cargo-mutants copies only the
  `server/` workspace, but `h-common` reads the repo-root `VERSION` via
  `include_str!`, so a copy never builds) and is single-threaded as a result.
  Remaining: calibrate the DuckDB-backed Tier 0 crates (expensive — prefer
  `--in-diff` per PR) and promote mutation to a PR check.
- The large live-gated surfaces (`h-storage-clickhouse` write paths / row
  decoding, `h-storage-aglake` client, deep per-route `h-api` assertions,
  `heron` orchestration) still need either a live server in CI or more scripted
  mocks; the `test_mock.rs` + `query_tests.rs` pair is the template.
- Auto-refresh `coverage-baseline.json` on merge to `main` (manual
  `--write-baseline` for now; direct-commit policy on `main` decides the shape).
- Agent independence (Phase 5.2): the `pr-review` test-quality section and the
  Test/Mutation agents live in the olympus agent repo, not here.
- Branch-coverage enforcement (nightly report is advisory until the unstable
  `--branch` flag proves stable on the pool).
