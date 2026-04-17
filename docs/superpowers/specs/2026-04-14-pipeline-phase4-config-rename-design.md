# Pipeline Phase 4: `worker_count` rename + shard-count guards

**Date:** 2026-04-14
**Status:** Design
**Depends on:** Phase 2 (`2026-04-13-pipeline-phase2-turn-metrics-sharding-design.md`) — landed 2026-04-13

## Goal

Make pipeline parallelism knobs self-explanatory and guard against
misconfigured shard counts. This is a **minimal, surgical** slice of
the larger Phase 4 work sketched in `docs/design/pipeline-performance.md`
— CPU-aware defaults are explicitly out of scope.

## Motivation

`pipeline.worker_count` does not say what it counts. After Phase 1 and
Phase 2, the field actually means "how many flow-sharded parallel
pairs of (protocol worker, llm processor) tasks the pipeline runs" —
the value drives two stages that are 1:1 coupled by `hash(flow_key) % N`.
Meanwhile `turn.shard_count` and `metrics.shard_count` use the clearer
"shard_count" vocabulary introduced in Phase 2. This phase aligns the
flow-sharding knob with that vocabulary.

In addition, Phase 2's final review flagged that `spawn_turn_stage` /
`spawn_metrics_stage` accept `shard_count = 0` without complaint: the
stage then spawns zero tasks, never closes `turns_tx` / `metrics_out_tx`,
and `sink_handle.await` in main.rs hangs forever. A one-line assertion
at each stage entry closes the gap.

## Scope

### In scope

1. **Rename** `pipeline.worker_count` → `pipeline.flow_shard_count`:
   - TOML key in `server/config/default.toml`
   - Rust field on `PipelineConfig` in `ts-common/src/config.rs`
   - `ProtocolStageConfig.worker_count` field in `ts-protocol/src/stage.rs`
   - All internal wiring, tests, and `tracing::info!` startup log line
   - Comments and the `docs/design/pipeline-performance.md` "as-built"
     sections (historical Phase 1 / Phase 2 spec & plan docs are immutable
     and are NOT edited)

2. **Entry-point guards**: add
   `assert!(config.shard_count >= 1, "<stage>: shard_count must be >= 1")`
   to the top of `spawn_turn_stage` and `spawn_metrics_stage`. This
   matches the existing `assert!(!metrics_shard_txs.is_empty(), ...)` /
   `assert!(!turn_shard_txs.is_empty(), ...)` style in `spawn_llm_stage`.

### Out of scope

- CPU-aware defaults (`max(2, num_cpus)` and friends). Values stay
  hard-coded at 4 / 1 / 1.
- Moving `turn.shard_count` / `metrics.shard_count` into `[pipeline]`.
  Those remain in their domain sections because they only affect their
  own stage; `flow_shard_count` is in `[pipeline]` because it spans
  protocol + llm.
- The two other Phase-2 final-review follow-ups: `sweep()` micro-opt and
  fixture-conditional parity test. Tracked separately.
- Any change to how flow sharding works (the hash function, the 1:1
  pairing, or the channel topology).

## Design

### Configuration

`server/config/default.toml`:

```toml
[pipeline]
flow_shard_count = 4    # flow-sharded protocol + llm extraction tasks

[turn]
idle_timeout_secs = 600
sweep_interval_secs = 10
shard_count = 1

[metrics]
shard_count = 1
```

`ts-common/src/config.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    #[serde(default = "default_flow_shard_count")]
    pub flow_shard_count: usize,
}

fn default_flow_shard_count() -> usize { 4 }
```

No migration / alias for the old key. This pre-GA codebase does not
promise config-forward-compat, and silently accepting the old name
would defeat the "self-explanatory" goal. Users upgrading across this
change get a clear `missing field / unknown field` deserialization
error at startup.

### Protocol stage

`ts-protocol/src/stage.rs`: rename `ProtocolStageConfig.worker_count`
to `ProtocolStageConfig.flow_shard_count`. Update the panic message
and docstring accordingly. All call sites update to pass the new
name.

### Turn / metrics stage guards

`ts-turn/src/stage.rs`, at the top of `spawn_turn_stage`:

```rust
assert!(
    config.shard_count >= 1,
    "spawn_turn_stage: shard_count must be >= 1"
);
```

`ts-metrics/src/stage.rs`, at the top of `spawn_metrics_stage`:

```rust
assert!(
    config.shard_count >= 1,
    "spawn_metrics_stage: shard_count must be >= 1"
);
```

These come **before** the existing
`assert!(shard_rxs.len() == config.shard_count, ...)` so that the
"zero" case gets the more specific error message instead of a
length-mismatch one.

### Startup log

`main.rs` currently logs:

```
  pipeline: {} worker(s)
```

After the rename:

```
  pipeline: flow_shard_count={}
```

Also add (already partially present) a single "shard topology" line
that enumerates all three so operators see the whole picture at a
glance:

```
  shards: flow={} turn={} metrics={}
```

This replaces the current scattered shard logging and gives ops one
line to grep for.

### Documentation

`docs/design/pipeline-performance.md`: the living "as-built" sections
(currently use `worker_count` in the shard-key formulas and
"Recommended Evolution Path" entries) get updated to `flow_shard_count`.
The Phase 1 / Phase 2 spec and plan docs under `docs/superpowers/`
are point-in-time artifacts and are **not** rewritten — any future
reader who needs to cross-reference old field names can use `git log`.

## Testing

- Existing unit tests referencing `worker_count` are mechanically
  updated to `flow_shard_count` (both `ts-protocol/src/stage.rs` tests
  and `ts-turn/tests/integration.rs` helpers).
- New test in `ts-turn/src/stage.rs`:
  `#[should_panic(expected = "shard_count must be >= 1")]`
  asserting `spawn_turn_stage` with `shard_count = 0` panics at the
  right place.
- Equivalent test in `ts-metrics/src/stage.rs`.
- Full `cargo test --workspace` must pass; `cargo fmt --check` on
  touched files clean.

## Risks

- **Config breakage**. Any existing `config/*.toml` file with
  `pipeline.worker_count = N` will fail to deserialize after this
  change (`unknown field`). This is intentional — the whole point is
  to remove the misleading name. The `server/config/default.toml` in
  the repo is the only checked-in config; user configs are migrated by
  hand.
- **Doc drift**. Phase 1 / 2 spec/plan documents will continue to
  reference `worker_count`. They are snapshots of the state at their
  writing date; not editing them is correct but may confuse a reader
  doing free-text search. Mitigated by: (a) updating the **living**
  `pipeline-performance.md` doc, (b) a clear entry in the next phase's
  spec pointing at this rename.

## Out-of-scope work tracked for later

1. CPU-aware defaults — original Phase 4 sketch; revisit after
   production telemetry tells us whether fixed `4` is ever wrong.
2. `sweep()` called on every ingest in `ts-turn/src/stage.rs` — micro-
   optimization, not hot per current profiling.
3. Committed pcap fixture for CI so `claude_cli_messages_multi_shard_parity`
   runs unconditionally.
