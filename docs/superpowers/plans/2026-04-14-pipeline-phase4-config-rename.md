# Pipeline Phase 4: `worker_count` rename + shard-count guards — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename the misleading `pipeline.worker_count` knob to `pipeline.flow_shard_count` and guard `spawn_turn_stage` / `spawn_metrics_stage` against `shard_count = 0` (which would otherwise hang shutdown forever).

**Architecture:** Two orthogonal changes in one phase. (1) A pure rename across `PipelineConfig`, `ProtocolStageConfig`, `default.toml`, `main.rs`, and the integration test — the codebase always compiles in a single atomic commit. (2) Two entry-point `assert!` lines plus `#[should_panic]` tests. No behavior change for correctly-configured deployments.

**Tech Stack:** Rust, Tokio, serde (config), TOML.

**Spec:** `docs/superpowers/specs/2026-04-14-pipeline-phase4-config-rename-design.md`

---

## File Structure

**Modify:**
- `server/ts-common/src/config.rs` — rename `PipelineConfig::worker_count` and its default fn.
- `server/config/default.toml` — rename the TOML key under `[pipeline]`.
- `server/ts-protocol/src/stage.rs` — rename `ProtocolStageConfig::worker_count`, update docstring and panic messages.
- `server/app/tokenscope/src/main.rs` — update field access, local binding name, and startup log line.
- `server/ts-turn/tests/integration.rs` — update the local `worker_count` variable and `ProtocolStageConfig` field init.
- `server/ts-turn/src/stage.rs` — add `shard_count >= 1` guard + `#[should_panic]` test.
- `server/ts-metrics/src/stage.rs` — add `shard_count >= 1` guard + `#[should_panic]` test.
- `docs/design/pipeline-performance.md` — update as-built references from `worker_count` to `flow_shard_count`.

**Not modified (immutable history):**
- `docs/superpowers/specs/2026-04-13-pipeline-phase1-*.md`
- `docs/superpowers/specs/2026-04-13-pipeline-phase2-*.md`
- `docs/superpowers/plans/2026-04-13-pipeline-phase1-*.md`
- `docs/superpowers/plans/2026-04-13-pipeline-phase2-*.md`

---

## Task 1: Guard `spawn_turn_stage` against `shard_count = 0`

**Files:**
- Modify: `server/ts-turn/src/stage.rs` — add guard + test.

- [ ] **Step 1: Write the failing test**

Add this test **inside the existing `#[cfg(test)] mod tests { ... }` block** in `server/ts-turn/src/stage.rs` (the file already has a tests module with similar `#[should_panic]` tests — scan for `panics_on_length_mismatch` to locate the right spot, and place the new test adjacent to it):

```rust
    #[tokio::test]
    #[should_panic(expected = "spawn_turn_stage: shard_count must be >= 1")]
    async fn panics_on_zero_shard_count() {
        let (_turns_tx, _turns_rx) = mpsc::channel::<LlmTurn>(1);
        spawn_turn_stage(
            TurnStageConfig {
                shard_count: 0,
                tracker: TrackerConfig::default(),
            },
            vec![],
            _turns_tx,
            ts_llm::profiles::build_default_registry,
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ts-turn --lib stage::tests::panics_on_zero_shard_count`
Expected: FAIL. The existing `assert_eq!(shard_rxs.len(), config.shard_count, ...)` currently accepts `(0, 0)` as equal, so `spawn_turn_stage` returns normally without spawning any tasks — the `#[should_panic]` expectation is not met.

- [ ] **Step 3: Add the guard assertion**

In `server/ts-turn/src/stage.rs`, inside `pub fn spawn_turn_stage<F>(...)`, insert the new assertion **before** the existing `assert_eq!(shard_rxs.len(), ...)`. The existing signature body starts at line 35-48; the result should read:

```rust
pub fn spawn_turn_stage<F>(
    config: TurnStageConfig,
    shard_rxs: Vec<mpsc::Receiver<IdentifiedCall>>,
    turns_tx: mpsc::Sender<LlmTurn>,
    build_registry: F,
) where
    F: Fn() -> ProfileRegistry + Send + Sync + 'static,
{
    assert!(
        config.shard_count >= 1,
        "spawn_turn_stage: shard_count must be >= 1"
    );
    assert_eq!(
        shard_rxs.len(),
        config.shard_count,
        "shard_rxs.len() must equal config.shard_count",
    );
    let build_registry = Arc::new(build_registry);
    // ... (rest of function body unchanged)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ts-turn --lib stage::tests::panics_on_zero_shard_count`
Expected: PASS.

Also run the full `ts-turn` test suite to check no regression:

Run: `cargo test -p ts-turn`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add server/ts-turn/src/stage.rs
git commit -m "feat(ts-turn): guard spawn_turn_stage against shard_count=0

Without this, a misconfigured pipeline.turn_shard_count=0 would spawn
zero shards, never close turns_tx, and hang sink_handle.await at
shutdown. Caught by the final Phase-2 review."
```

---

## Task 2: Guard `spawn_metrics_stage` against `shard_count = 0`

**Files:**
- Modify: `server/ts-metrics/src/stage.rs` — add guard + test.

- [ ] **Step 1: Write the failing test**

Add this test inside the existing `#[cfg(test)] mod tests { ... }` block in `server/ts-metrics/src/stage.rs` (place it next to the existing `panics_on_length_mismatch` test):

```rust
    #[tokio::test]
    #[should_panic(expected = "spawn_metrics_stage: shard_count must be >= 1")]
    async fn panics_on_zero_shard_count() {
        let (_mtx, _mrx) = mpsc::channel::<LlmMetric>(1);
        spawn_metrics_stage(MetricsStageConfig { shard_count: 0 }, vec![], _mtx);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ts-metrics --lib stage::tests::panics_on_zero_shard_count`
Expected: FAIL. Same reason as Task 1 — the `assert_eq!(0, 0)` passes and `spawn_metrics_stage` quietly returns.

- [ ] **Step 3: Add the guard assertion**

In `server/ts-metrics/src/stage.rs`, at the top of `pub fn spawn_metrics_stage(...)`, insert the new assertion **before** the existing `assert_eq!(shard_rxs.len(), ...)`:

```rust
pub fn spawn_metrics_stage(
    config: MetricsStageConfig,
    shard_rxs: Vec<mpsc::Receiver<LlmEvent>>,
    metrics_tx: mpsc::Sender<LlmMetric>,
) {
    assert!(
        config.shard_count >= 1,
        "spawn_metrics_stage: shard_count must be >= 1"
    );
    assert_eq!(
        shard_rxs.len(),
        config.shard_count,
        "spawn_metrics_stage: shard_rxs.len() must equal config.shard_count",
    );
    // ... (rest of function body unchanged)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ts-metrics --lib stage::tests::panics_on_zero_shard_count`
Expected: PASS.

Run: `cargo test -p ts-metrics`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add server/ts-metrics/src/stage.rs
git commit -m "feat(ts-metrics): guard spawn_metrics_stage against shard_count=0

Mirrors the ts-turn guard. A misconfigured pipeline.metrics_shard_count=0
would otherwise spawn zero shards, never close metrics_tx, and hang the
sink drain at shutdown."
```

---

## Task 3: Rename `worker_count` → `flow_shard_count` (atomic, all call sites)

This is a single-commit rename. The codebase must compile after the commit, so every file that references `worker_count` changes in the same commit.

**Files:**
- Modify: `server/ts-common/src/config.rs`
- Modify: `server/config/default.toml`
- Modify: `server/ts-protocol/src/stage.rs`
- Modify: `server/app/tokenscope/src/main.rs`
- Modify: `server/ts-turn/tests/integration.rs`

- [ ] **Step 1: Rename `PipelineConfig::worker_count`**

Edit `server/ts-common/src/config.rs`. Locate the `PipelineConfig` struct (currently around lines 83-99). Replace the block with:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    #[serde(default = "default_flow_shard_count")]
    pub flow_shard_count: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            flow_shard_count: default_flow_shard_count(),
        }
    }
}

fn default_flow_shard_count() -> usize {
    4
}
```

No other symbols in this file change.

- [ ] **Step 2: Rename `ProtocolStageConfig::worker_count`**

Edit `server/ts-protocol/src/stage.rs`. Replace the struct, its `Default` impl, and the doc/panic strings. The resulting top of the file (lines 15-50 in the current version) becomes:

```rust
/// Configuration for the protocol parsing stage.
pub struct ProtocolStageConfig {
    pub flow_shard_count: usize,
    /// Channel capacity for dispatcher → each worker (internal).
    pub worker_queue_size: usize,
}

impl Default for ProtocolStageConfig {
    fn default() -> Self {
        Self {
            flow_shard_count: 4,
            worker_queue_size: 4096,
        }
    }
}

/// Spawn the protocol parsing stage: one dispatcher task that consumes from
/// `raw_rx`, plus `config.flow_shard_count` flow-worker tasks whose outputs are
/// routed to `event_txs[i]`.
///
/// Panics if `event_txs.len() != config.flow_shard_count` — that is a wiring
/// bug in the composition root, not a runtime condition.
pub fn spawn_protocol_stage(
    config: ProtocolStageConfig,
    mut raw_rx: mpsc::Receiver<RawPacket>,
    event_txs: Vec<mpsc::Sender<ProtocolEvent>>,
    metrics_sys: &mut MetricsSystem,
) {
    assert_eq!(
        event_txs.len(),
        config.flow_shard_count,
        "event_txs length must equal flow_shard_count (composition-root wiring bug)"
    );

    let mut worker_txs = Vec::with_capacity(config.flow_shard_count);
```

Only these lines change; the rest of `spawn_protocol_stage` is unchanged (the internal loop uses `event_txs.into_iter()`, which does not reference the field).

- [ ] **Step 3: Update TOML default**

Edit `server/config/default.toml`. Locate the `[pipeline]` section (currently around lines 27-29) and replace:

```toml
[pipeline]
worker_count = 4
```

with:

```toml
[pipeline]
flow_shard_count = 4
```

- [ ] **Step 4: Update `main.rs` call sites and startup log**

Edit `server/app/tokenscope/src/main.rs`. Two edits:

First, the startup log around line 150 currently reads:

```rust
    tracing::info!("  pipeline: {} worker(s)", config.pipeline.worker_count);
```

Replace with a single topology line that also removes the now-redundant scattered shard logging. Find the existing block of `tracing::info!` lines for capture/pipeline/storage/api/internal_metrics/turn and update the "pipeline" line, then *append* a shards line right after the turn line. The final ordering of the info block becomes:

```rust
    tracing::info!("  capture: {} effective source(s)", source_configs.len());
    for (i, src) in source_configs.iter().enumerate() {
        tracing::info!("    source[{i}]: {src:?}");
    }
    tracing::info!(
        "  pipeline: flow_shard_count={}",
        config.pipeline.flow_shard_count
    );
    tracing::info!("  storage: backend={}", config.storage.backend);
    tracing::info!("  api: {}:{}", config.api.listen, config.api.port);
    tracing::info!(
        "  internal_metrics: enabled={}, interval={}s",
        config.internal_metrics.enabled,
        config.internal_metrics.interval_secs
    );
    tracing::info!(
        "  turn: idle_timeout={}s, sweep_interval={}s",
        config.turn.idle_timeout_secs,
        config.turn.sweep_interval_secs
    );
    tracing::info!(
        "  shards: flow={} turn={} metrics={}",
        config.pipeline.flow_shard_count,
        config.turn.shard_count,
        config.metrics.shard_count
    );
```

Second, the "Compose channels" section around line 227 currently reads:

```rust
        let worker_count = config.pipeline.worker_count;
        let turn_shards = config.turn.shard_count;
        let metrics_shards = config.metrics.shard_count;
        let queue_size = 4096usize;

        let (raw_tx, raw_rx) = mpsc::channel(queue_size);

        let mut event_txs = Vec::with_capacity(worker_count);
        let mut event_rxs = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let (tx, rx) = mpsc::channel(queue_size);
            event_txs.push(tx);
            event_rxs.push(rx);
        }
```

Rename the local `worker_count` → `flow_shards` (to match the vocabulary; `_shards` plural aligns with the sibling `turn_shards` / `metrics_shards` locals already in this function):

```rust
        let flow_shards = config.pipeline.flow_shard_count;
        let turn_shards = config.turn.shard_count;
        let metrics_shards = config.metrics.shard_count;
        let queue_size = 4096usize;

        let (raw_tx, raw_rx) = mpsc::channel(queue_size);

        let mut event_txs = Vec::with_capacity(flow_shards);
        let mut event_rxs = Vec::with_capacity(flow_shards);
        for _ in 0..flow_shards {
            let (tx, rx) = mpsc::channel(queue_size);
            event_txs.push(tx);
            event_rxs.push(rx);
        }
```

And the `ProtocolStageConfig` construction block, currently:

```rust
        let protocol_cfg = ProtocolStageConfig {
            worker_count,
            ..Default::default()
        };
```

becomes:

```rust
        let protocol_cfg = ProtocolStageConfig {
            flow_shard_count: flow_shards,
            ..Default::default()
        };
```

- [ ] **Step 5: Update the integration-test helper**

Edit `server/ts-turn/tests/integration.rs`. Around line 45-73, the `run_pcap_sharded` helper currently sets:

```rust
    let worker_count = 1usize;
```

and later constructs `ProtocolStageConfig` with `worker_count`:

```rust
    let cfg = ProtocolStageConfig {
        worker_count,
        ..Default::default()
    };
```

Rename the local to `flow_shards` and pass it as the new field:

```rust
    let flow_shards = 1usize;
```

```rust
    let cfg = ProtocolStageConfig {
        flow_shard_count: flow_shards,
        ..Default::default()
    };
```

These are the only two lines in the test file that reference `worker_count`.

- [ ] **Step 6: Build to verify nothing else references the old name**

Run: `cd server && cargo build --workspace --all-targets`
Expected: clean build, no errors, no new warnings. If the compiler surfaces any remaining `worker_count` reference (for instance in a file this plan did not anticipate), fix that reference the same way — struct field → `flow_shard_count`, local variable → `flow_shards` — and re-run until clean.

- [ ] **Step 7: Run the workspace test suite**

Run: `cd server && cargo test --workspace`
Expected: all tests pass (278 pre-existing + 2 new guard tests from Tasks 1 & 2 = 280).

- [ ] **Step 8: Commit**

```bash
git add server/ts-common/src/config.rs server/config/default.toml \
        server/ts-protocol/src/stage.rs server/app/tokenscope/src/main.rs \
        server/ts-turn/tests/integration.rs
git commit -m "refactor(phase4): rename pipeline.worker_count → flow_shard_count

The field counts flow-sharded protocol + llm extraction tasks (the two
stages are 1:1 coupled by hash(flow_key) % N). Aligns with the
shard_count vocabulary introduced in Phase 2 and removes the misleading
'worker' term. Also adds a single 'shards: flow=.. turn=.. metrics=..'
startup log line for operator visibility.

Breaking: existing config files using pipeline.worker_count fail to
deserialize after this commit. Pre-GA; no alias."
```

---

## Task 4: Update the living `pipeline-performance.md` doc

**Files:**
- Modify: `docs/design/pipeline-performance.md`

- [ ] **Step 1: Replace every current-topology reference**

Open `docs/design/pipeline-performance.md`. Using a text editor with literal (non-regex) find-and-replace, replace the string `worker_count` with `flow_shard_count` **only in the "Current Implementation" / "Current Stage-by-Stage Analysis" / "Why the Current Design Does Not Fully Utilize Multicore" / "Summary" sections** — i.e., the sections describing the codebase as-built today. These contain the shard-key formulas and surrounding prose:

- line ~46: `shard key: hash(flow_key) % worker_count`
- line ~123: `shard = hash(flow_key) % worker_count`
- line ~141: `same shard key as the protocol stage (hash(flow_key) % worker_count, inherited from FlowDispatcher)`
- line ~282: `The current default worker_count is fixed at 4.` → `The current default flow_shard_count is fixed at 4.`
- line ~535: `hash(flow_key) % worker_count` inside the "Per-flow correlation preserved" bullet
- line ~543: `different worker_count` in the `LlmCall.id` stability note → `different flow_shard_count`
- line ~727: `LLM extraction now scales with worker_count` → `LLM extraction now scales with flow_shard_count`

- [ ] **Step 2: Rewrite the "Phase 4: CPU-Aware Defaults" section header note**

The "Phase 4" section at line ~648-670 currently proposes the CPU-aware defaults under its own heading using `worker_count`. **Keep that section's body using `worker_count`** — it is a snapshot of the future work proposal as of Phase 1 writing, and a companion "Phase 4.1: Config Rename" sub-section captures what actually landed.

At line ~648 (`### Phase 4: CPU-Aware Defaults`), **insert** a new subsection *above* the existing one:

```markdown
### Phase 4.1: Config rename — `worker_count` → `flow_shard_count` — ✅ Implemented (2026-04-14)

The name `pipeline.worker_count` did not say what it counted. After
Phase 1 and Phase 2, the field drives both the protocol flow-worker
pool and the llm-processor pool (1:1 coupled by `hash(flow_key) % N`).
Phase 4.1 renamed it to `pipeline.flow_shard_count`, aligning with the
`shard_count` vocabulary introduced for `turn.shard_count` /
`metrics.shard_count` in Phase 2.

Spec: `docs/superpowers/specs/2026-04-14-pipeline-phase4-config-rename-design.md`.
Also introduced: `shard_count >= 1` entry-point guards on
`spawn_turn_stage` and `spawn_metrics_stage` to prevent a silently-hung
shutdown when the shard count was configured as zero.

The CPU-aware-defaults portion of the original Phase 4 proposal remains
future work — see the sub-section below.

### Phase 4: CPU-Aware Defaults
```

(That is: the existing heading `### Phase 4: CPU-Aware Defaults` stays where it is. The new "Phase 4.1" section is inserted just above it.)

- [ ] **Step 3: Update the "Recommended Evolution Path" subsection bullets**

Still in `docs/design/pipeline-performance.md`, the "Phase 4: CPU-Aware Defaults" body contains two bullets that use `worker_count`:

- line ~654: `` - `ts-common::config` — replace fixed defaults (`pipeline.worker_count = 4`) with CPU-derived defaults``
- line ~655: `` - apply to `worker_count`, `turn.shard_count`, `metrics.shard_count``

Update to:

```markdown
- `ts-common::config` — replace fixed defaults (`pipeline.flow_shard_count = 4`) with CPU-derived defaults
- apply to `flow_shard_count`, `turn.shard_count`, `metrics.shard_count`
```

And the table row:

- line ~661: `` | `pipeline.worker_count` | `max(2, num_cpus)` | ...``

Update to:

```markdown
| `pipeline.flow_shard_count` | `max(2, num_cpus)` | protocol + LLM extraction is the hottest stage after Phase 1 |
```

And the prose line:

- line ~665: `` Keep explicit overrides in config. Log the effective values at startup (already done for `worker_count`; extend to the new fields).``

Update to:

```markdown
Keep explicit overrides in config. Log the effective values at startup (already done for `flow_shard_count`; extend to the new fields).
```

And line ~669-670:

```markdown
- explicit config values in existing deployments continue to win over the new defaults (test by setting `worker_count = 2` in a config and verifying the startup log reports 2, not `num_cpus`)
- startup log reports the resolved effective values for `worker_count`, `turn.shard_count`, `metrics.shard_count` so operators can see what was picked
```

becomes:

```markdown
- explicit config values in existing deployments continue to win over the new defaults (test by setting `flow_shard_count = 2` in a config and verifying the startup log reports 2, not `num_cpus`)
- startup log reports the resolved effective values for `flow_shard_count`, `turn.shard_count`, `metrics.shard_count` so operators can see what was picked
```

- [ ] **Step 4: Update Phase 3's shard-key formula mentions**

Line ~621 of the same file reads:

```markdown
- worker shard key promotes to `hash(source_id, flow_key) % worker_count` to avoid cross-source flow-key collisions when address space overlaps
```

and line ~385 has a diagram line:

```text
shard key:  hash(source_id, flow_key) % worker_count
```

and line ~313 has:

```markdown
Worker shards (shard key: hash(source_id, flow_key) % worker_count) own:
```

Replace `worker_count` with `flow_shard_count` in all three. These are forward-looking references (Phase 3 / target architecture) and will be consistent with the renamed field when Phase 3 lands.

- [ ] **Step 5: Verify nothing else in the file still uses `worker_count`**

Run (from the repo root):

```bash
grep -n worker_count docs/design/pipeline-performance.md
```

Expected: no matches.

(If any remain — for example, a quoted literal in a code block that *was* historical prose but got missed — read the surrounding sentence: if it describes the code as it exists today or is forward-looking, update it; if it is strictly a historical quote (e.g., "Phase 1 originally proposed..."), leave it. Record your call in the commit message if ambiguous.)

- [ ] **Step 6: Commit**

```bash
git add docs/design/pipeline-performance.md
git commit -m "docs(pipeline-performance): update living doc for flow_shard_count rename

Updates as-built topology sections and future-phase references from
worker_count to flow_shard_count. Adds a Phase 4.1 landed-work entry
under Recommended Evolution Path. Historical Phase 1 / Phase 2
spec & plan documents under docs/superpowers/ are NOT edited — they
are point-in-time artifacts."
```

---

## Task 5: Workspace verification

- [ ] **Step 1: Full test suite**

Run: `cd server && cargo test --workspace`
Expected: 280 tests pass (278 pre-phase-4 + 2 new guard tests). If any test references `worker_count` and fails to compile, you missed a call site in Task 3 — locate it (`grep -rn worker_count server/`) and fix.

- [ ] **Step 2: Warning sweep**

Run: `cd server && cargo build --workspace --all-targets 2>&1 | grep -E "warning|error"`
Expected: no output (the Phase 2 Task 14 cleanup left the workspace at zero warnings; Phase 4 should not regress this).

- [ ] **Step 3: Format check on touched files**

Run (from `/Users/timmy/code/netis/TokenScope/server`):

```bash
rustfmt --check --edition 2021 \
    ts-common/src/config.rs \
    ts-protocol/src/stage.rs \
    ts-turn/src/stage.rs \
    ts-turn/tests/integration.rs \
    ts-metrics/src/stage.rs \
    app/tokenscope/src/main.rs
```

Expected: clean (exit 0, no diff output other than nightly-feature warnings). If there is a diff, apply it:

```bash
rustfmt --edition 2021 \
    ts-common/src/config.rs \
    ts-protocol/src/stage.rs \
    ts-turn/src/stage.rs \
    ts-turn/tests/integration.rs \
    ts-metrics/src/stage.rs \
    app/tokenscope/src/main.rs
```

Then re-run the workspace tests to confirm nothing broke, and stage the format fixes for a separate commit:

```bash
git add -u server
git commit -m "style: cargo fmt after Phase 4 rename"
```

(If there is no diff, skip this commit.)

- [ ] **Step 4: Smoke-test the rename end-to-end**

Run: `cd server && cargo run --bin tokenscope -- --config config/default.toml` and let it run for ~2 seconds, then Ctrl+C.
Expected startup log contains the new lines:

```
  pipeline: flow_shard_count=4
  shards: flow=4 turn=1 metrics=1
```

and clean shutdown (no panic, no hang). If the binary panics with `unknown field worker_count` at startup, your `default.toml` edit from Task 3 Step 3 was missed — fix and re-run.

(If `cargo run` is infeasible in the execution environment — e.g., missing libpcap — substitute `cargo build --bin tokenscope` and a manual read-through of `src/main.rs` to confirm the log lines are the new form.)

---

## Self-Review

**Spec coverage:**

| Spec requirement | Task |
|---|---|
| Rename TOML key in `default.toml` | Task 3 Step 3 |
| Rename Rust field on `PipelineConfig` | Task 3 Step 1 |
| Rename `ProtocolStageConfig::worker_count` | Task 3 Step 2 |
| Update internal wiring (main.rs) | Task 3 Step 4 |
| Update test wiring (integration.rs) | Task 3 Step 5 |
| Update startup log line | Task 3 Step 4 |
| Update living `pipeline-performance.md` as-built sections | Task 4 Steps 1-5 |
| Do NOT edit historical Phase 1 / 2 spec & plan docs | Task 4 (explicit non-goal in "Files not modified") |
| Entry guard on `spawn_turn_stage` | Task 1 |
| Entry guard on `spawn_metrics_stage` | Task 2 |
| Full `cargo test --workspace` passes | Task 5 Step 1 |
| `cargo fmt` clean on touched files | Task 5 Step 3 |
| No alias for old key — clean deserialize error on legacy configs | Task 3 Step 1 (`#[serde(default = ...)]` on renamed field only; legacy `worker_count` has no handler and errors) |

No gaps.

**Placeholder scan:** No "TBD", "implement later", "handle edge cases", or unqualified "write tests". Every code-changing step contains either the exact new code or the exact before/after snippet with file path.

**Type consistency:**
- `pipeline.flow_shard_count: usize` in `PipelineConfig` (Task 3 Step 1) matches usage in `main.rs` (Task 3 Step 4) and the test file uses the field name on `ProtocolStageConfig`, not the config struct.
- `ProtocolStageConfig::flow_shard_count: usize` (Task 3 Step 2) matches the field-init syntax in `main.rs` Task 3 Step 4 and `integration.rs` Task 3 Step 5.
- Guard panic messages are quoted identically in the assertion body (Tasks 1/2 Step 3) and the `#[should_panic(expected = ...)]` attribute (Tasks 1/2 Step 1).
- Log line wording (`pipeline: flow_shard_count=N`, `shards: flow=N turn=N metrics=N`) is consistent between Task 3 Step 4 (implementation) and Task 5 Step 4 (verification).
