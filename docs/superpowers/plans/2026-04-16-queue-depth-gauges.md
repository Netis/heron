# Queue Depth Gauges Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire queue depth gauges for all 8 inter-stage channels in the pipeline, using `WeakSender` to avoid blocking EOF cascade shutdown.

**Architecture:** Replace the 3 existing (unwired) gauge metrics with 8 gauges covering every channel in the pipeline topology. Each gauge is a `register_queue_probe` closure that captures a `Vec<WeakSender<T>>` (one per channel instance), sums `max_capacity - capacity` across all of them, and returns 0 once channels close. Per-pipeline gauges are registered against that pipeline's `MetricsSystem`; shared sink gauges are registered against `per_pipeline_metrics[0]`.

**Tech Stack:** Rust, tokio (`mpsc::Sender::downgrade()` → `WeakSender`), `ts_common::internal_metrics`

---

### Task 1: Replace gauge metric variants

**Files:**
- Modify: `server/ts-common/src/internal_metrics.rs:172-176` (gauge block in `define_metrics!`)
- Modify: `server/ts-common/src/internal_metrics.rs:596-604` (test `test_gauge_delta_is_zero`)
- Modify: `server/ts-common/src/internal_metrics.rs:617-618` (test `test_format_grouped`)
- Modify: `server/ts-common/src/internal_metrics.rs:677-678` (test `test_queue_probe_sampling`)

- [ ] **Step 1: Replace the 3 gauge variants with 8 in `define_metrics!`**

In `server/ts-common/src/internal_metrics.rs`, replace the `// -- Queue depths (gauges) --` block (lines 172-176) with:

```rust
    // -- Queue depths (gauges) --
    QueueDepthRaw          => { kind: Gauge, group: Protocol, short: "q.raw"           },
    QueueDepthParsed       => { kind: Gauge, group: Protocol, short: "q.parsed"        },
    QueueDepthEvent        => { kind: Gauge, group: Llm,      short: "q.event"         },
    QueueDepthTurnShard    => { kind: Gauge, group: Turn,     short: "q.turn_shard"    },
    QueueDepthMetricsShard => { kind: Gauge, group: Metrics,  short: "q.metrics_shard" },
    QueueDepthCalls        => { kind: Gauge, group: Storage,  short: "q.calls"         },
    QueueDepthTurns        => { kind: Gauge, group: Storage,  short: "q.turns"         },
    QueueDepthMetricsOut   => { kind: Gauge, group: Storage,  short: "q.metrics_out"   },
```

- [ ] **Step 2: Update test `test_gauge_delta_is_zero`**

Replace `Metric::DispatcherQueueDepth` with `Metric::QueueDepthRaw` (2 occurrences in this test).

```rust
    #[test]
    fn test_gauge_delta_is_zero() {
        let mut sys = MetricsSystem::new();
        sys.register_queue_probe(Metric::QueueDepthRaw, || 42);
        let svc = sys.start();
        svc.sample_probes();

        let mut mon = MetricsMonitor::new(svc);
        let poll = mon.poll();
        assert_eq!(poll.snapshot.values[&Metric::QueueDepthRaw], 42);
        assert_eq!(poll.deltas[&Metric::QueueDepthRaw], 0);
    }
```

- [ ] **Step 3: Update test `test_format_grouped`**

Replace `Metric::StorageQueueDepth` with `Metric::QueueDepthCalls` and update the assertion from `"q.storage=5"` to `"q.calls=5"`.

```rust
        sys.register_queue_probe(Metric::QueueDepthCalls, || 5);
        // ...
        assert!(grouped[2].1.contains("q.calls=5"));
```

- [ ] **Step 4: Update test `test_queue_probe_sampling`**

Replace `Metric::DispatcherQueueDepth` with `Metric::QueueDepthRaw` (3 occurrences).

```rust
    #[test]
    fn test_queue_probe_sampling() {
        let depth = Arc::new(AtomicU64::new(0));
        let mut sys = MetricsSystem::new();

        let depth_clone = depth.clone();
        sys.register_queue_probe(Metric::QueueDepthRaw, move || {
            depth_clone.load(Ordering::Relaxed)
        });

        let svc = sys.start();

        assert_eq!(svc.aggregate(Metric::QueueDepthRaw), Some(0));

        depth.store(42, Ordering::Relaxed);
        svc.sample_probes();
        assert_eq!(svc.aggregate(Metric::QueueDepthRaw), Some(42));

        depth.store(5, Ordering::Relaxed);
        svc.sample_probes();
        assert_eq!(svc.aggregate(Metric::QueueDepthRaw), Some(5));
    }
```

- [ ] **Step 5: Run tests**

Run: `cd server && cargo test -p ts-common -- internal_metrics`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add server/ts-common/src/internal_metrics.rs
git commit -m "refactor(metrics): replace 3 placeholder gauges with 8 queue depth gauges"
```

---

### Task 2: Wire queue depth probes in `pipeline.rs`

**Files:**
- Modify: `server/app/tokenscope/src/pipeline.rs`

**Context:** `Pipeline::build` has `&mut [MetricsSystem]` — one per pipeline. Per-pipeline channels register probes against that pipeline's `MetricsSystem`. Shared sink channels register against `per_pipeline_metrics[0]`. The `tokio::sync::mpsc::Sender::downgrade()` method returns a `WeakSender<T>` that does not count toward the sender reference count, so it won't prevent EOF cascade.

The probe pattern for a `Vec` of channels:

```rust
use tokio::sync::mpsc::WeakSender;

let weaks: Vec<WeakSender<T>> = senders.iter().map(|tx| tx.downgrade()).collect();
metrics_sys.register_queue_probe(Metric::QueueDepthFoo, move || {
    weaks.iter().filter_map(|w| w.upgrade()).map(|s| {
        (s.max_capacity() - s.capacity()) as u64
    }).sum()
});
```

**Key timing constraints for when `downgrade()` must happen:**

| Channel | Created at | Consumed/dropped at | Downgrade window |
|---------|-----------|---------------------|------------------|
| `disp_txs` (raw) | loop, line 185 | `RoutingSender::new(disp_txs)` line 289 | After loop ends at line 204, before line 289 |
| `parsed_txs` | `make_shard_channels` line 166 | Cloned to dispatchers, originals `drop(parsed_txs)` line 207 | Before line 207 — but WeakSender survives original drop because dispatcher clones keep channel alive |
| `event_txs` | `make_shard_channels` line 169 | Moved into `spawn_protocol_stage` line 210 | Before line 210 |
| `turn_shard_txs` | `make_shard_channels` line 173 | Moved into `spawn_llm_stage` line 228 | Before line 228 |
| `metrics_shard_txs` | `make_shard_channels` line 175 | Moved into `spawn_llm_stage` line 228 | Before line 228 |
| `calls_tx` | Before loop, line 132 | Cloned in loop, original dropped line 297 | Before line 297 |
| `turns_tx` | Before loop, line 133 | Cloned in loop, original dropped line 298 | Before line 298 |
| `metrics_out_tx` | Before loop, line 134 | Cloned in loop, original dropped line 299 | Before line 299 |

- [ ] **Step 1: Add `WeakSender` import**

At the top of `server/app/tokenscope/src/pipeline.rs`, add to the `tokio::sync::mpsc` import:

```rust
use tokio::sync::mpsc::{self, WeakSender};
```

Replace the existing `use tokio::sync::mpsc;` line.

- [ ] **Step 2: Add `Metric` import**

Add to the `ts_common` import:

```rust
use ts_common::internal_metrics::Metric;
```

(Currently `pipeline.rs` imports `MetricsSystem` but not `Metric`; `Metric` is imported only in `main.rs`.)

- [ ] **Step 3: Register shared sink queue probes (before the per-pipeline loop)**

After creating the shared sink channels (after line 134), and before the per-pipeline loop (line 153), register 3 probes against `per_pipeline_metrics[0]`:

```rust
        // Shared sink queue depth probes — registered against the first
        // pipeline's MetricsSystem (same as the storage_sink worker).
        {
            let w = calls_tx.downgrade();
            per_pipeline_metrics[0].register_queue_probe(Metric::QueueDepthCalls, move || {
                w.upgrade()
                    .map_or(0, |s| (s.max_capacity() - s.capacity()) as u64)
            });
            let w = turns_tx.downgrade();
            per_pipeline_metrics[0].register_queue_probe(Metric::QueueDepthTurns, move || {
                w.upgrade()
                    .map_or(0, |s| (s.max_capacity() - s.capacity()) as u64)
            });
            let w = metrics_out_tx.downgrade();
            per_pipeline_metrics[0].register_queue_probe(Metric::QueueDepthMetricsOut, move || {
                w.upgrade()
                    .map_or(0, |s| (s.max_capacity() - s.capacity()) as u64)
            });
        }
```

- [ ] **Step 4: Register per-pipeline queue probes (inside the per-pipeline loop)**

Inside the per-pipeline loop, after all channels are created and stages are spawned (after the metrics stage handles block at ~line 287, before `pipeline_txs.push` at line 289), add:

```rust
            // ---- Per-pipeline queue depth probes ----
            // Downgrade senders to WeakSender before they are moved/dropped,
            // so probes don't keep channels alive past EOF.
            let raw_weaks: Vec<WeakSender<RawPacket>> =
                disp_txs.iter().map(|tx| tx.downgrade()).collect();
            metrics_sys.register_queue_probe(Metric::QueueDepthRaw, move || {
                raw_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
```

Note: `parsed_txs` are dropped at line 207 before this point. The `WeakSender` must be captured **before** the drop. So for `parsed_txs`, capture weaks right after `make_shard_channels` and before the dispatcher loop drops them:

Insert right after `make_shard_channels` for parsed (after line 167):

```rust
            let parsed_weaks: Vec<WeakSender<WorkerInput>> =
                parsed_txs.iter().map(|tx| tx.downgrade()).collect();
```

Similarly, right after `make_shard_channels` for event (after line 172):

```rust
            let event_weaks: Vec<WeakSender<ts_protocol::model::ProtocolEvent>> =
                event_txs.iter().map(|tx| tx.downgrade()).collect();
```

Right after `make_shard_channels` for turn_shard (after line 174):

```rust
            let turn_shard_weaks: Vec<WeakSender<TurnShardInput>> =
                turn_shard_txs.iter().map(|tx| tx.downgrade()).collect();
```

Right after `make_shard_channels` for metrics_shard (after line 176):

```rust
            let metrics_shard_weaks: Vec<WeakSender<LlmEvent>> =
                metrics_shard_txs.iter().map(|tx| tx.downgrade()).collect();
```

Then, at the end of the per-pipeline section (after metrics stage handles, before `pipeline_txs.push`), register the remaining 4 probes:

```rust
            metrics_sys.register_queue_probe(Metric::QueueDepthParsed, move || {
                parsed_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
            metrics_sys.register_queue_probe(Metric::QueueDepthEvent, move || {
                event_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
            metrics_sys.register_queue_probe(Metric::QueueDepthTurnShard, move || {
                turn_shard_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
            metrics_sys.register_queue_probe(Metric::QueueDepthMetricsShard, move || {
                metrics_shard_weaks
                    .iter()
                    .filter_map(|w| w.upgrade())
                    .map(|s| (s.max_capacity() - s.capacity()) as u64)
                    .sum()
            });
```

- [ ] **Step 5: Run full test suite**

Run: `cd server && cargo test`
Expected: All tests pass (334+).

- [ ] **Step 6: Commit**

```bash
git add server/app/tokenscope/src/pipeline.rs
git commit -m "feat(metrics): wire 8 queue depth gauges via WeakSender probes"
```
