# Pipeline Phase 1: Parallelize LLM Extraction

## Context

`docs/design/pipeline-performance.md` describes a 5-phase evolution of TokenScope's pipeline toward a multicore-efficient topology. This spec covers **Phase 1 only**: moving LLM extraction off the single main-loop lane and onto per-worker instances.

Phases 2–5 are out of scope for this spec and will be addressed independently.

## Goal

Remove the first major fan-in bottleneck — the single `LlmProcessor` in `app/tokenscope/src/main.rs` that serializes all worker output — by running **N parallel `LlmProcessor` instances**, one per flow worker, paired 1:1.

After this phase:

- protocol parsing (N workers) and LLM extraction (N processors) both scale with `worker_count`
- the main loop still owns `TurnTracker` + `MetricsAggregator` (single instance, removed in Phase 2)
- no schema, record shape, or tagging change

## Approach: N paired LlmProcessor tasks, with `main.rs` as composition root

An earlier reading of the parent doc suggested *embedding* `LlmProcessor` directly inside `FlowWorker`. That implementation would require `ts-protocol` to depend on `ts-llm`, which would invert the existing dependency (`ts-llm → ts-protocol`) and force either a crate merge or a dependency-inversion refactor — a larger change than Phase 1 warrants.

Instead, the correctness-critical property of `LlmProcessor` is that its `pending: HashMap<FlowKey, PendingCall>` state must see every event for a given flow in order. Since `FlowDispatcher` already pins each flow to one worker by `hash(flow_key) % worker_count`, a **1:1 pairing of worker ↔ `LlmProcessor` task** preserves that invariant without moving the processor inside the worker.

`main.rs` takes on the role of **composition root**: it creates every channel that crosses a stage boundary — the `RawPacket` channel (capture → protocol stage), the N `ProtocolEvent` channels (protocol stage → llm stage), and the single `LlmEvent` channel (llm stage → main loop) — and hands sender/receiver ends to the appropriate stage. Stages only own their **internal** channels (e.g., `ts-protocol`'s dispatcher → worker `ParsedPacket` channels, which are an implementation detail and stay hidden). This rule — *boundary channels in `main.rs`, internal channels in the owning stage* — keeps the topology readable in one place and establishes the "stage" wiring pattern that Phase 2 (`ts-turn::spawn_turn_stage`, `ts-metrics::spawn_metrics_stage`) will reuse unchanged.

```text
main.rs owns every boundary channel:

           Sender<RawPacket>                                                      Receiver<LlmEvent>
             (to capture)                                                         (main loop reads)
                 ▲                                                                       ▲
                 │                                                                       │
       ┌─────────┴─────────┐                                                             │
       │     main.rs       │  hands out channel endpoints below                          │
       └─────────┬─────────┘                                                             │
                 │                                                                       │
                 ▼                                                                       │
   Receiver<RawPacket>         N × Sender<ProtocolEvent>      N × Receiver<ProtocolEvent>│ Sender<LlmEvent>
┌─────────────────────────────────────────┐              ┌────────────────────────────┐  │
│ ts-protocol::spawn_protocol_stage       │              │ ts-llm::spawn_llm_stage    │  │
│  dispatcher + workers 0..N-1            │──────────────▶│  llm-proc 0..N-1          │──┘
│  (ParsedPacket channels internal only)  │              │                            │
└─────────────────────────────────────────┘              └────────────────────────────┘
```

Naming rationale: after this refactor, the function in `ts-protocol` only spawns the dispatcher + N flow workers — it is no longer "the pipeline". Renaming to `spawn_protocol_stage` makes ownership honest and pairs symmetrically with `ts-llm::spawn_llm_stage` (and, in Phase 2, `ts-turn::spawn_turn_stage` / `ts-metrics::spawn_metrics_stage`). `main.rs` is the sole orchestrator — "pipeline" is a main-level concept composed out of stages.

Trade-off accepted: one extra channel hop per event (worker → llm-proc). LLM extraction itself is ms-scale JSON work, so a µs-scale hop is noise. In exchange, the `ts-protocol` ↔ `ts-llm` boundary (transport vs semantics — enshrined in `CLAUDE.md`) stays intact, and both stages become independently testable with driver channels.

## Changes

### `ts-protocol` — rename + new signature

- Rename `ts_protocol::pipeline::start_pipeline` → `ts_protocol::spawn_protocol_stage`. The function also moves out of `pipeline.rs` into a new file (e.g., `stage.rs`) so the module name matches the concept; `pipeline.rs` can be deleted since its only resident is the renamed function. `PipelineConfig` is renamed to `ProtocolStageConfig` and colocated.
- New signature — both boundary channels are **injected**, and the function returns nothing:
  ```rust
  pub fn spawn_protocol_stage(
      config: ProtocolStageConfig,
      raw_rx: mpsc::Receiver<RawPacket>,              // ingress boundary: created by main.rs
      event_txs: Vec<mpsc::Sender<ProtocolEvent>>,    // egress boundary: length == config.worker_count
      metrics_sys: &mut MetricsSystem,
  )
  ```
- Each worker `i` clones its `Sender<ProtocolEvent>` from `event_txs[i]`. The function no longer creates event channels, no longer creates the raw-packet channel, and no longer owns the `dispatcher_queue_size` / `output_queue_size` configs for those boundaries.
- Internal `ParsedPacket` channels (dispatcher → worker) remain owned by this function — they are implementation detail, not boundary wiring.
- `PipelineHandle` is deleted.
- `FlowWorker::new` signature is unchanged. `FlowDispatcher` logic is unchanged.
- `ProtocolStageConfig.output_queue_size` and `ProtocolStageConfig.dispatcher_queue_size` are removed (boundary channel sizing is the caller's concern; `worker_queue_size` stays since it bounds an internal channel).
- Validation: on entry, assert that `event_txs.len() == config.worker_count`. Length mismatch is a wiring bug, not a runtime condition.

### `ts-llm` — new module

- New public function at the crate root (re-exported from `lib.rs`):
  ```rust
  pub fn spawn_llm_stage(
      event_rxs: Vec<mpsc::Receiver<ProtocolEvent>>,
      output_tx: mpsc::Sender<LlmEvent>,
      metrics_sys: &mut MetricsSystem,
  )
  ```
- For each `event_rx` in the vector, `spawn_llm_stage` spawns one Tokio task that:
  1. owns a fresh `LlmProcessor`
  2. reads `ProtocolEvent`s until the channel closes
  3. calls `processor.process(event)` and forwards each produced `LlmEvent` into a clone of `output_tx`
  4. exits when its input closes, dropping its `output_tx` clone
- No join handle is returned. Shutdown is drop-based: when every worker finishes and drops its `Sender<ProtocolEvent>`, every `event_rx` closes, every llm-proc exits, every `output_tx` clone drops, and the shared `output_rx` observes `None`. This matches the existing cascade.
- `LlmProcessor::new` and the rest of `ts-llm` are unchanged. Verified side-effect-free: no shared `Arc`/`Mutex`, no global state.
- Optionally (nice-to-have, not required): register a per-task `worker.{i}` slot in `metrics_sys` so future LLM-stage metrics (e.g., `LlmCallsCompleted`, `LlmCallsPending`) can slot in without changing this signature. Implementation plan decides whether to include this now or defer.

### `app/tokenscope/src/main.rs`

New wiring, end-to-end:

```rust
// 1. Compose channels (composition root — every boundary channel lives here)
let worker_count = config.pipeline.worker_count;
let queue_size = 4096;  // matches current PipelineConfig::default().output_queue_size
                        // implementation plan may promote this to a config field
let (raw_tx, raw_rx) = mpsc::channel(queue_size);
let mut event_txs = Vec::with_capacity(worker_count);
let mut event_rxs = Vec::with_capacity(worker_count);
for _ in 0..worker_count {
    let (tx, rx) = mpsc::channel(queue_size);
    event_txs.push(tx);
    event_rxs.push(rx);
}
let (llm_tx, mut llm_rx) = mpsc::channel(queue_size);

// 2. Start stages — both receive their channel endpoints; neither returns one
ts_protocol::spawn_protocol_stage(protocol_cfg, raw_rx, event_txs, &mut metrics_sys);
ts_llm::spawn_llm_stage(event_rxs, llm_tx, &mut metrics_sys);

// raw_tx is handed to capture sources (same as today)

// 3. Main loop consumes LlmEvent directly
loop {
    tokio::select! {
        maybe_event = llm_rx.recv() => {
            match maybe_event {
                Some(llm_event) => { /* TurnTracker + MetricsAggregator — unchanged */ }
                None => break,
            }
        }
        _ = tokio::signal::ctrl_c() => { cancel.cancel(); break; }
    }
}
```

- Delete the outer `for llm_event in llm.process(event)` loop and the `ProtocolEvent` intermediate type in the select arm; the `LlmEvent` now arrives directly.
- Delete the local `LlmProcessor` and its `call_count()` / `pending_count()` shutdown log lines (see below).
- `TurnTracker` + `MetricsAggregator` logic and the write-buffer forwarding inside the `Some(llm_event) => ...` arm are unchanged.

### Startup-log `call_count` handling

`llm.call_count()` at shutdown (`main.rs:408-412`) currently reports total completed calls from the single `LlmProcessor`. After this change there are `N` processors, each with a private counter.

Resolution: **delete** the `call_count()` + `pending_count()` references from the shutdown log. The completed-call count is already visible via write-buffer logs and storage. Pending-count observability, if still wanted, will be reintroduced later through `MetricsSystem` as a summed internal metric — but that is explicitly **not** required for this phase.

### No config toggle

Cut over cleanly. Rationale:

- Confidence comes from existing unit tests in `ts-llm/src/processor.rs` and pcap-replay fixtures
- The change is a refactor, not a semantic change — record shapes, tagging, and schema are identical
- Keeping dual paths in `main.rs` for one release would be more bug-surface than the rollback insurance is worth

## Correctness Invariants

| Invariant | How it's preserved |
|---|---|
| Same-flow events stay ordered | `FlowDispatcher` already pins flow → worker; per-worker channel is single-producer-single-consumer |
| Request/response/SSE correlation | A flow's events all reach the same `LlmProcessor` because worker and llm-proc are 1:1 |
| Shutdown cascade | main.rs drops `raw_tx` after capture sources exit → `raw_rx` (held by dispatcher) closes → dispatcher exits, dropping per-worker `ParsedPacket` senders → workers exit, dropping their `event_tx` clones → per-worker `ProtocolEvent` channels close → llm-proc tasks exit, dropping their `output_tx` clones → shared `LlmEvent` rx observes `None` → main loop exits |
| Backpressure | `main.rs` allocates every channel with bounded capacity (initially the current default `4096`). Worker→llm-proc channels bound the per-worker stream; the shared LlmEvent channel bounds the N llm-procs → main loop fan-in. No unbounded channels. |

## Correctness Checks (Acceptance)

1. **Existing unit tests pass unchanged.** `ts-llm` / `ts-protocol` / `ts-turn` test suites are untouched by the refactor.
2. **New `spawn_llm_stage` unit test.** Driver test that feeds a handcrafted `ProtocolEvent` stream (request → SSE events → response) into one of N receivers and verifies the expected `LlmEvent::Start` + `LlmEvent::Complete` arrive on the output channel. Covers: (a) single-receiver case, (b) N=4 receivers with different flow_keys, (c) clean task exit when input channel drops.
3. **Pcap replay parity.** Replaying fixtures with `worker_count = 1` produces the same `llm_calls`, `llm_turns`, `llm_metrics` row content as pre-change `main`. (With `worker_count = 1` there is exactly one llm-proc, mirroring today's topology.)
4. **Multi-worker replay.** Replaying the same fixture with `worker_count = 4` produces the same **set** of `llm_calls` rows (row order in the write buffer may differ; `id` embeds `request_time` and a per-processor counter, so ids may differ between runs — compare on content, not `id`).
5. **Graceful shutdown.** Ctrl+C on a live capture exits cleanly with no orphaned tasks and no dropped in-flight events beyond what the bounded channel capacity allows.

### Note on `LlmCall.id` stability across shard counts

Today, `LlmCall.id = format!("call-{request_time:016x}-{call_count:04x}")` where `call_count` is the single processor's monotonic counter. With N processors, each has its own counter — so two runs of the same pcap with different `worker_count` will produce the same call *contents* but different `id` values for the same call.

This is acceptable because `id` is a debug identifier, not referenced across tables. `llm_turns` identifies its members by `(session_id, turn_id)`, not by `LlmCall.id`. Flag in the implementation plan as "id format is not a wire contract; note in the changelog if anyone depends on it."

## Risks

- **Hot single flow.** A single very hot SSE flow now concentrates *both* protocol parsing and JSON extraction on one worker+llm-proc pair. Mitigation: Phase 4 raises `worker_count` default to CPU-aware. Out of scope here.
- **Task count grows.** Worker count `N` now means `2N` pipeline tasks instead of `N + 1`. At `worker_count ≤ 16` (the realistic cap), this is still trivial for Tokio. No action.
- **Per-worker pending state at shutdown.** Each `LlmProcessor` holds its own `pending` map. Unmatched requests at shutdown are dropped, same as today — just split across N maps. No change in observable behavior.
- **Rename + signature breaking change.** `start_pipeline` → `spawn_protocol_stage`, `PipelineConfig` → `ProtocolStageConfig`, the `raw_rx` ingress is now injected by the caller, and both `output_queue_size` and `dispatcher_queue_size` are removed. The two callers today — `app/tokenscope/src/main.rs` and `server/ts-turn/tests/integration.rs` — are updated in the same PR. Flag in the implementation plan for reviewer awareness.

## Out of Scope

- Sharding `TurnTracker` — Phase 2
- Sharding `MetricsAggregator` — Phase 2
- Per-source ingress isolation — Phase 3
- CPU-aware defaults — Phase 4
- Storage read/write pool split — Phase 5
- Any change to `LlmCall` / `LlmTurn` / `LlmMetric` schemas or record shapes
- Any change to the `FlowDispatcher` shard key (still `hash(flow_key) % worker_count`)
- Re-introducing a summed `call_count` internal metric
