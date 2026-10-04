# 11 — Verification Coverage

**Status:** Proposed
**Scope:** cross-cutting — test policy, CI gates, tooling, and agent workflow. No runtime behavior change.
**Related:** [Implementation plan](../superpowers/plans/2026-09-29-verification-coverage.md)

## Motivation

We currently have no coverage measurement at all. The quality chain (see
`CLAUDE.md` → *Quality & release pipeline*) is strong but entirely
*scenario-driven*: pcap-corpus goldens, schema-migration goldens,
fault-injection recovery, aglake retry/auth/concurrency, the staging soak and
the nightly longevity soak. Those tests encode past incidents well, but nothing
tells us **what is not tested**, and a PR can add a new state machine, a new
storage branch, or a new parser rule with zero behavioral verification and pass
CI green.

A single repo-wide `Line ≥ 80%` would not fix that. It rewards trivial code and
is easy to game. This document defines a **risk-driven verification coverage
policy** with six dimensions, and — critically — makes the three that resist
gaming the headline gates:

> **Changed-code coverage · Critical-scenario coverage · Mutation score**

Line coverage is kept as a hygiene floor, not a target.

## What already exists (do not rebuild)

| Dimension | Existing asset |
|---|---|
| System / E2E | `app/heron/tests/pipeline_e2e.rs`, `cli_smoke.rs`, `signal_shutdown.rs` |
| System / soak | `staging-soak`, `ebpf-soak`, nightly `longevity-soak` |
| Fault injection | `h-storage-duckdb --features fault-injection` `concurrent_tests` |
| Data migration | `h-storage-duckdb/tests/migrations.rs` (legacy DB shapes) |
| Protocol parser | `h-turn/tests/corpus_golden.rs` + `just corpus` |
| Durability / auth | `h-storage-aglake` `retry_tests` / `auth_tests` / `concurrency_tests` |
| Judgement logic | `scripts/staging/tests/test_tara_invariants.py`, `test_longevity_check.py` |
| Syntax / lint | `scripts/lint/*`, clippy (informational), fmt, tsc |
| Hot-path perf | `h-protocol/benches/hot_paths.rs` (compile-only bitrot gate) |
| Review | `pr-review` agent (olympus) |

The gap is **measurement, per-tier floors, diff coverage, scenario
completeness, mutation testing, waivers, and independent test generation.**

## Design summary

| Aspect | Decision |
|---|---|
| Policy SSOT | `verification/coverage-policy.json` (crates → tier → floors) |
| Machine baseline | `verification/coverage-baseline.json` (committed, ratchets up only) |
| Waivers | `verification/waivers.json` (reason + owner + expiry, enforced) |
| Scenarios | `verification/scenarios.json` + in-code `@scenario <ID> <kind>` tags |
| Rust instrumentation | `cargo-llvm-cov` → lcov; **line/region/function on stable**, branch via nightly |
| TS instrumentation | `bun test --coverage-reporter=lcov` |
| Diff coverage | stdlib `scripts/ci/diff_coverage.py` (lcov + `git diff -U0`), no pip deps |
| Mutation | `cargo-mutants`; `--in-diff` on PRs, full Tier 0 nightly |
| Detectives | `scripts/ci/check_coverage_policy.py`, `check_scenarios.py` |
| Headline gates | changed-line ≥ policy, Tier 0 scenario ≥ 100%, Tier 0/1 mutation floors |
| New-crate gate | every workspace crate must be classified in the policy (static lint) |

## 1. Code coverage — a floor, not a goal

Reported metrics and their use:

| Metric | Meaning | Use |
|---|---|---|
| Line | lines executed | hygiene floor / regression signal |
| **Region** | basic-block/region edges | **stable branch proxy** (see note) |
| Function | function entered at least once | finds wholly untested modules |
| Branch | `if`/`match`/`?` edges | nightly + Tier 0 only |
| Condition | `A && B` combinations | Tier 0 logic, expressed as scenario tests rather than a metric |

**Branch coverage caveat.** `cargo-llvm-cov --branch` is currently *unstable*
and needs a nightly toolchain (`-Z coverage-options=branch`). The repo already
provisions nightly for eBPF. We use **region coverage as the stable, blocking
proxy** for edge coverage, and run `--branch` on nightly as the authoritative
edge metric for Tier 0/1 where it is available. If the flag breaks, the gate
degrades to region rather than failing the build.

### Per-tier floors

Floors are *not* one number for the repo, and the initial values are set to the
recorded baseline (rounded down), then ratcheted. `0` means "not gated".

| Tier | Line | Region | Function | Mutation | Scenario |
|---|---|---|---|---|---|
| **Tier 0** — safety/integrity | 95 | 90 | 90 | 85 | 100% P0 |
| **Tier 1** — core product | 90 | 85 | 85 | 70 | 100% P0/P1 |
| **Tier 2** — normal app | 80 | 75 | 80 | — | — |
| **Tier 3** — glue/UI/generated | 60 | — | — | — | — |

Tier 0 additionally requires, by construction: boundary tests, negative tests,
fault injection, and concurrency tests where the module has concurrency. These
are enforced through the scenario matrix (§4), not through a percentage.

## 2. Criticality tiers

Assignment is explicit and lives in the policy file. New crates default to
*unclassified* and fail the static lint until assigned.

| Module | Tier | Rationale |
|---|---|---|
| `h-storage` | 0 | write-buffer durability contract, retention, pair sweep |
| `h-storage-duckdb` | 0 | schema migration + recovery + checkpoint integrity |
| `h-storage-clickhouse` | 0 | **data deletion / retention** — currently the weakest crate |
| `h-storage-aglake` | 0 | retry/ack durability, session auth, bounded fan-out |
| `h-protocol` | 0 | L2–L4 + HTTP/SSE parser; boundary + `unsafe` cast surface |
| `h-capture/ebpf` | 0 | uprobe capture, `unsafe`, TLS framing |
| `h-llm` | 1 | wire-API detection + extractors (product core) |
| `h-turn` | 1 | agent-turn state machine |
| `h-metrics` | 1 | sliding-window aggregation / percentiles |
| `h-capture` | 1 | libpcap + ZMQ acquisition, pcap rotation/retention |
| `h-api` | 1 | REST surface driving the console |
| `app/heron` | 1 | pipeline orchestration + shutdown |
| `h-pcap-extract` | 2 | read-side utility |
| `h-common` | 2 | config / errors / internal metrics |
| `h-export` | 2 | export formatting |
| `console` | 3 | presentation |
| `console/src/lib/wire-apis`, `console/src/lib/call-pair.ts` | 2 | logic mirrored from the backend |

Excluded (with reason, not silently): `h-ebpf-prog` (BPF program, separate
`bpfel-unknown-none` toolchain), `h-ebpf-common` (types shared into the BPF
program, no host runtime).

## 3. Changed-code coverage (the primary per-PR gate)

Repository floors are aspirational for legacy code. What a PR *must* satisfy is
coverage of the code it changes:

```yaml
changed_code:
  line:   90
  region: 85      # stable edge proxy
  branch: 85      # when the nightly branch report is supplied
  require_change: true   # no new untested lines in Tier 0/1 without a waiver
```

Computed by `scripts/ci/diff_coverage.py`: parse lcov, take the added/changed
line ranges from `git diff -U0 <merge-base>...HEAD -- 'server/**/*.rs'`,
intersect, and apply the policy for the owning tier (Tier 2/3 use a lower
floor; Tier 3 is excluded). A changed line in an existing file that was
previously untested still counts against the PR — this is what stops the
"just add code, tests later" pattern.

## 4. Scenario coverage (behavior, not code)

If line coverage is 98% but only the happy path is tested, the system is still
unreliable. Tier 0/1 modules get an explicit **scenario matrix** in
`verification/scenarios.json` (shown YAML-style below for readability; the
committed file is JSON so the stdlib-only checkers can read it):

```yaml
version: 1
scenarios:
  - id: DUCKDB-RECOVER-001
    tier: 0
    priority: P0
    module: h-storage-duckdb
    requirement: "DuckDB FATAL/invalidate is followed by reopen_all_connections"
    invariant: "no committed row is lost and every read surface works after reopen"
    verification: [integration, fault]
  - id: DUCKDB-DISKFULL-001
    tier: 0
    priority: P0
    module: h-storage-duckdb
    requirement: "write during ENOSPC returns Err, never a partial row"
    verification: [fault]
  - id: MIGRATE-LEGACY-001
    tier: 0
    priority: P0
    module: h-storage-duckdb
    requirement: "init() migrates every recorded legacy DB shape"
    verification: [integration]
  - id: PARSE-CORPUS-001
    tier: 0
    priority: P0
    module: h-turn
    requirement: "known pcap corpus replays to invariant parse/pair/turn/persist"
    verification: [integration, e2e]
  - id: AGLAKE-RETRY-413-001
    tier: 0
    priority: P0
    module: h-storage-aglake
    requirement: "HEC 413 is split/retried; partial-success 400 retries invalid events"
    verification: [integration]
  - id: AGLAKE-AUTH-401-001
    tier: 0
    priority: P0
    module: h-storage-aglake
    requirement: "expired session is re-authenticated before the next write"
    verification: [integration]
  - id: CLICKHOUSE-DELETE-001
    tier: 0
    priority: P0
    module: h-storage-clickhouse
    requirement: "retention sweep deletes exactly the expired range and leaves the rest"
    verification: [integration]
```

Tests declare which scenario they verify with a tag the checker can see:

```rust
// @scenario DUCKDB-DISKFULL-001 fault
#[tokio::test]
async fn write_returns_err_on_disk_full() { ... }
```

`scripts/ci/check_scenarios.py` (stdlib) scans `server/**/*.rs` and
`console/src/**/*.ts` for `@scenario <ID> <kind>` tags and asserts:
- every scenario has ≥1 test per `verification` kind it declares;
- every `P0`/`P1` scenario is fully verified;
- every tag names an existing scenario (no orphan tags);
- every Tier 0/1 module in the policy appears in the matrix.

This is a deterministic, offline, cheap gate — it runs in the existing `ci.yml`
lint phase, not in the heavy coverage job.

## 5. Mutation testing (test effectiveness)

Coverage proves code ran; mutation proves the test would notice if it were
wrong. `cargo-mutants` on Tier 0 (and Tier 1 once calibrated):

- **PR**: `cargo-mutants --in-diff <patch> --in-place` on Tier 0 files only, so
  cost scales with the diff, not the crate.
- **Nightly**: full Tier 0 mutation run; surviving mutants in safety-critical
  code open a scrubbed, deduplicated issue (same pattern as the longevity
  soak). Mutation score = killed / (killed + survived), excluding
  `unviable`/`timeout`.

Mutation is deliberately **not repo-wide** — the build cost (bundled DuckDB)
makes it prohibitive and low-value in presentation code.

## 6. System coverage (already largely present)

Integration, E2E, performance, soak, and fault/chaos are tracked as a first-
class dimension rather than a side effect of the coverage number. The existing
staging/ebpf/longevity soaks and `synth_*` pipelines are the implementation.
The policy records which system-level suites cover which Tier 0 scenario so the
matrix can show the full row (unit / integration / e2e / fault) and expose
empty cells such as `CLICKHOUSE-DELETE-001` above.

## 7. Waivers

Every exclusion or floor reduction requires a durable, expiring waiver:

```yaml
waivers:
  - module: h-storage-clickhouse
    metric: scenario
    reason: "retention-delete integration test requires a live CH instance; staging covers it"
    owner: platform
    expires: 2026-12-31
    tracking: "#<issue>"
```

`check_coverage_policy.py` fails on: missing owner, missing expiry, malformed
date, or an expired waiver. A waiver that expires must be renewed deliberately
or the underlying gap fixed — exclusions cannot accumulate silently. The
allowlist files under `scripts/lint/` already follow this "no silent
allowlist" philosophy; this mirrors it.

## 8. Agent-generated tests

Because agents produce code far faster than verification, two rules apply to
agent workflows:

1. **No coverage-targeted test generation.** An agent may not be tasked with
   "raise coverage to X". Tests are derived from the requirement, invariant,
   edge cases, and failure modes (the scenario matrix is the input), then the
   implementation is verified against them — never tests reverse-engineered
   from the implementation.
2. **Independent verifier.** Tests are not all authored by the implementation
   agent. The flow is:

   ```
   Coding Agent → implementation
                      │
                      ▼
   Test Agent ← spec / design doc / scenario matrix   (independent)
                      │
                      ▼
   Mutation Agent → verifier                           (independent)
   ```

   The `pr-review` agent (olympus) gains a test-quality section that flags
   assertion-free, implementation-mirroring, and coverage-chasing tests, and
   requires a `@scenario <ID> <kind>` tag for Tier 0/1 changes without one.

## Tooling and where it runs

| Check | Workflow | Blocking |
|---|---|---|
| Policy schema, tier classification, waiver expiry | `ci.yml` (lint) | yes |
| Scenario matrix completeness / orphan tags | `ci.yml` (lint) | yes |
| Changed-code coverage (Rust + TS) | `coverage.yml` | yes |
| Per-tier floors vs. committed baseline (no decrease) | `coverage.yml` | yes |
| Branch coverage (nightly toolchain) | `coverage.yml` (schedule) | nightly report; Tier 0 after calibration |
| Mutation `--in-diff` (Tier 0) | `coverage.yml` | yes after calibration |
| Full Tier 0 mutation | `mutation.yml` (schedule) | nightly issue, not a block |
| System suites | existing `ci.yml` / staging / ebpf / longevity | yes (existing) |

Coverage is split into its own workflow because instrumentation recompiles the
workspace (bundled DuckDB dominates) and the single `ci-pool` runner must not
have the required `ci` check held hostage by a 30–60 min coverage build. The
cheap static gates stay in `ci.yml` so policy drift is caught even when the
coverage job is skipped.

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Coverage build exceeds runner budget | M | separate workflow, shared cache, `paths:` filter, `--no-report` then report, shard |
| `--branch` unstable flag breaks | M | region is the blocking proxy; branch degrades to nightly report |
| Floors set too high → gate disabled to merge | H | baseline-derived initial floors + expiring waivers; diff gate is the real bar |
| Mutation cost | H | `--in-diff` on PR, Tier 0 only, nightly full |
| Tests written to the metric | H | scenario matrix + independent Test Agent + review rule |
| Baseline ratchet blocks unrelated merges | M | nightly baseline refresh on main; no-decrease compared to committed baseline |
| Coverage of `main.rs` / FFI skews numbers | L | `#[coverage(off)]`/exclusions in policy, documented |

## Rollout

1. **Measure** — add tooling, produce the first report, commit the baseline.
2. **Ratchet** — enforce changed-code coverage + static policy/scenario gates.
3. **Scenarios** — expand the matrix and require P0 tags in `ci.yml`.
4. **Mutation** — calibrate Tier 0, then enforce `--in-diff`.
5. **System + agents** — wire the nightly mutation issue and the independent
   Test Agent, and add the test-quality section to `pr-review`.

See the [implementation plan](../superpowers/plans/2026-09-29-verification-coverage.md).
