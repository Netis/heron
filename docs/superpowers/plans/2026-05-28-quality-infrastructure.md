# Quality infrastructure: testing + verification + canary chain

> **Status**: P0 done (PR#53, PR#57). P1-P4 ready for execution.
> Designed 2026-05-26; expect a separate engineering session to pick
> up P1 onward from this document.

## Context

Three prod failures in May 2026 motivate this plan:

1. **PR#48** — DuckDB FATAL reopen only covered the `agent_turns` writer;
   the read pool stayed poisoned. PR#52 fixed it after the prod 500-loop
   fired three times. Unit tests didn't catch it because they only
   exercised the "reopen succeeds" assertion, not "every downstream
   surface still works after reopen".
2. **PR#47** — Added a `TimeRange` validator but missed the caller in
   `agent_sessions.rs:58` that constructed the type directly, bypassing
   validation. Vivi caught it on second review by grep.
3. **PR#45** — Wired `secrets.AGENT_GH_TOKEN` into three workflows
   without provisioning the secret. The wiwi `actions/checkout` step
   exploded the first time it ran. Slipped because no test exercises
   the deploy/auth chain at PR time.

The pattern: each failure class is **mechanical and detectable**, but
nothing in CI or PR review looks for that class. The goal of this plan
is a layered chain — pre-commit → CI → staging → canary → prod observer —
where every class of failure has at least one deterministic gate before
it lands in front of users.

Plus an **agent system** that drives this chain autonomously so it
doesn't require the maintainer to remember to run anything.

A fourth real failure appeared during P0 itself and is now also covered:
4. **2026-05-26 `-` secret incident** — `printf VALUE | gh secret set NAME --body -`
   silently persisted the literal `-` as the secret value. The GHA
   masking algorithm then garbled every dash in workflow logs, and the
   real symptom showed up far from the cause (vivi `curl: option ***/v1/models`,
   wiwi `git push: Invalid username or token`). PR#57's
   `check-secret-values.sh` catches this class on every PR by asserting
   secret length / shape sanity (forbids `-`, empty, < 8 chars).
   Documented in memory `feedback_gh_secret_stdin_dash.md`.

## Architecture: layered chain

```
L0  pre-commit hooks (fmt/clippy/fast tests)               [exists]
L1  CI: cargo test --workspace + bun test                  [exists]
L2  CI: PCAP replay integration (ts-turn/tests/integration) [exists]
L3  CI: in-crate fault-injection + schema-migration tests   [P1, TODO]
L4  CI: secret-ref + secret-value + caller-audit linters    [DONE in P0]
L5  vivi PR diff review                                     [exists]
L6  lava: auto-deploy to single staging VM on wukong        [P2, TODO]
L7  tara: staging soak (replay-server + chaos + known-good self-test) [P2, TODO]
L8  lava: shadow canary on wuneng (separate data-dir + ZMQ feed) [P3, TODO]
L9  mara: continuous prod observer; closes incident loop    [P3, TODO]
```

L0–L5 gate every PR before merge. L6–L7 gate `main` before deploy.
L8 gates `main` before being marked the prod default. L9 runs forever
and feeds new incidents back to wiwi via auto-filed issues.

## Agent inventory

| Agent | Status | Lives where | Job |
|-------|--------|-------------|-----|
| **vivi** | ✅ live | self-hosted runner on wuneng | PR diff review; extend prompt with learnings-checklist from past misses |
| **wiwi** | ✅ live | self-hosted runner on wuneng | Autonomous PR implementer for well-scoped issues |
| **tara** | 🔴 not yet | will live inside `ts-stage` VM on wukong | Soak runner: drives replay-server load + fault-injection scenarios; emits pass/fail JSON to lava |
| **lava** (was deva) | 🔴 not yet | GitHub Actions workflow on self-hosted runner | Deploy coordinator: orchestrates staging-soak → canary → prod with explicit metric gates and auto-rollback |
| **mara** (was owly) | 🔴 not yet | wukong (NOT wuneng — failure isolation) | Prod observer: tails wuneng logs/metrics; auto-files issues with reproducer on FATAL/500/regression. Closes loop to wiwi. |

Distinction between tara and lava: **tara is a test runner script**
invoked inside the staging VM; **lava is a workflow orchestrator** that
owns gating across staging→canary→prod. Different blast radius and
different cadence — keep separate.

## wukong staging topology

**Single VM** (escalate to multiple only if CPU saturates):

- `ts-stage` on wukong: 8 GB RAM, 4 vCPU, 50 GB disk, libvirt-managed
- Reuses the proven libvirt path that already runs `tokenscope-ci` VM
  on wuneng
- One internal veth pair inside the VM so loadgen→tokenscope traffic
  is sniffable on a virtual NIC
- All staging components (tokenscope-under-test, replay-server,
  templated-generator, tara) run as systemd units inside the VM
- Snapshot baseline: weekly `virsh snapshot-create-as ts-stage clean`
  so every soak starts from a known clean state

Avoids three traps a multi-VM design would hit: libvirt quirks across N
domains, network bridge complexity, snapshot/restore fan-out.

## Small-model strategy (Qwen3.5 < 4B)

**Three-tier upstream** (NOT pure-Qwen):

| Tier | Share | What | Why |
|------|-------|------|-----|
| **Tier 1: replay server** | ~95% of load | Rust service that serves recorded SSE byte streams from `testdata/pcaps/` fixture extract. Microsecond-precise replay. | Real wire bytes, deterministic, fast. Already 90% built (`ts-pcap-extract` feeds this) |
| **Tier 2: templated generator** | ~4% of load | Tiny Rust service that emits configurable-shape OpenAI/Anthropic responses with knobs (tool-call count, stall ms, malformed JSON, premature disconnect) | Reproducible edge-case probing; no LLM in the loop |
| **Tier 3: Qwen3.5-1.7B offline** | 1%, **offline only** | Generates *new* PCAP fixtures when wire-api shape changes upstream. Captured to disk, fed back to tier 1. | LLM as **fixture-synthesizer**, not live upstream — live Qwen latency would distort TTFT measurements and make the whole soak benchmark useless. |

Qwen3.5-1.7B model already exists on wukong filesystem
(`/home/vader/models/`). Spin up via existing vLLM pattern for the
nightly fixture-gen cron only. Don't keep it warm.

## Health-check mechanism

Hybrid push (intra-system) + external pull (dead-man-switch):

1. **Push side**: each agent (tara, lava, mara) POSTs heartbeat to a
   new `/api/ops/heartbeat` endpoint on wuneng (separate from
   `/api/health` which is intentionally scoped narrow to pipeline
   drained-state — don't overload it). Heartbeat payload:
   `{agent, host, pid, last_action, last_outcome, ts}`.
2. **Pull side**: a 5-minute GitHub Actions schedule cron (or a wukong
   cron) hits `/api/ops/heartbeat-summary`. If the cron *itself*
   doesn't fire (GitHub records every schedule trigger), or wuneng
   doesn't respond, alert via email/mattermost. This is the
   dead-man-switch that catches "everything on wuneng died including
   the dashboard".
3. **systemd watchdog** for `heron` itself on wuneng + `ts-stage` —
   already deployable via systemd-run.
4. **Self-test layer**: every tara soak runs **two binaries** — the
   last-released tag + the PR candidate. Both go through identical
   load. If the released tag fails any invariant, the harness is
   broken, not the candidate. Catches harness rot, which would
   otherwise silently neuter the entire chain.
5. **Dashboard**: a new `/ops` page on the existing console showing:
   per-agent last-seen, VM uptime, last soak result, last canary
   timestamp, recent mara-filed issues, schedule-cron heartbeat
   freshness.

## Phased rollout (priority order = highest ROI first)

### P0 (DONE 2026-05-26..28)

- ✅ `scripts/lint/check-secrets.sh` — referenced secrets must be
  provisioned (closes PR#45 class)
- ✅ `scripts/lint/check-validated-constructors.py` — direct struct
  construction outside the validating constructor is rejected (closes
  PR#47 class)
- ✅ `scripts/lint/check-secret-values.sh` — secret values must be
  non-empty, ≥ 8 chars, not literal `-` (closes 2026-05-26
  stdin-`-` incident class)
- ✅ All three wired into `.github/workflows/ci.yml`

Cost: ~2 days. Retrospectively prevents 3 of 4 named incidents.

### P1 (1–2 weeks, RECOMMENDED NEXT)

**In-crate fault-injection module + DuckDB recovery tests:**

- Add `server/ts-storage-duckdb/src/fault_injection.rs` (feature-gated
  `FaultPoint` enum + injection hooks). Implementation note: in-crate,
  NOT a new crate — promotion to its own crate only when a second
  backend (e.g., parquet writer) needs the same hooks.
- Instrument `server/ts-storage/src/pair_sweeper.rs` with fault
  injection points behind the feature flag.
- Add an enabling cargo feature `fault-injection` in
  `server/ts-storage-duckdb/Cargo.toml`.
- Existing PR#52 test
  (`reopen_all_connections_keeps_reads_and_writes_alive`) becomes the
  template. Extend to deterministically drive `FaultPoint::DuckDbInvalidate`
  rather than relying on real prod-load pressure.

**Schema-migration test harness with golden DBs:**

- `testdata/golden-dbs/<version>.duckdb` — frozen DuckDB files per
  released version. Decide pinning strategy: per release tag is
  cleanest; per minor is acceptable.
- New test in `ts-storage-duckdb/tests/migrations.rs`: load golden DB
  → boot current binary → assert auto-migration runs + sample queries
  return canonical row counts.
- Pre-populate with `v0.1.0.duckdb` and `v0.2.0.duckdb`. Future
  releases follow the same convention.

**Closes**: PR#48 class + future schema migrations + the ENOSPC class
(fault-injection on disk-write paths).

### P2 (2–4 weeks)

**Single `ts-stage` VM on wukong** + replay-server tier 1 + tara soak
runner + lava deploy coordinator.

Files to add:

- `scripts/staging/provision-vm.sh` — wukong libvirt provision script
- `scripts/staging/replay_server.rs` (or new `ts-replay-upstream`
  binary inside `server/`) — tier-1 upstream
- `scripts/staging/templated_upstream.rs` — tier-2 edge-case generator
- `scripts/staging/lava.sh` — deploy orchestrator
- `scripts/staging/tara.sh` — soak runner
- `.github/workflows/staging-soak.yml` — runs tara on `ts-stage` after
  CI green

Verification:

- `./scripts/staging/lava.sh deploy <PR-branch>` provisions `ts-stage`,
  installs the build, runs tara → tara returns JSON `{pass: true, metrics: {…}}`
- During tara's run, replay-server feeds 5 min of `testdata/pcaps/`;
  `ts-stage`'s `/api/internal-metrics` must show zero
  `CaptureKernelPacketsDropped` and 100% LLM-call parse success
- Known-good-binary self-test: lava runs both `v0.2.0` tag and PR
  candidate through the same tara invocation; if v0.2.0 fails any
  invariant, lava marks "harness broken" and does NOT block the PR

### P3 (4–8 weeks)

**Shadow canary on wuneng** with separate data-dir + ZMQ feed +
cgroup mem cap + window-aligned metric diff. Plus **mara observer**
and `/api/ops/heartbeat` + `/ops` dashboard.

Files to add:

- `server/ts-api/src/routes/ops.rs` — `/api/ops/heartbeat` +
  `/api/ops/heartbeat-summary` endpoints
- `console/src/pages/ops.tsx` — health dashboard
- `scripts/staging/mara.sh` — prod log/metric observer

Canary deploy: parallel `heron --data-dir /var/lib/heron-canary
--feed zmq://...` (ZMQ feed from prod tap, no duplicate AF_PACKET on
the NIC) + `MemoryMax=4G` cgroup. Window-aligned 1-min comparison of
`agent_turns` count, p99 TTFT, FATAL-log count between prod and canary.
Initial threshold: ±2% on counts, ±5ms on p99 TTFT, 0 FATAL.

mara synthetic test: inject a FATAL log line in `ts-stage`'s
heron.log → mara observes within 30s → files an issue on the
`Netis/TokenScope` repo with the line + last-100-line context →
wiwi-eligible issue.

### P4 (when needed)

- `scripts/staging/generate_fixtures.py` — Qwen3.5 offline fixture
  builder
- Templated generator (tier 2)
- Dead-man-switch cron (`.github/workflows/dead-man-switch.yml`) — 5-min
  schedule that pull-monitors wuneng

### Pitfalls in shadow-canary design (must enforce when P3 lands)

1. **DuckDB file lock**: DuckDB explicitly does NOT support concurrent
   processes writing the same file. New binary MUST write to a
   separate directory (e.g., `/var/lib/heron-canary/`) — never
   `--data-dir` same as prod. The comparison is over the derived
   metrics, not the raw DB.
2. **libpcap kernel-side packet duplication cost**: two processes both
   opening AF_PACKET on the same NIC each consume a full copy of the
   ring buffer. On a busy NIC this can OOM or drop packets in **both**
   processes. Mitigation: have canary read via ZMQ from a tap
   (cloud-probe pattern that's already supported).
3. **Clock skew between processes**: TTFT comparisons fail silently if
   the two binaries have different `now_ms()` semantics. Pin both to
   the same `CLOCK_MONOTONIC` source and assert max-skew < 1ms in
   comparison harness.
4. **Memory pressure**: two heron processes on wuneng might trigger
   the OOM killer at peak load. Need explicit cgroup limit on canary
   (systemd-run supports this — `--property=MemoryMax=`).
5. **Metric divergence false-positives**: aggregation windows are not
   aligned across processes started seconds apart. Comparison harness
   must compare *aligned windows*, not "current 1-min metric".

## Critical files

### New (to be created in P1-P4)

- `server/ts-storage-duckdb/src/fault_injection.rs`
- `server/ts-api/src/routes/ops.rs`
- `console/src/pages/ops.tsx`
- `scripts/staging/provision-vm.sh`
- `scripts/staging/replay_server.rs` (or new `ts-replay-upstream`)
- `scripts/staging/templated_upstream.rs`
- `scripts/staging/lava.sh`
- `scripts/staging/tara.sh`
- `scripts/staging/mara.sh`
- `scripts/staging/generate_fixtures.py`
- `testdata/golden-dbs/<version>.duckdb`
- `.github/workflows/staging-soak.yml`
- `.github/workflows/dead-man-switch.yml`

### Modified

- `.github/workflows/ci.yml` — schema-migration test gate +
  fault-injection feature build (P1)
- `scripts/agent-bot/run_triage.sh` — extend vivi prompt with
  learnings checklist
- `server/ts-storage-duckdb/Cargo.toml` — add `fault-injection`
  cargo feature (P1)
- `server/ts-storage/src/pair_sweeper.rs` — instrument with fault
  injection points (P1)
- `server/ts-api/src/lib.rs` — register the new `ops` route (P3)
- `CLAUDE.md` — document the new chain (P3)

### Already done (P0; no further work)

- `scripts/lint/check-secrets.sh`
- `scripts/lint/check-secret-values.sh`
- `scripts/lint/check-validated-constructors.py`
- `scripts/lint/secrets.allowlist`
- `scripts/lint/validated-types.txt`
- `.github/workflows/ci.yml` (lint steps wired in)

## Verification (per phase)

### P0 — DONE

- ✅ Inject `${{ secrets.NEVER_PROVISIONED }}` into a workflow → CI
  fails secret-ref lint with exit 1
- ✅ Inject `AGENT_GH_TOKEN=-` (literal dash) → secret-value lint
  fails with actionable error pointing at `--body -` stdin bug
- ✅ Inject raw `TimeRange { start_us, end_us }` outside the
  validating constructor → caller-audit lint fails

### P1

- Enable `fault-injection` feature; trigger
  `pair_sweeper::FaultPoint::DuckDbInvalidate` in a test → assert
  `reopen_all_connections` recovers; assert query_turns,
  query_distinct_agent_kinds, write_turns/calls/metrics ALL succeed
  post-reopen (the test already in PR#52, extended to drive the
  fault point deterministically)
- Load `testdata/golden-dbs/v0.1.0.duckdb` → boot current binary →
  assert auto-migration runs + sample queries return canonical row
  counts

### P2

- `./scripts/staging/lava.sh deploy <PR-branch>` provisions
  `ts-stage`, installs build, runs tara → tara returns JSON
  `{pass: true, metrics: {…}}`. lava blocks promotion until pass=true.
- During tara's run, replay-server feeds 5 min of `testdata/pcaps/`;
  `ts-stage`'s `/api/internal-metrics` must show zero
  `CaptureKernelPacketsDropped` and 100% LLM-call parse success
- Known-good-binary self-test: lava runs both `v0.2.0` tag and PR
  candidate through the same tara invocation; if v0.2.0 fails any
  invariant, lava marks "harness broken" and does NOT block the PR

### P3

- Deploy canary to wuneng with `--data-dir /var/lib/heron-canary`
  + `--feed zmq://...` (ZMQ feed from prod tap, no duplicate
  AF_PACKET on the NIC) + `MemoryMax=4G` cgroup
- Window-aligned 1-min comparison of `agent_turns` count, p99 TTFT,
  FATAL-log count between prod and canary → assert within tolerance
- mara synthetic test: inject FATAL log line in `ts-stage`'s
  heron.log → mara observes within 30s → files an issue on the
  `Netis/TokenScope` repo with the line + last-100-line context

### P4

- `scripts/staging/generate_fixtures.py --model qwen3.5-1.7b
  --scenarios tool-chain-overflow,malformed-json` → produces new PCAP
  → existing `ts-turn/tests/integration.rs` consumes it → pass
- Kill wuneng `heron-adhoc.service` (post-Phase 4 rename) →
  dead-man-switch cron fires on next 5-min tick → email alert lands
  within 10 min of kill

## Explicit out-of-scope

- **No separate `ts-chaos` crate** — fault injection lives inside
  `ts-storage-duckdb` (or `h-storage-duckdb` post-Phase 2) as a
  feature-flagged module. Promote to its own crate only if a second
  backend needs the same hooks.
- **No multi-VM staging** — single `ts-stage`. Escalate only if
  loadgen contends with tokenscope-under-test for CPU.
- **No live Qwen3.5 mock-LLM upstream** — Qwen latency would distort
  TTFT measurements. Live tier is replay-server only.
- **No tara-as-agent rebrand** — tara is a script invoked by lava,
  not a standalone agent with its own workflow. Keep agent rank
  reserved for vivi/wiwi/lava/mara.
- **No frontend Playwright e2e** — low ROI for the current console
  surface; defer.

## Open decisions (flag at implementation time)

1. **Where does lava run?** GHA workflow on self-hosted runner
   (familiar; integrates with vivi/wiwi) OR dedicated long-running
   daemon on wukong (cleaner but new infra surface)?
2. **mara filing cadence**: dedupe by issue title (one issue per
   recurring FATAL pattern) or one issue per incident?
3. **Cgroup memory cap for canary**: 4 GB is a guess. Need a baseline
   measurement of prod heron steady-state RSS first.
4. **Schema-migration golden-DB pinning**: per release tag, per minor,
   or per major? Affects testdata/ size growth.

## Lessons that motivated this plan (for the next session's context)

Memory files in `/Users/vader/.claude/projects/-Users-vader-code-netis-TraceForge/memory/`:

- `feedback_gh_secret_stdin_dash.md` — the `--body -` footgun
- `reference_tokenscope_agent_bot.md` — vivi/wiwi loop topology
- `feedback_tokenscope_runner_pat_extraheader.md` — actions/checkout
  PAT collision on tokenscope-ci runner

Merged PRs that built the agent-bot foundation P0 sits on top of:

- PR#45 — agent-bot scaffolding
- PR#46 — post_review ADMIN_GH_TOKEN
- PR#51 — PAT fan-out for `agent:try` label
- PR#52 — DuckDB reopen ALL connections (the canonical "fault-injection
  needed" example)
- PR#53 — P0 lints: secret-ref + caller-audit
- PR#54 — wiwi checkout uses GITHUB_TOKEN + PAT in push URL only
- PR#55 — wiwi 120-min timeout + stream claude output live
- PR#56 — clear extraheader before push so PAT wins over App token
- PR#57 — P0 secret-value linter
- PR#58 — wiwi commit + auto-commit fallback
- PR#59 — full Heron rebrand Phase 1+3+6

Future sessions can resume the plan from P1 directly; the agent-bot
substrate is fully operational and the four most important failure
classes (PR#45/#47/#48 + the `-` secret class) are now gated in CI.
