# Pipeline Phase 2: Shard Turn Tracking + Metrics Aggregation

## Context

`docs/design/pipeline-performance.md` describes a 5-phase evolution of TokenScope's pipeline. Phase 1 (2026-04-13, merged) parallelized LLM extraction by running N `LlmProcessor` tasks paired 1:1 with flow workers. The remaining fan-in bottleneck is the **single `TurnTracker` + `MetricsAggregator` pair** driven by the `main.rs` `tokio::select!` loop.

This spec covers **Phase 2 only**: sharding both aggregators so `main.rs` leaves the data plane entirely and becomes a pure composition root + signal handler.

Phases 3–5 remain out of scope.

## Goal

Eliminate the second major fan-in bottleneck by running:

- **T** parallel `TurnTracker` shards, keyed by `hash(session_id) % T`
- **M** parallel `MetricsAggregator` shards, keyed by `hash(provider, model, server_ip) % M`
- **1** dedicated `spawn_storage_sink_stage` task that owns the write buffer and exclusive `StorageBackend` access

After this phase:

- Protocol parsing (N workers), LLM extraction (N llm-procs), turn tracking (T shards), and metrics aggregation (M shards) all scale independently.
- `main.rs` owns no per-event state. It creates boundary channels, spawns stages, waits for ctrl-c or pcap completion, drops the ingress sender, and awaits the sink handle.
- Default `T = M = 1` preserves Phase 1 behavior. Sharding is opt-in via config until Phase 4 introduces CPU-aware defaults.
- Schema/record-shape simplification: `LlmCall` drops `session_id` / `turn_id` / `client_kind`; `LlmTurn` adds `call_ids: Vec<String>`. The turn → calls lookup reverses direction (`LlmTurn.call_ids` references `LlmCall.id` instead of `LlmCall.turn_id` pointing at a turn). Acceptable because the project is pre-release and no API/frontend consumer depends on the removed columns.

## Approach

### Composition Root Extended

`main.rs` already created every boundary channel in Phase 1 (`raw`, `protocol_event × N`, `llm_event × 1`). Phase 2 extends the same rule: every new boundary channel — `turn_shard × T`, `metrics_shard × M`, `storage_calls`, `storage_turns`, `storage_metrics` — is created in `main.rs` and handed to the appropriate stage. No stage creates a channel that crosses its own boundary.

```text
main.rs owns every boundary channel. llm-proc fan-outs Arc<LlmCall> in three independent directions:

raw_tx ─▶ protocol ── event_txs[N] ─▶ llm stage ──┬─ turn_shard_txs[T]   ─▶ turn stage    ── turns_tx    ──┐
                                                  ├─ metrics_shard_txs[M] ─▶ metrics stage ── metrics_tx  ─┼─▶ storage sink
                                                  └─ calls_tx (Arc<LlmCall>, direct) ───────────────────── ┘
```

Each of the three arrows out of llm-proc is independent; the turn stage does NOT forward calls to the sink (it only emits `LlmTurn` rows).

### Profile Identification Moves Upstream

Today `TurnTracker::ingest(&mut call)` internally runs `ProfileRegistry::find(call)` + `profile.extract_ids(call)` to obtain the `session_id`. Phase 2 requires the `session_id` **before** ingest — it's the turn shard routing key.

The full `ProfileRegistry` + `profiles/` module (`claude_cli.rs`, `codex_cli.rs`, ...) **moves from `ts-turn` to `ts-llm`**. Rationale:

- `ts-turn::profile.rs` already `use ts_llm::model::LlmCall;` — it is more tightly coupled to the LLM domain than to the turn state machine.
- Moving (not copying) avoids source drift: adding or editing a profile stays a one-file change.
- `ts-turn` adds `pub use ts_llm::profile::*;` re-exports so external callers (`ts_turn::profiles::build_default_registry`) keep compiling without changes.
- `ts-llm` takes on responsibility for profile matching; the `LlmProcessor` becomes the natural owner of `ProfileRegistry` (one registry shared by all N llm-proc tasks via `Arc`).

Each llm-proc task, after `processor.process(event)` produces an `LlmEvent::Complete`, immediately runs `registry.find(&call)` + `profile.extract_ids(&call)` and attaches the result as `identity: Option<CallIdentity>` on the event. Downstream shards never call `find` again.

### Routing Fan-out in llm-proc

`LlmCall` is shared across shards and storage via `Arc<LlmCall>` — all three consumers are read-only, so no `Mutex` / `RwLock`. Each llm-proc task holds three independent sender sets:

- `turn_shard_txs: Vec<mpsc::Sender<IdentifiedCall>>` (length `T`) — turn shard needs both the call body (token counts, timestamps, messages for `user_input`/`final_answer_preview`) and its identity
- `metrics_shard_txs: Vec<mpsc::Sender<LlmEvent>>` (length `M`) — `LlmEvent::Complete` now carries `Arc<LlmCall>` internally
- `calls_tx: mpsc::Sender<Arc<LlmCall>>` (shared singleton into storage sink)

For each event it produces:

| Event | To calls_tx? | To turn shard? | To metrics shard? |
|---|---|---|---|
| `Start` | no | no | yes (by `hash(provider, model, server_ip) % M`) |
| `Complete` with identity | yes | yes (by `hash(session_id) % T`) | yes |
| `Complete` without identity | yes | **no** (unidentified calls do not enter turn path) | yes |

For identified `Complete` events, llm-proc wraps the call once — `let arc = Arc::new(call)` — and sends `arc.clone()` to each of the three destinations. Cloning an `Arc` is a refcount bump, not a body copy. Since nothing mutates the call after llm-proc finishes `process`, there is no visibility or ordering concern: all consumers see the same immutable snapshot.

The routing happens **inside the llm-proc task** — not in a separate router task. Keeps one extra channel hop off each event and keeps the owner of the `profile identification → routing` decision in one place.

### Per-shard Tasks

Each turn shard is one Tokio task owning a `TurnTracker`. Each metrics shard is one Tokio task owning a `MetricsAggregator`. No shared state, no locks.

Turn shard loop (packet-driven, no wall-clock ticker):

```rust
loop {
    match shard_rx.recv().await {
        Some(identified) => {
            for ev in tracker.ingest(&identified.call, &identified.identity) {
                forward(ev, &turns_tx).await;
            }
            for ev in tracker.sweep() {
                forward(ev, &turns_tx).await;
            }
        }
        None => break,
    }
}
for ev in tracker.flush_all() { forward(ev, &turns_tx).await; }
```

The tracker reads `&LlmCall` (no mutation). As each call is ingested into a turn, the tracker pushes `call.id.clone()` onto the turn's `call_ids` vector — that is the only persisted back-reference, replacing the Phase-1 `LlmCall.turn_id` column.

Metrics shard loop (also packet-driven):

```rust
loop {
    match shard_rx.recv().await {
        Some(event) => {
            for m in aggregator.process(&event) { metrics_tx.send(m).await?; }
        }
        None => break,
    }
}
for m in aggregator.flush_all() { metrics_tx.send(m).await?; }
```

**Packet-time clock rule:** `TurnTracker.virtual_now_us` is advanced by `ingest` from the call's timestamps; `sweep` uses that virtual clock. `MetricsAggregator` window boundaries are derived from event timestamps. No Tokio `interval` drives either — pcap replay must be deterministic, and idle wall-clock time carries no semantic meaning. The only wall-clock ticker in the pipeline is the storage sink's write-buffer flush interval (pure I/O batching, does not affect final DB contents).

### Storage Sink Stage

A single `spawn_storage_sink_stage` task owns the write buffer and exclusive access to the `StorageBackend`. It consumes three channels concurrently (`LlmCall`, `LlmTurn`, `LlmMetric`), batches into the write buffer, and flushes on either a wall-clock interval or final channel close.

This task is the **only stage that returns a `JoinHandle`**. `main.rs` awaits it after dropping `raw_tx` to ensure all data is persisted before process exit. All other stages exit via drop cascade without explicit join.

## Changes

### `ts-llm` — receive ProfileRegistry; extend events; route

- Move `ts-turn/src/profile.rs` → `ts-llm/src/profile.rs`.
- Move `ts-turn/src/profiles/` (claude_cli.rs, codex_cli.rs, mod.rs) → `ts-llm/src/profiles/`.
- `ts-llm/src/lib.rs` adds `pub mod profile; pub mod profiles;` and re-exports.
- `ts-llm::model` changes:
  - **`LlmCall` drops three fields**: `session_id`, `turn_id`, `client_kind`. These were only ever written by `TurnTracker::ingest` for storage-FK purposes. After Phase 2 the back-reference lives on `LlmTurn.call_ids`.
  - Add:
    ```rust
    #[derive(Clone, Debug)]
    pub struct CallIdentity {
        pub profile_name: &'static str,
        pub client_kind: String,
        pub session_id: String,
        pub turn_id_hint: Option<String>,   // e.g. Codex turn id from request body
    }

    #[derive(Debug)]
    pub struct IdentifiedCall {
        pub call: Arc<LlmCall>,
        pub identity: CallIdentity,
    }
    ```
- `LlmEvent::Complete` changes from tuple variant `Complete(LlmCall)` to struct variant holding an `Arc`:
  ```rust
  pub enum LlmEvent {
      Start(LlmCallStart),                                    // unchanged — small value type
      Complete { call: Arc<LlmCall>, identity: Option<CallIdentity> },
  }
  ```
  `Start` stays as `LlmCallStart` (a lightweight begin-of-call marker used by metrics for concurrency counting — not the same shape as the completed `LlmCall`).
- `spawn_llm_stage` new signature:
  ```rust
  pub fn spawn_llm_stage(
      event_rxs: Vec<mpsc::Receiver<ProtocolEvent>>,
      turn_shard_txs: Vec<mpsc::Sender<IdentifiedCall>>,
      metrics_shard_txs: Vec<mpsc::Sender<LlmEvent>>,
      calls_tx: mpsc::Sender<Arc<LlmCall>>,
      registry: Arc<ProfileRegistry>,
      metrics_sys: &mut MetricsSystem,
  );
  ```
- Each llm-proc task gets clones of all three sender sets + a shared `Arc<ProfileRegistry>`. Routing logic lives inside the task after `processor.process(event)`.
- **Optional observability**: per-llm-proc `LlmCallsUnidentified` metric (count of Complete calls with `identity = None`). Implementation plan decides whether to include now.

### `ts-turn` — re-export; new stage; slimmed tracker

- Delete `ts-turn/src/profile.rs` and `ts-turn/src/profiles/`.
- `ts-turn/src/lib.rs` re-exports: `pub use ts_llm::profile::{ClientProfile, ExtractedIds, ProfileRegistry}; pub use ts_llm::profiles::build_default_registry;` — callers continue using `ts_turn::profiles::build_default_registry`.
- `TurnTracker::ingest` signature changes:
  ```rust
  pub fn ingest(&mut self, call: &LlmCall, identity: &CallIdentity) -> Vec<TurnEvent>;
  ```
  Immutable borrow — no more mutation of the call. The tracker:
  - Reads fields it needs (`id`, `request_time`, token counts, provider, model, response body for final text extraction, etc.)
  - Appends `call.id.clone()` to the current turn's `call_ids`
  - Does not run `registry.find` / `extract_ids` internally. The registry remains a field of `TurnTracker` because per-profile methods (`is_user_turn_start`, `extract_user_input`, `extract_assistant_text`, `subagent`) are still needed; the registry is looked up by `identity.profile_name` (O(profiles count), trivially small) — not by scanning every profile's `matches`.
- **`LlmTurn` adds** `pub call_ids: Vec<String>` — ordered list of `LlmCall.id` values that belong to this turn. Tracker appends in ingest order. `final_call_id` stays (it is the terminating response — semantic, not just membership).
- New `ts-turn::spawn_turn_stage`:
  ```rust
  pub struct TurnStageConfig {
      pub shard_count: usize,      // default 1
      pub tracker: TrackerConfig,
  }

  pub fn spawn_turn_stage(
      config: TurnStageConfig,
      shard_rxs: Vec<mpsc::Receiver<IdentifiedCall>>,
      turns_tx: mpsc::Sender<LlmTurn>,
      registry: Arc<ProfileRegistry>,
      metrics_sys: &mut MetricsSystem,
  );
  ```
- Validates `shard_rxs.len() == config.shard_count`.
- Each shard task owns a fresh `TurnTracker::new(registry.clone(), config.tracker.clone())`.

### `ts-metrics` — new stage

- New `ts-metrics::spawn_metrics_stage`:
  ```rust
  pub struct MetricsStageConfig {
      pub shard_count: usize,      // default 1
  }

  pub fn spawn_metrics_stage(
      config: MetricsStageConfig,
      shard_rxs: Vec<mpsc::Receiver<LlmEvent>>,
      metrics_tx: mpsc::Sender<LlmMetric>,
      metrics_sys: &mut MetricsSystem,
  );
  ```
- Validates `shard_rxs.len() == config.shard_count`.
- Each shard task owns a fresh `MetricsAggregator::new()`.
- `MetricsAggregator::process` signature unchanged (still takes `&LlmEvent`). The `Complete` variant is now struct-shaped; internal pattern matching updated.

### `ts-storage` — new sink stage

- New `ts-storage::spawn_storage_sink_stage`:
  ```rust
  pub struct StorageSinkConfig {
      pub channel_capacity: usize,         // default 4096 (applies to each of the three rx)
      pub flush_interval_ms: u64,          // default 1000
      pub write_buffer: WriteBufferConfig, // existing type, relocated if necessary
  }

  pub fn spawn_storage_sink_stage(
      config: StorageSinkConfig,
      calls_rx: mpsc::Receiver<Arc<LlmCall>>,
      turns_rx: mpsc::Receiver<LlmTurn>,
      metrics_rx: mpsc::Receiver<LlmMetric>,
      backend: Arc<dyn StorageBackend>,
      metrics_sys: &mut MetricsSystem,
  ) -> JoinHandle<()>;
  ```
- Loop structure:
  ```rust
  let mut flush_tick = tokio::time::interval(Duration::from_millis(config.flush_interval_ms));
  loop {
      tokio::select! {
          Some(call) = calls_rx.recv() => buffer.push_call(call),   // Arc<LlmCall>, serialized from &LlmCall on flush
          Some(turn) = turns_rx.recv() => buffer.push_turn(turn),
          Some(m) = metrics_rx.recv() => buffer.push_metric(m),
          _ = flush_tick.tick() => buffer.flush(&backend).await,
          else => break,  // all three rx closed
      }
  }
  buffer.flush(&backend).await;  // final flush
  ```
- **`WriteBuffer` signature**: `push_call` takes `Arc<LlmCall>`. Backend `insert_calls` takes `&[Arc<LlmCall>]` (or iterates over `&LlmCall` via `as_ref`). No body copy; the Arc's inner data is dropped once the final insert completes.
- **`llm_calls` schema drops three columns**: `session_id`, `turn_id`, `client_kind`. `llm_turns` gains `call_ids` (stored as JSON array in DuckDB, native array in PG/ClickHouse — same encoding pattern as existing `models_used` / `subagents_used`). `dbview/calls.rs` drops the corresponding print lines.
- Only Phase-2 stage that returns a `JoinHandle`.

### `app/tokenscope/src/main.rs` — composition root rewrite

End-to-end wiring:

```rust
let worker_count = config.pipeline.worker_count;
let turn_shards = config.turn.shard_count;       // default 1
let metrics_shards = config.metrics.shard_count; // default 1
let queue_size = 4096;

// Boundary channels
let (raw_tx, raw_rx) = mpsc::channel(queue_size);
let (event_txs, event_rxs) = make_channel_pairs(worker_count, queue_size);
let (turn_shard_txs, turn_shard_rxs) = make_channel_pairs(turn_shards, queue_size);
let (metrics_shard_txs, metrics_shard_rxs) = make_channel_pairs(metrics_shards, queue_size);
let (calls_tx, calls_rx) = mpsc::channel::<Arc<LlmCall>>(config.storage.sink.channel_capacity);
let (turns_tx, turns_rx) = mpsc::channel(config.storage.sink.channel_capacity);
let (metrics_tx, metrics_rx) = mpsc::channel(config.storage.sink.channel_capacity);

// Shared state
let registry = Arc::new(build_default_registry());

// Stages
ts_protocol::spawn_protocol_stage(protocol_cfg, raw_rx, event_txs, &mut metrics_sys);
ts_llm::spawn_llm_stage(
    event_rxs,
    turn_shard_txs,
    metrics_shard_txs,
    calls_tx,
    registry.clone(),
    &mut metrics_sys,
);
ts_turn::spawn_turn_stage(turn_cfg, turn_shard_rxs, turns_tx, registry.clone(), &mut metrics_sys);
ts_metrics::spawn_metrics_stage(metrics_cfg, metrics_shard_rxs, metrics_tx, &mut metrics_sys);
let sink_handle = ts_storage::spawn_storage_sink_stage(
    sink_cfg, calls_rx, turns_rx, metrics_rx, backend, &mut metrics_sys,
);

// Hand raw_tx clones to capture sources, then drop main's own reference
// so the cascade can start as soon as sources finish (pcap) or ctrl-c cancels them.
for source in capture_sources { spawn_source(source, raw_tx.clone(), cancel.clone()); }
drop(raw_tx);

// Ctrl-c cancels capture sources; pcap natural end occurs when sources drop their clones.
// Either way, the drop cascade flows to the sink.
tokio::select! {
    _ = tokio::signal::ctrl_c() => { cancel.cancel(); }
    res = &mut sink_handle => { res?; return Ok(()); }
}
sink_handle.await?;
```

- Delete the Phase-1 `llm_rx.recv()` loop, the local `TurnTracker`, the local `MetricsAggregator`, and all write-buffer forwarding code from `main.rs`.
- `main.rs` now contains no domain logic beyond composition and signal handling.

### Configuration

New config fields (all with defaults preserving Phase 1 behavior):

```toml
[turn]
shard_count = 1
# TrackerConfig fields unchanged

[metrics]
shard_count = 1

[storage.sink]
channel_capacity = 4096
flush_interval_ms = 1000
# WriteBufferConfig fields unchanged
```

Removed: `turn.sweep_interval_ms` (if previously present — sweep is packet-driven), `metrics.flush_interval_ms` (window close is packet-driven).

## Correctness Invariants

| Invariant | How it's preserved |
|---|---|
| Same-flow event order | Phase 1 property unchanged (dispatcher pins flow → worker → llm-proc) |
| Same-session turn consistency | `IdentifiedCall` hashed by `session_id` → same turn shard → single `TurnTracker` serial consumer |
| Same-dimension metrics consistency | `LlmEvent` hashed by `(provider, model, server_ip)` → same metrics shard |
| Profile identification happens once per call | llm-proc runs `find` + `extract_ids` after `process`; shards consume result via `identity` |
| Unidentified calls still reach `llm_calls` | llm-proc forwards `Arc<LlmCall>` to `calls_tx` unconditionally; only turn path is skipped |
| Turn → calls back-reference preserved | `LlmTurn.call_ids` populated by tracker in ingest order (replaces `LlmCall.turn_id` FK) |
| No hidden mutation of shared `LlmCall` | `Arc<LlmCall>` is immutable; all three consumers (sink, turn shard, metrics shard) only read |
| pcap replay determinism | Sweep + metrics windows driven by packet timestamps (`virtual_now_us`); storage-sink flush timing does not affect final DB content |
| Shutdown no-loss | drop cascade + per-stage `flush_all` + `main.rs` awaits `sink_handle` |
| Backpressure | Every channel bounded (default 4096); no unbounded buffers anywhere |

## Correctness Checks (Acceptance)

1. **Existing unit tests pass after type updates.** `ts-turn` / `ts-metrics` / `ts-llm` suites only need minimal follow-on edits for `LlmEvent::Complete` struct variant and new `TurnTracker::ingest` signature.
2. **`ts-turn/tests/integration.rs` updated.** Run with `worker_count=1, turn_shards=1, metrics_shards=1`. Assertions on turn counts unchanged. Wire through `spawn_turn_stage` + `spawn_metrics_stage` + `spawn_storage_sink_stage` using an in-memory backend.
3. **New driver tests:**
   - `spawn_turn_stage` with `T=4`, handcrafted `IdentifiedCall` stream covering multiple `session_id` values. Verify finalized `LlmTurn` set matches single-shard equivalent.
   - `spawn_metrics_stage` with `M=4`, handcrafted `LlmEvent` stream covering multiple `(provider, model, server_ip)` dimensions. Verify aggregated `LlmMetric` set matches single-shard equivalent.
   - `spawn_storage_sink_stage` driver test: three concurrent senders + clean shutdown. Verify all records persisted, no drops.
4. **Pcap replay parity (`T=M=1`).** `llm_calls` / `llm_turns` / `llm_metrics` row *content* identical to Phase-1 main. Row order in the write buffer and `LlmCall.id` values may differ because per-processor counters are unchanged from Phase 1 (already flagged as non-contract).
5. **Multi-shard replay (`T=M=4`).** Same fixture produces the same **set** of records as `T=M=1`. Compare on content equivalence, not insertion order.
6. **Graceful shutdown.** Ctrl-c on live capture and pcap natural termination both exit cleanly: no orphaned tasks, no pending rows in write buffer.

## Risks

- **`LlmEvent::Complete` struct-variant + `Arc<LlmCall>` breaking change.** All pattern-matching call sites must be updated in one PR: `ts-llm::processor`, `ts-metrics::aggregator`, `ts-turn/tests/integration.rs`, `main.rs` (deleted anyway). Flag for reviewer awareness in implementation plan.
- **`LlmCall` field removal + `LlmTurn.call_ids` addition.** Storage schema (`llm_calls` drops 3 columns, `llm_turns` gains `call_ids`), `dbview` output, and any test fixtures referencing the removed fields all update in the same PR. The project is pre-release — no data migration required, but reviewers should confirm no silent consumers remain.
- **`ProfileRegistry` relocation is a large file move.** `ts-turn` re-exports minimize external API churn but internal `use` paths shift significantly. PR size is moderate; no behavior change.
- **`TurnTracker::ingest` signature change.** Internal change only; external callers go through `spawn_turn_stage` post-Phase-2.
- **Hot single session.** A single very long session concentrates all its turn work on one shard. Same pattern as Phase 1's hot flow. Mitigation deferred to Phase 4 (CPU-aware defaults).
- **Storage sink remains a single task.** Three channels fan in but the task does I/O only, not CPU. Phase 5 will split read/write pools.
- **Unidentified-call observability gap.** Today unidentified calls are invisible. Recommend adding `LlmCallsUnidentified` per-llm-proc metric as part of this phase (or the next) — helps diagnose profile coverage.

## Out of Scope

- CPU-aware defaults for `shard_count` (Phase 4)
- Per-source ingress isolation (Phase 3)
- Storage read/write pool split (Phase 5)
- `LlmMetric` schema or record-shape changes (only `LlmCall` loses 3 fields and `LlmTurn` gains `call_ids` — see Goal section)
- `FlowDispatcher` shard-key change
- Profile matching logic changes (only the module location moves)
- Reintroducing the Phase-1-deleted summed `call_count` metric
