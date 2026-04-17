# Pipeline Phase 1: Parallelize LLM Extraction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the single-`LlmProcessor` fan-in bottleneck in `app/tokenscope/src/main.rs` by running N parallel `LlmProcessor` tasks paired 1:1 with flow workers, with `main.rs` as the composition root for all boundary channels.

**Architecture:** Introduce `ts_llm::spawn_llm_stage` as a new public API. Rename `ts_protocol::pipeline::start_pipeline` → `ts_protocol::spawn_protocol_stage` with boundary channels (both `raw_rx` ingress and `event_txs` egress) injected by the caller. `main.rs` creates every cross-stage channel and wires the two stages together.

**Tech Stack:** Rust 2021 · Tokio (mpsc channels, tasks) · Cargo workspace (server/)

**Parent spec:** `docs/superpowers/specs/2026-04-13-pipeline-phase1-llm-parallelization-design.md`

---

## File Structure

**Create:**
- `server/ts-llm/src/stage.rs` — new `spawn_llm_stage` public API plus unit tests
- `server/ts-protocol/src/stage.rs` — new `spawn_protocol_stage` + `ProtocolStageConfig`

**Modify:**
- `server/ts-llm/Cargo.toml` — add tokio dependency (ts-llm currently has none)
- `server/ts-llm/src/lib.rs` — add `pub mod stage` + re-export
- `server/ts-protocol/src/lib.rs` — remove `pub mod pipeline`, add `pub mod stage` + re-export
- `server/app/tokenscope/src/main.rs` — rewire as composition root
- `server/ts-turn/tests/integration.rs` — migrate to new stage API

**Delete:**
- `server/ts-protocol/src/pipeline.rs` — replaced by `stage.rs`

---

## Task 1: Add tokio dep to ts-llm and scaffold `stage.rs`

Prep task so later TDD steps have a place to write.

**Files:**
- Modify: `server/ts-llm/Cargo.toml`
- Create: `server/ts-llm/src/stage.rs`
- Modify: `server/ts-llm/src/lib.rs`

- [ ] **Step 1: Add tokio to ts-llm dependencies**

Edit `server/ts-llm/Cargo.toml`, add `tokio.workspace = true` under `[dependencies]`:

```toml
[package]
name = "ts-llm"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
ts-common.workspace = true
ts-protocol.workspace = true
serde.workspace = true
serde_json.workspace = true
bytes.workspace = true
tokio.workspace = true
tracing.workspace = true
thiserror.workspace = true
```

- [ ] **Step 2: Create skeleton `stage.rs`**

Create `server/ts-llm/src/stage.rs` with just the module doc and imports:

```rust
//! LLM extraction stage: spawns N parallel LlmProcessor tasks, paired 1:1 with
//! flow workers. Each task owns its own LlmProcessor and processes events from
//! exactly one input receiver, forwarding LlmEvents to a shared output channel.

use tokio::sync::mpsc;

use ts_protocol::model::ProtocolEvent;

use crate::model::LlmEvent;
use crate::processor::LlmProcessor;
```

- [ ] **Step 3: Register module and re-export**

Edit `server/ts-llm/src/lib.rs`:

```rust
pub mod model;
pub mod processor;
pub mod stage;

// Internal modules — not part of the public API.
pub(crate) mod anthropic;
pub(crate) mod detector;
pub(crate) mod openai;

pub use stage::spawn_llm_stage;
```

- [ ] **Step 4: Verify the crate still compiles**

Run: `cd server && cargo check -p ts-llm`
Expected: success (no warnings about `spawn_llm_stage` since we haven't declared it yet — actually this will fail because the `pub use` references a missing symbol).

- [ ] **Step 5: Stub `spawn_llm_stage` so compile passes**

Append to `server/ts-llm/src/stage.rs`:

```rust
/// Spawn N parallel LLM-extraction tasks, one per input receiver. Each task
/// owns its own `LlmProcessor` and forwards emitted `LlmEvent`s into a clone of
/// `output_tx`. Tasks exit cleanly when their input channel closes.
pub fn spawn_llm_stage(
    event_rxs: Vec<mpsc::Receiver<ProtocolEvent>>,
    output_tx: mpsc::Sender<LlmEvent>,
) {
    for mut rx in event_rxs {
        let tx = output_tx.clone();
        tokio::spawn(async move {
            let mut processor = LlmProcessor::new();
            while let Some(event) = rx.recv().await {
                for llm_event in processor.process(event) {
                    if tx.send(llm_event).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
}
```

- [ ] **Step 6: Verify compile**

Run: `cd server && cargo check -p ts-llm`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add server/ts-llm/Cargo.toml server/ts-llm/src/lib.rs server/ts-llm/src/stage.rs
git commit -m "feat(ts-llm): scaffold spawn_llm_stage

New public API at the ts-llm crate root: spawns N parallel LlmProcessor
tasks, one per input ProtocolEvent receiver, forwarding emitted
LlmEvents into a shared output channel. Prep for Phase 1 pipeline
refactor — tests follow."
```

---

## Task 2: TDD — single-receiver test for `spawn_llm_stage`

Verify the happy path: one receiver fed a request + response produces Start + Complete.

**Files:**
- Modify: `server/ts-llm/src/stage.rs`

- [ ] **Step 1: Add failing test**

Append to `server/ts-llm/src/stage.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::IpAddr;
    use ts_protocol::model::{HttpRequestData, HttpResponseData, ProtocolEvent};
    use ts_protocol::net::FlowKey;

    use crate::model::ProviderFormat;

    fn flow_key(port: u16) -> FlowKey {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        FlowKey::new(ip, port, ip, 8080)
    }

    fn openai_request(fk: FlowKey, ts_us: i64) -> HttpRequestData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        });
        HttpRequestData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            version: 1,
            headers: vec![
                ("authorization".to_string(), "Bearer sk-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: ts_us,
        }
    }

    fn openai_response(fk: FlowKey, ts_us: i64) -> HttpResponseData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        });
        HttpResponseData {
            flow_key: fk,
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(body.to_string()),
            first_byte_timestamp_us: ts_us + 100_000,
            complete_timestamp_us: ts_us + 200_000,
        }
    }

    #[tokio::test]
    async fn single_receiver_emits_start_and_complete() {
        let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>(16);
        let (out_tx, mut out_rx) = mpsc::channel::<LlmEvent>(16);

        spawn_llm_stage(vec![event_rx], out_tx);

        let fk = flow_key(5000);
        event_tx
            .send(ProtocolEvent::HttpRequest(openai_request(fk.clone(), 1_000_000)))
            .await
            .unwrap();
        event_tx
            .send(ProtocolEvent::HttpResponse(openai_response(fk, 1_000_000)))
            .await
            .unwrap();
        drop(event_tx);

        let first = out_rx.recv().await.expect("Start event");
        match first {
            LlmEvent::Start(s) => assert_eq!(s.provider, ProviderFormat::OpenAI),
            LlmEvent::Complete(_) => panic!("expected Start first"),
        }
        let second = out_rx.recv().await.expect("Complete event");
        match second {
            LlmEvent::Complete(call) => {
                assert_eq!(call.provider, ProviderFormat::OpenAI);
                assert_eq!(call.model, "gpt-4");
                assert_eq!(call.input_tokens, Some(5));
                assert_eq!(call.output_tokens, Some(3));
            }
            LlmEvent::Start(_) => panic!("expected Complete second"),
        }
        assert!(out_rx.recv().await.is_none(), "channel should close");
    }
}
```

- [ ] **Step 2: Run test, verify PASS**

Run: `cd server && cargo test -p ts-llm --lib stage::tests::single_receiver_emits_start_and_complete`
Expected: PASS (`spawn_llm_stage` is already implemented from Task 1; this test locks in behavior).

- [ ] **Step 3: Commit**

```bash
git add server/ts-llm/src/stage.rs
git commit -m "test(ts-llm): single-receiver happy path for spawn_llm_stage"
```

---

## Task 3: TDD — multi-receiver parallel test

Verify N=4 receivers each with a different flow key produce 4 independent Start/Complete pairs.

**Files:**
- Modify: `server/ts-llm/src/stage.rs`

- [ ] **Step 1: Add failing test**

Append inside the `mod tests` block in `server/ts-llm/src/stage.rs`, after the first test:

```rust
    #[tokio::test]
    async fn four_receivers_parallel_four_flows() {
        let mut event_txs = Vec::with_capacity(4);
        let mut event_rxs = Vec::with_capacity(4);
        for _ in 0..4 {
            let (tx, rx) = mpsc::channel::<ProtocolEvent>(16);
            event_txs.push(tx);
            event_rxs.push(rx);
        }
        let (out_tx, mut out_rx) = mpsc::channel::<LlmEvent>(64);

        spawn_llm_stage(event_rxs, out_tx);

        // Each input channel gets a distinct flow_key (port chosen per receiver).
        for (i, tx) in event_txs.iter().enumerate() {
            let fk = flow_key(5000 + i as u16);
            tx.send(ProtocolEvent::HttpRequest(openai_request(fk.clone(), 1_000_000)))
                .await
                .unwrap();
            tx.send(ProtocolEvent::HttpResponse(openai_response(fk, 1_000_000)))
                .await
                .unwrap();
        }
        drop(event_txs);

        let mut starts = 0usize;
        let mut completes = 0usize;
        while let Some(evt) = out_rx.recv().await {
            match evt {
                LlmEvent::Start(_) => starts += 1,
                LlmEvent::Complete(_) => completes += 1,
            }
        }
        assert_eq!(starts, 4, "one Start per receiver");
        assert_eq!(completes, 4, "one Complete per receiver");
    }
```

- [ ] **Step 2: Run test, verify PASS**

Run: `cd server && cargo test -p ts-llm --lib stage::tests::four_receivers_parallel_four_flows`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add server/ts-llm/src/stage.rs
git commit -m "test(ts-llm): N=4 parallel receivers emit independent events"
```

---

## Task 4: TDD — clean shutdown test

Verify dropping all input senders causes tasks to exit and the output channel to close.

**Files:**
- Modify: `server/ts-llm/src/stage.rs`

- [ ] **Step 1: Add failing test**

Append inside the `mod tests` block:

```rust
    #[tokio::test]
    async fn tasks_exit_when_input_channels_drop() {
        let mut event_txs = Vec::new();
        let mut event_rxs = Vec::new();
        for _ in 0..3 {
            let (tx, rx) = mpsc::channel::<ProtocolEvent>(4);
            event_txs.push(tx);
            event_rxs.push(rx);
        }
        let (out_tx, mut out_rx) = mpsc::channel::<LlmEvent>(4);

        spawn_llm_stage(event_rxs, out_tx);
        // out_tx is dropped here — only the per-task clones remain alive.

        // Drop all input senders; each llm-proc task should exit, dropping its
        // out_tx clone. Once all clones are dropped, out_rx observes None.
        drop(event_txs);

        assert!(out_rx.recv().await.is_none(), "output channel must close after input drop");
    }
```

- [ ] **Step 2: Run test, verify PASS**

Run: `cd server && cargo test -p ts-llm --lib stage::tests::tasks_exit_when_input_channels_drop`
Expected: PASS

- [ ] **Step 3: Run the full ts-llm test suite to confirm no regression**

Run: `cd server && cargo test -p ts-llm`
Expected: All tests pass (existing `processor.rs` tests + 3 new `stage.rs` tests).

- [ ] **Step 4: Commit**

```bash
git add server/ts-llm/src/stage.rs
git commit -m "test(ts-llm): clean task exit on input channel drop"
```

---

## Task 5: Introduce `spawn_protocol_stage` alongside `start_pipeline`

Add the new API without removing the old one yet. Keeps main.rs and the integration test compiling across the refactor.

**Files:**
- Create: `server/ts-protocol/src/stage.rs`
- Modify: `server/ts-protocol/src/lib.rs`

- [ ] **Step 1: Create `stage.rs`**

Create `server/ts-protocol/src/stage.rs`:

```rust
//! Protocol parsing stage: runs one dispatcher + N flow workers. Both the
//! ingress channel (`raw_rx`) and the egress channels (`event_txs`) are
//! injected by the caller — the stage owns only the internal dispatcher →
//! worker `ParsedPacket` channels.

use tokio::sync::mpsc;

use ts_capture::RawPacket;
use ts_common::internal_metrics::{Metric, MetricsSystem};

use crate::flow::FlowDispatcher;
use crate::model::ProtocolEvent;
use crate::tcp::FlowWorker;

/// Configuration for the protocol parsing stage.
pub struct ProtocolStageConfig {
    pub worker_count: usize,
    /// Channel capacity for dispatcher → each worker (internal).
    pub worker_queue_size: usize,
}

impl Default for ProtocolStageConfig {
    fn default() -> Self {
        Self {
            worker_count: 4,
            worker_queue_size: 4096,
        }
    }
}

/// Spawn the protocol parsing stage: one dispatcher task that consumes from
/// `raw_rx`, plus `config.worker_count` flow-worker tasks whose outputs are
/// routed to `event_txs[i]`.
///
/// Panics if `event_txs.len() != config.worker_count` — that is a wiring bug
/// in the composition root, not a runtime condition.
pub fn spawn_protocol_stage(
    config: ProtocolStageConfig,
    mut raw_rx: mpsc::Receiver<RawPacket>,
    event_txs: Vec<mpsc::Sender<ProtocolEvent>>,
    metrics_sys: &mut MetricsSystem,
) {
    assert_eq!(
        event_txs.len(),
        config.worker_count,
        "event_txs length must equal worker_count (composition-root wiring bug)"
    );

    let mut worker_txs = Vec::with_capacity(config.worker_count);
    for (i, event_tx) in event_txs.into_iter().enumerate() {
        let (wtx, mut wrx) = mpsc::channel(config.worker_queue_size);
        worker_txs.push(wtx);

        let worker_metrics = metrics_sys.register_worker(
            &format!("worker.{i}"),
            &[
                Metric::NetPacketsParsed,
                Metric::HttpRequestsParsed,
                Metric::HttpResponsesParsed,
                Metric::SseEventsParsed,
                Metric::HttpResyncEvents,
                Metric::FlowsTimedOut,
            ],
        );

        tokio::spawn(async move {
            let mut worker = FlowWorker::new(event_tx, worker_metrics);
            while let Some(pkt) = wrx.recv().await {
                worker.process(pkt).await;
            }
        });
    }

    let dispatcher_metrics = metrics_sys.register_worker(
        "dispatcher",
        &[Metric::PipelinePacketsDispatched],
    );
    tokio::spawn(async move {
        let dispatcher = FlowDispatcher::new(worker_txs, dispatcher_metrics);
        while let Some(raw) = raw_rx.recv().await {
            if !dispatcher.dispatch(&raw).await {
                break;
            }
        }
    });
}
```

- [ ] **Step 2: Register module in lib.rs**

Edit `server/ts-protocol/src/lib.rs`:

```rust
pub mod de;
pub mod flow;
pub mod http;
pub mod model;
pub mod net;
pub mod pipeline;
pub mod stage;
pub mod tcp;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("parse error: {0}")]
    Parse(String),

    #[error("channel closed")]
    ChannelClosed,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

pub use stage::{spawn_protocol_stage, ProtocolStageConfig};
```

- [ ] **Step 3: Verify compile**

Run: `cd server && cargo check -p ts-protocol`
Expected: PASS (both old `start_pipeline` and new `spawn_protocol_stage` coexist).

- [ ] **Step 4: Commit**

```bash
git add server/ts-protocol/src/stage.rs server/ts-protocol/src/lib.rs
git commit -m "feat(ts-protocol): add spawn_protocol_stage alongside start_pipeline

New stage API with boundary channels (raw_rx, event_txs) injected by
the caller. Returns nothing — composition root owns all endpoints.
start_pipeline is kept temporarily so callers can migrate
independently; it will be removed after the migration lands."
```

---

## Task 6: Migrate `main.rs` to the new wiring

Replace the old single-`LlmProcessor` topology with the composition-root pattern.

**Files:**
- Modify: `server/app/tokenscope/src/main.rs`

- [ ] **Step 1: Update imports**

In `server/app/tokenscope/src/main.rs`, replace the existing `use ts_protocol::pipeline::...` line with the new stage import, and remove the `LlmProcessor` import (no longer used in main). The final import block for the pipeline-relevant pieces should look like:

```rust
use tokio::sync::mpsc;
use ts_capture::CaptureSource;
use ts_common::config::{AppConfig, CaptureSourceConfig};
use ts_common::internal_metrics::{Metric, MetricsReporter, MetricsSystem};
use ts_llm::model::{LlmCall, LlmEvent};
use ts_metrics::aggregator::MetricsAggregator;
use ts_metrics::model::LlmMetric;
use ts_protocol::{spawn_protocol_stage, ProtocolStageConfig};
use ts_storage::{create_backend, create_buffer};
use ts_turn::profiles::build_default_registry;
use ts_turn::tracker::{TrackerConfig, TurnEvent, TurnTracker};
use ts_turn::LlmTurn;
```

(Add the `tokio::sync::mpsc` import if not already present; remove `use ts_llm::processor::LlmProcessor;`.)

- [ ] **Step 2: Replace the pipeline-start block with composition-root wiring**

In the `if !sources.is_empty()` branch, replace the block that currently reads (roughly lines 272-278):

```rust
// Start the protocol processing pipeline.
let pipeline_config = PipelineConfig {
    worker_count: config.pipeline.worker_count,
    ..Default::default()
};
let (raw_tx, mut pipeline) = start_pipeline(pipeline_config, &mut metrics_sys);
```

with:

```rust
// Compose channels: every cross-stage boundary lives here.
let worker_count = config.pipeline.worker_count;
let queue_size = 4096usize;
let (raw_tx, raw_rx) = mpsc::channel(queue_size);
let mut event_txs = Vec::with_capacity(worker_count);
let mut event_rxs = Vec::with_capacity(worker_count);
for _ in 0..worker_count {
    let (tx, rx) = mpsc::channel(queue_size);
    event_txs.push(tx);
    event_rxs.push(rx);
}
let (llm_tx, mut llm_rx) = mpsc::channel(queue_size);

// Start stages. Both take their channel endpoints by parameter.
let protocol_cfg = ProtocolStageConfig {
    worker_count,
    ..Default::default()
};
spawn_protocol_stage(protocol_cfg, raw_rx, event_txs, &mut metrics_sys);
ts_llm::spawn_llm_stage(event_rxs, llm_tx);
```

- [ ] **Step 3: Replace the main select-loop event source**

Find the `tokio::select! { event = pipeline.event_rx.recv() => { ... match event { Some(event) => { ... for llm_event in llm.process(event) { ... } } None => break } } ... }` block (roughly lines 320-384). Replace:

- the select arm `event = pipeline.event_rx.recv()` → `maybe_event = llm_rx.recv()`
- inside `Some(event) => { ... for llm_event in llm.process(event) { ... } }`, remove the `for llm_event in llm.process(event)` outer loop — the `llm_event` is now the direct receiver item

The full replacement for the main loop should read:

```rust
loop {
    tokio::select! {
        maybe_event = llm_rx.recv() => {
            match maybe_event {
                Some(llm_event) => {
                    match &llm_event {
                        LlmEvent::Start(start) => {
                            tracing::trace!("{start}");
                        }
                        LlmEvent::Complete(call) => {
                            tracing::trace!("{call}");
                            let mut tagged = call.clone();
                            let events = tracker.ingest(&mut tagged);
                            if let Err(e) = calls_handle.send(tagged).await {
                                tracing::error!("failed to send call to buffer: {e}");
                            }
                            for te in events {
                                if let TurnEvent::Completed(t) = te {
                                    tracing::trace!("{t}");
                                    if let Err(e) = turns_handle.send(t).await {
                                        tracing::error!("failed to send turn to buffer: {e}");
                                    }
                                }
                            }
                            for te in tracker.sweep() {
                                if let TurnEvent::Completed(t) = te {
                                    if let Err(e) = turns_handle.send(t).await {
                                        tracing::error!("failed to send swept turn: {e}");
                                    }
                                }
                            }
                        }
                    }
                    for metric in aggregator.process(&llm_event) {
                        tracing::trace!("{metric}");
                        if let Err(e) = metrics_handle.send(metric).await {
                            tracing::error!("failed to send metric to buffer: {e}");
                        }
                    }
                }
                None => break,
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl+C, stopping...");
            cancel.cancel();
            break;
        }
    }
}
```

- [ ] **Step 4: Remove the local `LlmProcessor` and the shutdown-log lines that read it**

Delete the line `let mut llm = LlmProcessor::new();` (just before the loop).

Delete the shutdown log block at roughly lines 408-412:

```rust
tracing::info!(
    "capture complete: {} LLM calls extracted, {} pending",
    llm.call_count(),
    llm.pending_count(),
);
```

Do not replace it — per spec, pending/call-count observability is deferred to a future `MetricsSystem`-summed metric.

- [ ] **Step 5: Build the full workspace**

Run: `cd server && cargo build`
Expected: PASS. No references to `PipelineConfig` (protocol version), `start_pipeline`, or `PipelineHandle` remain in `main.rs`.

- [ ] **Step 6: Run the full workspace test suite**

Run: `cd server && cargo test`
Expected: PASS. The only failing test should be in `ts-turn/tests/integration.rs` which still imports the old `ts_protocol::pipeline::{start_pipeline, PipelineConfig}`. If the fixture file is absent, the integration test skips gracefully — in that case the import alone will cause a compile error. **Proceed to Task 7 to fix that test before declaring success.**

If the ts-turn test fails to **compile**, that is expected at this step. Continue without committing.

- [ ] **Step 7: Commit (only if `cd server && cargo check -p tokenscope` passes)**

```bash
git add server/app/tokenscope/src/main.rs
git commit -m "feat(tokenscope): composition-root wiring for protocol + llm stages

main.rs now owns every boundary channel (raw, per-worker ProtocolEvent,
shared LlmEvent) and hands endpoints to ts-protocol::spawn_protocol_stage
and ts-llm::spawn_llm_stage. The old single-LlmProcessor main-loop
lane is replaced by N parallel llm-proc tasks (1:1 with flow workers).

The shutdown log's 'N LLM calls extracted' line is removed — per-processor
counters are no longer globally aggregated; future observability will go
through MetricsSystem."
```

---

## Task 7: Migrate `ts-turn` integration test

**Files:**
- Modify: `server/ts-turn/tests/integration.rs`

- [ ] **Step 1: Update imports and body to use new API**

Replace the full content of `server/ts-turn/tests/integration.rs` with:

```rust
//! End-to-end: read pcap → ts-protocol stage → ts-llm stage →
//! ts-turn tracker → assert turn counts against ground truth.
//!
//! Skips gracefully if fixtures are missing (they are gitignored).

use std::path::PathBuf;

use tokio::sync::mpsc;

use ts_capture::CaptureSource;
use ts_capture::PcapFileSource;
use ts_common::internal_metrics::{Metric, MetricsSystem};
use ts_llm::model::LlmEvent;
use ts_protocol::{spawn_protocol_stage, ProtocolStageConfig};
use ts_turn::profiles::build_default_registry;
use ts_turn::tracker::{TrackerConfig, TurnEvent, TurnTracker};
use ts_turn::TurnStatus;

fn fixture(name: &str) -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/pcaps")
        .join(name);
    if root.exists() {
        Some(root)
    } else {
        None
    }
}

async fn run_pcap(name: &str) -> Option<Vec<ts_turn::LlmTurn>> {
    let path = fixture(name)?;
    let mut metrics_sys = MetricsSystem::new();

    let source_metrics = metrics_sys.register_worker(
        "capture.test",
        &[
            Metric::CapturePacketsReceived,
            Metric::CapturePacketsDropped,
        ],
    );

    // Composition root: all boundary channels created here.
    let worker_count = 1usize;
    let queue_size = 4096usize;
    let (raw_tx, raw_rx) = mpsc::channel(queue_size);
    let (event_tx, event_rx) = mpsc::channel(queue_size);
    let (llm_tx, mut llm_rx) = mpsc::channel(queue_size);

    let cfg = ProtocolStageConfig {
        worker_count,
        ..Default::default()
    };
    spawn_protocol_stage(cfg, raw_rx, vec![event_tx], &mut metrics_sys);
    ts_llm::spawn_llm_stage(vec![event_rx], llm_tx);

    let _metrics_svc = metrics_sys.start();

    let source = PcapFileSource::new(path);
    let cancel = tokio_util::sync::CancellationToken::new();
    let src_task = tokio::spawn({
        let tx = raw_tx.clone();
        let cancel = cancel.clone();
        async move {
            let _ = Box::new(source).run(tx, source_metrics, cancel).await;
        }
    });
    drop(raw_tx);

    let registry = build_default_registry();
    let mut tracker = TurnTracker::new(registry, TrackerConfig::default());
    let mut finalized: Vec<ts_turn::LlmTurn> = Vec::new();

    while let Some(llm_event) = llm_rx.recv().await {
        if let LlmEvent::Complete(mut call) = llm_event {
            for e in tracker.ingest(&mut call) {
                if let TurnEvent::Completed(t) = e {
                    finalized.push(t);
                }
            }
            for e in tracker.sweep() {
                if let TurnEvent::Completed(t) = e {
                    finalized.push(t);
                }
            }
        }
    }
    for e in tracker.flush_all() {
        if let TurnEvent::Completed(t) = e {
            finalized.push(t);
        }
    }
    let _ = src_task.await;
    Some(finalized)
}

#[tokio::test]
async fn claude_cli_messages_expects_one_complete_turn() {
    let Some(turns) = run_pcap("claude-cli-messages.pcap").await else {
        eprintln!("skip: fixture not present");
        return;
    };
    let anthropic: Vec<_> = turns.iter().filter(|t| t.provider == "anthropic").collect();
    eprintln!("claude-cli-messages: {} anthropic turns", anthropic.len());
    for t in &anthropic {
        eprintln!(
            "  turn {} status={:?} calls={}",
            t.turn_id, t.status, t.call_count
        );
    }
    assert_eq!(
        anthropic.len(),
        1,
        "expected 1 turn; got {}",
        anthropic.len()
    );
    assert_eq!(anthropic[0].status, TurnStatus::Complete);
    assert_eq!(anthropic[0].client_kind, "claude-cli");
}
```

- [ ] **Step 2: Run the full test suite**

Run: `cd server && cargo test`
Expected: PASS. If the pcap fixture is absent, the integration test prints `skip: fixture not present` and passes.

- [ ] **Step 3: Commit**

```bash
git add server/ts-turn/tests/integration.rs
git commit -m "test(ts-turn): migrate integration test to composition-root API"
```

---

## Task 8: Delete the old `start_pipeline` / `PipelineConfig` / `PipelineHandle`

With no callers remaining, retire the old API.

**Files:**
- Delete: `server/ts-protocol/src/pipeline.rs`
- Modify: `server/ts-protocol/src/lib.rs`

- [ ] **Step 1: Confirm no remaining callers**

Run: `cd server && cargo build 2>&1 | grep -E 'start_pipeline|PipelineConfig|PipelineHandle' || echo "no references"`
Expected: `no references`

Run a grep for safety:

Use the Grep tool with pattern `start_pipeline|PipelineHandle|ts_protocol::pipeline` over `server/`. Expected: no matches, or matches only inside `server/ts-protocol/src/pipeline.rs` (the file we're about to delete) and `server/ts-protocol/src/lib.rs` (the `pub mod pipeline;` line).

- [ ] **Step 2: Delete `pipeline.rs`**

```bash
rm server/ts-protocol/src/pipeline.rs
```

- [ ] **Step 3: Remove `pub mod pipeline;` from lib.rs**

Edit `server/ts-protocol/src/lib.rs` — remove the line `pub mod pipeline;`. The final module list should be:

```rust
pub mod de;
pub mod flow;
pub mod http;
pub mod model;
pub mod net;
pub mod stage;
pub mod tcp;
```

- [ ] **Step 4: Verify the full workspace builds and tests pass**

Run: `cd server && cargo build`
Expected: PASS, no warnings about dead code or missing modules.

Run: `cd server && cargo test`
Expected: PASS, all tests (ts-llm stage tests + ts-protocol existing tests + ts-turn integration test + workspace suites).

- [ ] **Step 5: Commit**

```bash
git add server/ts-protocol/src/lib.rs
git rm server/ts-protocol/src/pipeline.rs
git commit -m "chore(ts-protocol): remove old start_pipeline / PipelineConfig

All callers migrated in prior commits. The new stage.rs API is now the
only entry point for starting the protocol parsing stage."
```

---

## Task 9: End-to-end verification

**Files:** none (verification only)

- [ ] **Step 1: Clean build**

Run: `cd server && cargo clean && cargo build --release`
Expected: PASS. No warnings (beyond any pre-existing ones unrelated to this refactor).

- [ ] **Step 2: Full test suite**

Run: `cd server && cargo test`
Expected: PASS for every test.

- [ ] **Step 3: Manual pcap replay parity (if fixture available)**

If `testdata/pcaps/claude-cli-messages.pcap` (or any other fixture) exists:

Run pre-refactor (checkout parent of first refactor commit) and capture DB output, then post-refactor, and diff the `llm_calls` / `llm_turns` / `llm_metrics` contents. Per spec, row *content* (not `id`) must match with `worker_count = 1`.

A lighter-weight manual check: run `cargo run -p tokenscope -- --pcap-file testdata/pcaps/<fixture>.pcap` on HEAD and confirm storage inserts without panics.

If no fixture is present, note this in the PR description: "pcap replay parity check deferred — no fixture in testdata/pcaps/."

- [ ] **Step 4: Confirm shutdown cascade works on live capture**

If a live interface is available (optional for CI but required before merge on a dev machine):

```bash
cd server && cargo run -p tokenscope -- -i lo0 --bpf-filter 'tcp port 8080'
```

Let it idle for 5 seconds, then Ctrl+C. Expected in stderr logs:

```
received Ctrl+C, stopping...
waiting for storage buffers to flush...
storage buffers flushed
tokenscope stopped
```

No orphaned-task warnings, no panic. If the `capture sources did not stop in time` warning fires, that indicates a regression in the shutdown cascade — investigate before merging.

- [ ] **Step 5: Update spec's "only caller" risk note**

The spec's risk note in `docs/superpowers/specs/2026-04-13-pipeline-phase1-llm-parallelization-design.md` currently says "The only caller today is `app/tokenscope/src/main.rs`". That was wrong — `ts-turn/tests/integration.rs` was also a caller. Update the risk note to reflect reality:

Replace:
```
The only caller today is `app/tokenscope/src/main.rs`, updated in the same change.
```
with:
```
The two callers today — `app/tokenscope/src/main.rs` and `server/ts-turn/tests/integration.rs` — are updated in the same PR.
```

- [ ] **Step 6: Commit spec note fix**

```bash
git add docs/superpowers/specs/2026-04-13-pipeline-phase1-llm-parallelization-design.md
git commit -m "docs: fix Phase 1 spec risk note (integration test was also a caller)"
```

---

## Acceptance Summary

This plan delivers the Phase 1 spec when:

1. **Existing unit tests pass unchanged** — Task 9 Step 2
2. **New `spawn_llm_stage` unit tests pass** — Tasks 2, 3, 4 (single receiver, N=4 parallel, clean shutdown)
3. **Pcap replay parity with `worker_count = 1`** — Task 9 Step 3 (manual if fixture exists)
4. **Multi-worker replay consistency** — verifiable by running Task 9 Step 3 with `worker_count = 4` after editing config; row content must match the `worker_count = 1` run
5. **Graceful shutdown on Ctrl+C** — Task 9 Step 4

## What This Plan Does NOT Do

- Shard `TurnTracker` / `MetricsAggregator` — **Phase 2**
- Per-source ingress isolation — **Phase 3**
- CPU-aware default worker count — **Phase 4**
- Storage read/write pool split — **Phase 5**
- Introduce `metrics_sys` into `spawn_llm_stage` — deferred; add when the first LLM-stage metric is defined
- Change `LlmCall.id` format — IDs may differ between worker-count values (contents do not)
