# Internal Metrics Pipeline Observability

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every stage transition in the pipeline observable via internal metrics — track data produced, filtered, and dropped at each stage so the full funnel from capture to storage is visible.

**Architecture:** Currently only Capture and Protocol stages actually increment internal metrics. LLM, Turn, Metrics-aggregation, and Storage stages are black boxes. We will: (1) clean up the metric enum — rename 2, delete 4, add 11 new variants, add 2 new groups; (2) thread `MetricsSystem`/`MetricsWorker` into the 4 uninstrumented stages; (3) add `.inc()` calls at every filter/drop/produce point.

**Tech Stack:** Rust, `ts-common::internal_metrics` (AtomicU64-based counters), tokio mpsc channels.

---

## File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `server/ts-common/src/internal_metrics.rs` | Metric enum, MetricGroup enum, define_metrics! |
| Modify | `server/ts-protocol/src/flow.rs` | Use renamed `DispatcherPacketsRouted` / `DispatcherHeartbeatsDropped` |
| Modify | `server/ts-protocol/src/stage.rs` | Register workers with renamed metrics |
| Modify | `server/ts-llm/src/processor.rs` | Accept `MetricsWorker`, increment LLM-stage counters |
| Modify | `server/ts-llm/src/stage.rs` | Accept `MetricsSystem`, register workers, pass to processor, count identified/unidentified |
| Modify | `server/ts-turn/src/tracker.rs` | Accept `MetricsWorker`, count ingested/auxiliary/completed/timed-out |
| Modify | `server/ts-turn/src/stage.rs` | Register workers with Turn metrics, pass to tracker |
| Modify | `server/ts-metrics/src/aggregator.rs` | Accept `MetricsWorker`, count events received and windows flushed |
| Modify | `server/ts-metrics/src/stage.rs` | Register workers with Metrics metrics, pass to aggregator |
| Modify | `server/ts-storage/src/buffer.rs` | Accept `MetricsWorker`, count buffered/flushed/errors |
| Modify | `server/ts-storage/src/sink.rs` | Accept `MetricsWorker` or register workers, pass to buffers |
| Modify | `server/app/tokenscope/src/pipeline.rs` | Wire MetricsSystem into LLM and storage stages |
| Modify | `server/app/tokenscope/src/main.rs` | No changes expected (MetricsSystem already passed through) |
| Modify | `server/app/tokenscope/tests/pipeline_e2e.rs` | Adapt to any signature changes |

---

### Task 1: Update Metric enum and MetricGroup

**Files:**
- Modify: `server/ts-common/src/internal_metrics.rs:28-159` (MetricGroup + define_metrics!)

This is the single source of truth. All downstream tasks depend on these names.

- [ ] **Step 1: Add `Llm` and `Turn` to MetricGroup**

In `server/ts-common/src/internal_metrics.rs`, replace the `MetricGroup` enum, `ORDER`, and `as_str`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetricGroup {
    Capture,
    Protocol,
    Llm,
    Turn,
    Metrics,
    Storage,
}

impl MetricGroup {
    pub const ORDER: &[MetricGroup] = &[
        MetricGroup::Capture,
        MetricGroup::Protocol,
        MetricGroup::Llm,
        MetricGroup::Turn,
        MetricGroup::Metrics,
        MetricGroup::Storage,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            MetricGroup::Capture => "capture",
            MetricGroup::Protocol => "protocol",
            MetricGroup::Llm => "llm",
            MetricGroup::Turn => "turn",
            MetricGroup::Metrics => "metrics",
            MetricGroup::Storage => "storage",
        }
    }
}
```

- [ ] **Step 2: Replace the `define_metrics!` invocation**

Replace the entire `define_metrics! { ... }` block (lines 124-159) with:

```rust
define_metrics! {
    // -- Capture --
    CapturePacketsReceived   => { kind: Counter, group: Capture,  short: "pkts_recv"       },
    CapturePacketsDropped    => { kind: Counter, group: Capture,  short: "pkts_drop"       },
    CaptureBatchesReceived   => { kind: Counter, group: Capture,  short: "batches_recv"    },
    CaptureBatchesDropped    => { kind: Counter, group: Capture,  short: "batches_drop"    },
    CaptureHeartbeatsEmitted => { kind: Counter, group: Capture,  short: "hb_emit"         },

    // -- Protocol (dispatcher + flow workers) --
    DispatcherPacketsRouted      => { kind: Counter, group: Protocol, short: "dispatched"     },
    DispatcherHeartbeatsDropped  => { kind: Counter, group: Protocol, short: "hb_drop"        },
    NetPacketsParsed             => { kind: Counter, group: Protocol, short: "net_parsed"     },
    HttpRequestsParsed           => { kind: Counter, group: Protocol, short: "http_req"       },
    HttpResponsesParsed          => { kind: Counter, group: Protocol, short: "http_resp"      },
    SseEventsParsed              => { kind: Counter, group: Protocol, short: "sse_events"     },
    HttpResyncEvents             => { kind: Counter, group: Protocol, short: "http_resync"    },
    FlowsTimedOut                => { kind: Counter, group: Protocol, short: "flows_timeout"  },

    // -- LLM extraction --
    LlmRequestsDetected     => { kind: Counter, group: Llm, short: "req_detected"    },
    LlmRequestsIgnored      => { kind: Counter, group: Llm, short: "req_ignored"     },
    LlmCallsCompleted       => { kind: Counter, group: Llm, short: "calls_completed" },
    LlmCallsIdentified      => { kind: Counter, group: Llm, short: "calls_identified"},
    LlmCallsUnidentified    => { kind: Counter, group: Llm, short: "calls_unident"   },
    LlmResponsesOrphaned    => { kind: Counter, group: Llm, short: "resp_orphaned"   },
    LlmPendingExpired       => { kind: Counter, group: Llm, short: "pending_expired" },

    // -- Turn tracking --
    TurnCallsIngested        => { kind: Counter, group: Turn, short: "calls_ingested" },
    TurnCallsAuxiliary       => { kind: Counter, group: Turn, short: "calls_aux"      },
    TurnsCompleted           => { kind: Counter, group: Turn, short: "completed"      },
    TurnsTimedOut            => { kind: Counter, group: Turn, short: "timed_out"      },

    // -- Metrics aggregation --
    MetricsEventsReceived    => { kind: Counter, group: Metrics, short: "events_recv"    },
    MetricsWindowsFlushed    => { kind: Counter, group: Metrics, short: "windows_flush"  },

    // -- Storage --
    StorageRecordsBuffered   => { kind: Counter, group: Storage, short: "buffered"       },
    StorageRecordsFlushed    => { kind: Counter, group: Storage, short: "flushed"        },
    StorageFlushErrors       => { kind: Counter, group: Storage, short: "flush_errors"   },

    // -- Queue depths (gauges) --
    DispatcherQueueDepth     => { kind: Gauge, group: Protocol, short: "q.dispatcher"   },
    MetricsQueueDepth        => { kind: Gauge, group: Metrics,  short: "q.metrics"      },
    StorageQueueDepth        => { kind: Gauge, group: Storage,  short: "q.storage"      },
}
```

- [ ] **Step 3: Update tests that reference old variant names**

In the same file, update the test functions:

- `test_worker_registration_and_aggregation` — no changes (uses `NetPacketsParsed`, `HttpRequestsParsed` which are unchanged).
- `test_monitor_total_and_delta` — replace `Metric::LlmCallsExtracted` with `Metric::LlmCallsCompleted` (3 occurrences).
- `test_format_grouped` — replace `Metric::StorageRecordsBuffered` with `Metric::StorageRecordsBuffered` (unchanged), check that group name assertions match new group strings: `"capture"` stays, `"pipeline"` → `"protocol"`, `"storage"` stays. In the test, the `grouped[1].0` assertion should check `"protocol"` instead of `"pipeline"`.

```rust
// In test_format_grouped:
assert_eq!(grouped[1].0, "protocol");
```

- [ ] **Step 4: Verify compilation of ts-common**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check -p ts-common`

Expected: compiles with no errors. Downstream crates will fail (they reference old names) — that is expected and fixed in Tasks 2-3.

- [ ] **Step 5: Commit**

```bash
git add server/ts-common/src/internal_metrics.rs
git commit -m "refactor(metrics): restructure Metric enum — rename Pipeline→Protocol, add Llm/Turn groups, replace ambiguous variants"
```

---

### Task 2: Rename metrics in ts-protocol (dispatcher + flow workers)

**Files:**
- Modify: `server/ts-protocol/src/flow.rs:51,69` — rename metric references
- Modify: `server/ts-protocol/src/stage.rs:38-39` — rename metric registrations

- [ ] **Step 1: Update flow.rs**

In `server/ts-protocol/src/flow.rs`, line 51: replace `Metric::PipelinePacketsDispatched` with `Metric::DispatcherPacketsRouted`.

Line 69: replace `Metric::FlowDispatcherHeartbeatDropped` with `Metric::DispatcherHeartbeatsDropped`.

- [ ] **Step 2: Update stage.rs registration**

In `server/ts-protocol/src/stage.rs`, lines 38-39: replace:

```rust
        &[
            Metric::PipelinePacketsDispatched,
            Metric::FlowDispatcherHeartbeatDropped,
        ],
```

with:

```rust
        &[
            Metric::DispatcherPacketsRouted,
            Metric::DispatcherHeartbeatsDropped,
        ],
```

- [ ] **Step 3: Verify ts-protocol compiles**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check -p ts-protocol`

Expected: PASS

- [ ] **Step 4: Run ts-protocol tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-protocol`

Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add server/ts-protocol/src/flow.rs server/ts-protocol/src/stage.rs
git commit -m "refactor(protocol): use renamed DispatcherPacketsRouted/DispatcherHeartbeatsDropped metrics"
```

---

### Task 3: Instrument ts-llm (processor + stage)

**Files:**
- Modify: `server/ts-llm/src/processor.rs` — accept MetricsWorker, add `.inc()` calls
- Modify: `server/ts-llm/src/stage.rs` — accept MetricsSystem, register workers, count identified/unidentified

This is the biggest task — the LLM stage has 7 new metric points.

- [ ] **Step 1: Add MetricsWorker to LlmProcessor**

In `server/ts-llm/src/processor.rs`, add the import at the top:

```rust
use ts_common::internal_metrics::{Metric, MetricsWorker};
```

Add a `metrics` field to `LlmProcessor`:

```rust
pub struct LlmProcessor {
    pending: HashMap<FlowKey, PendingCall>,
    call_count: u64,
    registry: Arc<ProfileRegistry>,
    metrics: MetricsWorker,
}
```

Update `new()`:

```rust
pub fn new(registry: Arc<ProfileRegistry>, metrics: MetricsWorker) -> Self {
    Self {
        pending: HashMap::new(),
        call_count: 0,
        registry,
        metrics,
    }
}
```

- [ ] **Step 2: Add metric increments in processor methods**

In `on_request()`, after `detect_provider` returns `None` (line ~84):

```rust
None => {
    self.metrics.counter(Metric::LlmRequestsIgnored).inc();
    return Vec::new();
}
```

After the `LlmEvent::Start` is constructed and before it's returned (after line ~132, where `vec![LlmEvent::Start(start)]` is returned):

```rust
self.metrics.counter(Metric::LlmRequestsDetected).inc();
vec![LlmEvent::Start(start)]
```

In `on_response()`, at the very start where `self.pending.remove(&resp.flow_key)?` returns None (line ~142 — the `?` operator), change to explicit match:

```rust
fn on_response(&mut self, resp: HttpResponseData) -> Option<LlmCall> {
    let pending = match self.pending.remove(&resp.flow_key) {
        Some(p) => p,
        None => {
            self.metrics.counter(Metric::LlmResponsesOrphaned).inc();
            return None;
        }
    };
    // ... rest unchanged
```

At the end of `on_response()`, just before `Some(LlmCall { ... })`:

```rust
self.metrics.counter(Metric::LlmCallsCompleted).inc();
```

In `cleanup_stale()`, after the `retain` call, add after `before - self.pending.len()`:

```rust
let expired = before - self.pending.len();
self.metrics.counter(Metric::LlmPendingExpired).add(expired as u64);
expired
```

- [ ] **Step 3: Update LlmProcessor tests to pass MetricsWorker**

In the test module of `processor.rs`, create a helper that builds a dummy MetricsWorker:

```rust
fn test_metrics() -> MetricsWorker {
    use ts_common::internal_metrics::MetricsSystem;
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker("test", &[
        Metric::LlmRequestsDetected,
        Metric::LlmRequestsIgnored,
        Metric::LlmCallsCompleted,
        Metric::LlmResponsesOrphaned,
        Metric::LlmPendingExpired,
    ]);
    let _svc = sys.start();
    w
}
```

Update every `LlmProcessor::new(registry)` call in tests to `LlmProcessor::new(registry, test_metrics())`. There are 8 test functions that call `LlmProcessor::new`:

- `test_openai_chat_non_streaming` — `LlmProcessor::new(empty_registry())` → `LlmProcessor::new(empty_registry(), test_metrics())`
- `test_openai_chat_streaming` — same
- `test_anthropic_streaming` — same
- `test_response_without_request_ignored` — same
- `test_cleanup_stale_pending` — same
- `heartbeat_triggers_stale_cleanup` — same
- `stale_pending_is_replaced_silently_on_reuse` — same
- `test_non_llm_request_ignored` — same
- `complete_for_claude_cli_attaches_identity` — uses `LlmProcessor::new(registry)` → `LlmProcessor::new(registry, test_metrics())`
- `complete_without_profile_match_has_no_identity` — same
- `test_headers_and_response_id_passed_through` — same

- [ ] **Step 4: Update spawn_llm_stage signature to accept MetricsSystem**

In `server/ts-llm/src/stage.rs`, update the import:

```rust
use ts_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
```

Add `metrics_sys: &mut MetricsSystem` parameter to `spawn_llm_stage`:

```rust
pub fn spawn_llm_stage(
    event_rxs: Vec<mpsc::Receiver<ProtocolEvent>>,
    turn_shard_txs: Vec<mpsc::Sender<TurnShardInput>>,
    metrics_shard_txs: Vec<mpsc::Sender<LlmEvent>>,
    calls_tx: mpsc::Sender<Arc<LlmCall>>,
    registry: Arc<ProfileRegistry>,
    metrics_sys: &mut MetricsSystem,
) -> Vec<JoinHandle<()>> {
```

Inside the loop over `event_rxs`, register a worker and create the processor with it. Also add counting for `LlmCallsIdentified` / `LlmCallsUnidentified` around the identity check:

```rust
    let mut handles = Vec::with_capacity(event_rxs.len());
    for (i, mut rx) in event_rxs.into_iter().enumerate() {
        let reg = registry.clone();
        let turn_txs = turn_shard_txs.clone();
        let metrics_txs = metrics_shard_txs.clone();
        let calls_tx = calls_tx.clone();
        let worker_metrics = metrics_sys.register_worker(
            &format!("llm.{i}"),
            &[
                Metric::LlmRequestsDetected,
                Metric::LlmRequestsIgnored,
                Metric::LlmCallsCompleted,
                Metric::LlmCallsIdentified,
                Metric::LlmCallsUnidentified,
                Metric::LlmResponsesOrphaned,
                Metric::LlmPendingExpired,
            ],
        );
        handles.push(tokio::spawn(async move {
            let mut processor = LlmProcessor::new(reg, worker_metrics.clone());
            while let Some(event) = rx.recv().await {
                for llm_event in processor.process(event) {
                    match llm_event {
                        LlmEvent::Heartbeat { ts, ref stream_id } => {
                            // ... unchanged heartbeat fanout ...
                        }
                        other => {
                            let metrics_idx = metrics_shard_index(&other, metrics_txs.len());
                            if metrics_txs[metrics_idx]
                                .send(other.clone())
                                .await
                                .is_err()
                            {
                                return;
                            }
                            if let LlmEvent::Complete { call, identity } = other {
                                if calls_tx.send(call.clone()).await.is_err() {
                                    return;
                                }
                                if let Some(id) = identity {
                                    worker_metrics.counter(Metric::LlmCallsIdentified).inc();
                                    let idx = turn_shard_index(&call.stream_id, &id.session_id, turn_txs.len());
                                    let ic = IdentifiedCall { call, identity: id };
                                    if turn_txs[idx]
                                        .send(TurnShardInput::Call(ic))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                } else {
                                    worker_metrics.counter(Metric::LlmCallsUnidentified).inc();
                                }
                            }
                        }
                    }
                }
            }
        }));
    }
```

- [ ] **Step 5: Update spawn_llm_stage tests**

In the test module of `stage.rs`, every call to `spawn_llm_stage(...)` needs the new `&mut metrics_sys` parameter. There are 3 test functions:

`identified_call_fans_out_to_turn_shard_and_calls_tx_and_metrics`:
```rust
let mut metrics_sys = MetricsSystem::new();
spawn_llm_stage(
    vec![event_rx],
    vec![turn_tx],
    vec![metrics_tx],
    calls_tx,
    Arc::new(build_default_registry()),
    &mut metrics_sys,
);
let _svc = metrics_sys.start();
```

Same pattern for `unidentified_call_skips_turn_shard_still_reaches_calls_tx_and_metrics` and `turn_shard_index_stable_by_session_id_hash`.

Add the import `use ts_common::internal_metrics::MetricsSystem;` if not already present.

- [ ] **Step 6: Verify compilation and run tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-llm`

Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add server/ts-llm/src/processor.rs server/ts-llm/src/stage.rs
git commit -m "feat(llm): instrument LLM stage with internal metrics — detected/ignored/completed/identified/orphaned/expired"
```

---

### Task 4: Instrument ts-turn (tracker + stage)

**Files:**
- Modify: `server/ts-turn/src/tracker.rs` — accept MetricsWorker, count ingested/auxiliary/completed/timed-out
- Modify: `server/ts-turn/src/stage.rs` — register workers with Turn metrics, pass to tracker

- [ ] **Step 1: Add MetricsWorker to TurnTracker**

In `server/ts-turn/src/tracker.rs`, add the import:

```rust
use ts_common::internal_metrics::{Metric, MetricsWorker};
```

Add `metrics: MetricsWorker` field to `TurnTracker`:

```rust
pub struct TurnTracker {
    registry: Arc<ProfileRegistry>,
    config: TrackerConfig,
    active: HashMap<TurnKey, ActiveTurn>,
    next_turn_seq: HashMap<(String, String), u64>,
    virtual_now_us: i64,
    last_sweep_us: i64,
    metrics: MetricsWorker,
}
```

Update `new()` to accept and store it:

```rust
pub fn new(registry: Arc<ProfileRegistry>, config: TrackerConfig, metrics: MetricsWorker) -> Self {
    Self {
        registry,
        config,
        active: HashMap::new(),
        next_turn_seq: HashMap::new(),
        virtual_now_us: 0,
        last_sweep_us: 0,
        metrics,
    }
}
```

- [ ] **Step 2: Add metric increments in tracker**

In `ingest()`, at the very top (after updating `virtual_now_us`):

```rust
self.metrics.counter(Metric::TurnCallsIngested).inc();
```

After the `is_auxiliary` check (line ~213), before returning empty vec:

```rust
if profile.is_auxiliary(call) {
    self.metrics.counter(Metric::TurnCallsAuxiliary).inc();
    return Vec::new();
}
```

In `ingest()`, every place that pushes `TurnEvent::Completed(...)` into `events` (there are 3 places — line ~322, line ~409, and inside the explicit-turn-id path around line ~240), add after each push:

```rust
self.metrics.counter(Metric::TurnsCompleted).inc();
```

In `sweep()`, after each `TurnEvent::Completed(turn.finalize(TurnStatus::Incomplete))` push (line ~454):

```rust
self.metrics.counter(Metric::TurnsTimedOut).inc();
self.metrics.counter(Metric::TurnsCompleted).inc();
```

In `flush_all()`, after each `TurnEvent::Completed(...)` push (line ~472):

```rust
self.metrics.counter(Metric::TurnsCompleted).inc();
```

- [ ] **Step 3: Update TurnTracker tests**

In the test module of `tracker.rs`, create a helper:

```rust
fn test_metrics() -> MetricsWorker {
    use ts_common::internal_metrics::MetricsSystem;
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker("test", &[
        Metric::TurnCallsIngested,
        Metric::TurnCallsAuxiliary,
        Metric::TurnsCompleted,
        Metric::TurnsTimedOut,
    ]);
    let _svc = sys.start();
    w
}
```

Update every `TurnTracker::new(registry, config)` call to `TurnTracker::new(registry, config, test_metrics())`. Search the test module for all occurrences.

- [ ] **Step 4: Update spawn_turn_stage to pass MetricsWorker**

In `server/ts-turn/src/stage.rs`, update the worker registration (currently registering with empty metrics `&[]`). Replace:

```rust
let _ = metrics_sys.register_worker(&format!("turn.{i}"), &[]);
```

with:

```rust
let worker_metrics = metrics_sys.register_worker(
    &format!("turn.{i}"),
    &[
        Metric::TurnCallsIngested,
        Metric::TurnCallsAuxiliary,
        Metric::TurnsCompleted,
        Metric::TurnsTimedOut,
    ],
);
```

Add the `Metric` import if not present. Then pass `worker_metrics` to `TurnTracker::new`:

```rust
let mut tracker = TurnTracker::new(registry, tracker_cfg, worker_metrics);
```

- [ ] **Step 5: Verify compilation and run tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-turn`

Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add server/ts-turn/src/tracker.rs server/ts-turn/src/stage.rs
git commit -m "feat(turn): instrument turn tracker with internal metrics — ingested/auxiliary/completed/timed-out"
```

---

### Task 5: Instrument ts-metrics (aggregator + stage)

**Files:**
- Modify: `server/ts-metrics/src/aggregator.rs` — accept MetricsWorker, count events received / windows flushed
- Modify: `server/ts-metrics/src/stage.rs` — register workers, pass to aggregator

- [ ] **Step 1: Add MetricsWorker to MetricsAggregator**

In `server/ts-metrics/src/aggregator.rs`, add the import:

```rust
use ts_common::internal_metrics::{Metric, MetricsWorker};
```

Add `metrics: MetricsWorker` field to `MetricsAggregator`:

```rust
pub struct MetricsAggregator {
    buckets: HashMap<BucketKey, WindowBucket>,
    concurrency: HashMap<DimensionKey, i64>,
    latest_ts: HashMap<String, i64>,
    metrics: MetricsWorker,
}
```

Update `new()`:

```rust
pub fn new(metrics: MetricsWorker) -> Self {
    Self {
        buckets: HashMap::new(),
        concurrency: HashMap::new(),
        latest_ts: HashMap::new(),
        metrics,
    }
}
```

- [ ] **Step 2: Add metric increments**

In `process()` (line ~81), at the top of the function:

```rust
self.metrics.counter(Metric::MetricsEventsReceived).inc();
```

In `check_windows()`, count each flushed metric. Find where the bucket is flushed (the `bucket.flush(...)` call inside `check_windows`) and add after each push to the result vec:

```rust
self.metrics.counter(Metric::MetricsWindowsFlushed).inc();
```

Also in `flush_all()`, similarly count each flushed metric after `metrics.push(bucket.flush(...))`:

```rust
self.metrics.counter(Metric::MetricsWindowsFlushed).inc();
```

- [ ] **Step 3: Update MetricsAggregator tests**

In the test module of `aggregator.rs`, create a helper:

```rust
fn test_metrics() -> MetricsWorker {
    use ts_common::internal_metrics::MetricsSystem;
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker("test", &[
        Metric::MetricsEventsReceived,
        Metric::MetricsWindowsFlushed,
    ]);
    let _svc = sys.start();
    w
}
```

Update every `MetricsAggregator::new()` call to `MetricsAggregator::new(test_metrics())`.

- [ ] **Step 4: Update spawn_metrics_stage**

In `server/ts-metrics/src/stage.rs`, replace the empty worker registration:

```rust
let _ = metrics_sys.register_worker(&format!("metrics.{i}"), &[]);
```

with:

```rust
let worker_metrics = metrics_sys.register_worker(
    &format!("metrics.{i}"),
    &[
        Metric::MetricsEventsReceived,
        Metric::MetricsWindowsFlushed,
    ],
);
```

Add the `Metric` import. Pass `worker_metrics` to `MetricsAggregator::new`:

```rust
let mut agg = MetricsAggregator::new(worker_metrics);
```

- [ ] **Step 5: Update spawn_metrics_stage tests**

The tests in the same file create `MetricsAggregator` indirectly through `spawn_metrics_stage` — they should continue to work since `spawn_metrics_stage` now passes a registered worker. Verify.

- [ ] **Step 6: Verify compilation and run tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-metrics`

Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add server/ts-metrics/src/aggregator.rs server/ts-metrics/src/stage.rs
git commit -m "feat(metrics): instrument metrics aggregator with internal metrics — events received, windows flushed"
```

---

### Task 6: Instrument ts-storage (buffer + sink)

**Files:**
- Modify: `server/ts-storage/src/buffer.rs` — accept optional MetricsWorker, count buffered/flushed/errors
- Modify: `server/ts-storage/src/sink.rs` — create MetricsWorker, pass to buffers

- [ ] **Step 1: Add MetricsWorker to WriteBuffer**

In `server/ts-storage/src/buffer.rs`, add the import:

```rust
use ts_common::internal_metrics::MetricsWorker;
```

Note: We do NOT import `Metric` here — the buffer is generic and doesn't know which specific metrics to use. Instead, we pass three `MetricHandle` clones for buffered/flushed/errors. But that's over-engineered. Simpler: pass an optional `MetricsWorker` and the specific `Metric` variants to use. Simplest: pass three optional `MetricHandle`s. Actually simplest: just add `Option<MetricsWorker>` and three `Metric` constants.

Better approach — since WriteBuffer is generic, pass an `Option<BufferMetrics>` struct:

```rust
use ts_common::internal_metrics::MetricHandle;

/// Optional metric handles for a WriteBuffer instance.
pub struct BufferMetrics {
    pub buffered: MetricHandle,
    pub flushed: MetricHandle,
    pub errors: MetricHandle,
}
```

Update `WriteBuffer`:

```rust
pub struct WriteBuffer<T> {
    entity: &'static str,
    rx: mpsc::Receiver<T>,
    batch_size: usize,
    flush_interval: Duration,
    metrics: Option<BufferMetrics>,
}
```

Update `new()` to accept `metrics: Option<BufferMetrics>`:

```rust
pub fn new(
    entity: &'static str,
    rx: mpsc::Receiver<T>,
    batch_size: usize,
    flush_interval: Duration,
    metrics: Option<BufferMetrics>,
) -> Self {
    Self {
        entity,
        rx,
        batch_size,
        flush_interval,
        metrics,
    }
}
```

- [ ] **Step 2: Add metric increments in buffer**

In `run()`, after `batch.push(item)`:

```rust
if let Some(ref m) = self.metrics {
    m.buffered.inc();
}
```

In `flush()`, on success path (after `Ok(())`):

```rust
if let Some(ref m) = self.metrics {
    m.flushed.add(batch_len as u64);
}
```

On error path (after `Err(e)`):

```rust
if let Some(ref m) = self.metrics {
    m.errors.inc();
}
```

- [ ] **Step 3: Update WriteBuffer tests**

All `WriteBuffer::new(...)` calls in tests need the extra `None` argument for metrics:

```rust
let buffer = WriteBuffer::new("test", rx, 3, Duration::from_secs(60), None);
```

There are 4 test functions that call `WriteBuffer::new` — update each.

- [ ] **Step 4: Wire metrics in spawn_storage_sink_stage**

In `server/ts-storage/src/sink.rs`, update `spawn_storage_sink_stage` to accept a `MetricsWorker`:

```rust
use ts_common::internal_metrics::{Metric, MetricsWorker};
use crate::buffer::BufferMetrics;
```

Add `metrics: MetricsWorker` parameter:

```rust
pub fn spawn_storage_sink_stage(
    config: StorageSinkConfig,
    calls_rx: mpsc::Receiver<Arc<LlmCall>>,
    turns_rx: mpsc::Receiver<LlmTurn>,
    metrics_rx: mpsc::Receiver<LlmMetric>,
    backend: Arc<dyn StorageBackend>,
    metrics: MetricsWorker,
) -> JoinHandle<()> {
```

Create `BufferMetrics` from the worker:

```rust
let buf_metrics = BufferMetrics {
    buffered: metrics.counter(Metric::StorageRecordsBuffered).clone(),
    flushed: metrics.counter(Metric::StorageRecordsFlushed).clone(),
    errors: metrics.counter(Metric::StorageFlushErrors).clone(),
};
```

Pass `Some(buf_metrics.clone())` (or reconstruct — `MetricHandle` is `Clone`) to each `WriteBuffer::new`:

```rust
let calls_buffer = WriteBuffer::new(
    "calls", owned_rx, config.batch_size, flush_interval,
    Some(BufferMetrics {
        buffered: metrics.counter(Metric::StorageRecordsBuffered).clone(),
        flushed: metrics.counter(Metric::StorageRecordsFlushed).clone(),
        errors: metrics.counter(Metric::StorageFlushErrors).clone(),
    }),
);
// same for turns_buffer, metrics_buffer
```

Since all three buffers share the same counters (aggregate across entity types), clone from the same worker handles.

- [ ] **Step 5: Update sink tests**

In the test in `sink.rs`, update `spawn_storage_sink_stage(...)` call to pass a dummy MetricsWorker:

```rust
use ts_common::internal_metrics::MetricsSystem;

let mut metrics_sys = MetricsSystem::new();
let storage_metrics = metrics_sys.register_worker("storage_sink", &[
    Metric::StorageRecordsBuffered,
    Metric::StorageRecordsFlushed,
    Metric::StorageFlushErrors,
]);
let _svc = metrics_sys.start();

let handle = spawn_storage_sink_stage(cfg, calls_rx, turns_rx, metrics_rx, backend, storage_metrics);
```

- [ ] **Step 6: Update the `pub use` in `ts-storage/src/lib.rs`**

`BufferMetrics` is a public type used by `sink.rs`. It is internal to the crate — no changes needed to `lib.rs` unless `BufferMetrics` needs to be re-exported. Since `sink.rs` is in the same crate, no re-export is needed.

- [ ] **Step 7: Verify compilation and run tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test -p ts-storage`

Expected: all tests pass

- [ ] **Step 8: Commit**

```bash
git add server/ts-storage/src/buffer.rs server/ts-storage/src/sink.rs
git commit -m "feat(storage): instrument write buffer with internal metrics — buffered/flushed/errors"
```

---

### Task 7: Wire everything in the composition root (pipeline.rs + main.rs)

**Files:**
- Modify: `server/app/tokenscope/src/pipeline.rs` — pass MetricsSystem to spawn_llm_stage, register storage sink worker
- Modify: `server/app/tokenscope/src/main.rs` — update `CaptureHeartbeatsEmitted` registration (unchanged name, just verify)
- Modify: `server/app/tokenscope/tests/pipeline_e2e.rs` — adapt to signature changes

- [ ] **Step 1: Update Pipeline::build for spawn_llm_stage**

In `server/app/tokenscope/src/pipeline.rs`, the call to `ts_llm::spawn_llm_stage` (line ~206) needs the extra `metrics_sys` parameter:

```rust
let llm_handles = ts_llm::spawn_llm_stage(
    event_rxs,
    turn_shard_txs,
    metrics_shard_txs,
    calls_tx.clone(),
    registry.clone(),
    metrics_sys,
);
```

- [ ] **Step 2: Register storage sink worker**

The storage sink is shared (not per-pipeline). It needs its own `MetricsWorker`. Since the sink is outside the per-pipeline loop, we need to register it against one of the pipeline metrics systems OR create a separate system. Simplest: register against the first pipeline's MetricsSystem.

Actually, looking at the code more carefully — the sink is shared across pipelines. Best approach: register a storage worker before the loop, against the first MetricsSystem. But that conflates pipeline-specific and global state.

Better: accept an additional `MetricsSystem` parameter for shared stages, or register the storage worker outside and pass it in. Since `Pipeline::build` already takes `&mut [MetricsSystem]`, add the storage worker registration to the first system (index 0):

After the per-pipeline loop, before spawning the storage sink:

```rust
// Register storage sink metrics against the first pipeline's MetricsSystem.
// The sink is shared across all pipelines but we need at least one system
// to own the worker. Since per_pipeline_metrics is already consumed above,
// we cannot register post-loop. Instead, register BEFORE the loop.
```

Wait — `per_pipeline_metrics` is iterated via `.iter_mut()` in the loop. We can still mutate after. Actually no — looking again at line 152:

```rust
for (def, metrics_sys) in pipeline_defs.iter().zip(per_pipeline_metrics.iter_mut()) {
```

After this loop, `per_pipeline_metrics` is still accessible as `&mut [MetricsSystem]`. But the systems haven't been started yet (`.start()` is called in main.rs after `Pipeline::build` returns).

So we can register a storage worker against `per_pipeline_metrics[0]`:

After the loop, before `spawn_storage_sink_stage`:

```rust
// Storage sink is shared across pipelines. Register its worker against
// the first pipeline's MetricsSystem (arbitrary — it just needs a home).
let storage_worker = per_pipeline_metrics[0].register_worker(
    "storage_sink",
    &[
        Metric::StorageRecordsBuffered,
        Metric::StorageRecordsFlushed,
        Metric::StorageFlushErrors,
    ],
);
```

Then pass it to `spawn_storage_sink_stage`:

```rust
let sink_handle = ts_storage::spawn_storage_sink_stage(
    storage_config.clone(),
    calls_rx,
    turns_rx,
    metrics_out_rx,
    storage,
    storage_worker,
);
```

Add `Metric` to the import if not already present.

- [ ] **Step 3: Update pipeline_e2e.rs**

In `server/app/tokenscope/tests/pipeline_e2e.rs`, the test calls `Pipeline::build` — no signature changes needed for the test since we only added a `metrics_sys` parameter to `spawn_llm_stage` which is called internally. The `Pipeline::build` signature itself did not change. Verify no compilation errors.

- [ ] **Step 4: Verify full workspace compilation**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo check`

Expected: compiles with no errors

- [ ] **Step 5: Run all tests**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test`

Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add server/app/tokenscope/src/pipeline.rs
git commit -m "feat(pipeline): wire internal metrics into LLM and storage stages"
```

---

### Task 8: Verify the full funnel is observable

- [ ] **Step 1: Grep for all Metric:: usages and verify each variant is incremented**

Run: `cd /Users/timmy/code/netis/TokenScope/server && grep -rn 'Metric::' --include='*.rs' | grep -v '#\[' | grep -v '//' | grep -v 'test' | grep -v '\.spec()' | grep -v 'MetricKind' | grep -v 'MetricGroup' | grep -v 'MetricSpec' | sort`

Verify that every variant defined in the `define_metrics!` block (except Gauges, which are set via probes) appears in at least one `.inc()` or `.add()` call outside of test code.

- [ ] **Step 2: Run the full test suite one final time**

Run: `cd /Users/timmy/code/netis/TokenScope/server && cargo test`

Expected: all tests pass

- [ ] **Step 3: Final commit if any cleanups were needed**

```bash
git add -A
git commit -m "chore(metrics): final cleanup after pipeline observability wiring"
```
