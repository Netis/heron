# Pipeline Health Page Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the pipeline's internal metrics (currently logged-only) via a new HTTP API and a gated Console page so developers can diagnose backpressure, throughput drops, and runtime errors at a glance.

**Architecture:** Backend stays stateless — `MetricsSvc::snapshot()` is rendered to JSON on each request. The Console polls every 1–5s, computes per-second deltas client-side from consecutive frames, and renders 5 sections (backpressure topology, throughput funnel, state gauges, error red-list, fallback table). A separate `/api/server-info` endpoint exposes a `console.features.pipeline_health` flag the Console reads at boot to gate sidebar visibility and direct-URL access.

**Tech Stack:** Rust (axum, serde, tokio), `ts-common::internal_metrics`, React 19 + TypeScript, TanStack Query, Zustand, Tailwind, recharts (existing — not used here).

**Reference spec:** `docs/superpowers/specs/2026-04-28-pipeline-health-page-design.md`

---

## File Map

### Backend (Rust)

| File | Action | Responsibility |
|---|---|---|
| `server/config/default.toml` | Modify | Add `[console.features]` section |
| `server/ts-common/src/config.rs` | Modify | Add `ConsoleConfig`/`ConsoleFeatures` structs and field on `AppConfig` |
| `server/ts-api/src/lib.rs` | Modify | Extend `router()` signature with metrics + server-info contexts |
| `server/ts-api/src/routes/mod.rs` | Modify | Register `internal_metrics` and `server_info` modules |
| `server/ts-api/src/routes/internal_metrics.rs` | Create | `GET /api/internal-metrics` handler |
| `server/ts-api/src/routes/server_info.rs` | Create | `GET /api/server-info` handler |
| `server/app/tokenscope/src/main.rs` | Modify | Build contexts and pass to `ts_api::router(...)` |

### Frontend (TypeScript)

| File | Action | Responsibility |
|---|---|---|
| `console/src/types/api.ts` | Modify | Add `ServerInfo`, `MetricSnapshot`, `MetricRecord`, `MetricGroup`, `PipelineSnapshot` types |
| `console/src/hooks/use-server-info.ts` | Create | Single boot-time fetch of `/api/server-info` |
| `console/src/hooks/use-internal-metrics.ts` | Create | Polling hook for `/api/internal-metrics`, interval driven by store |
| `console/src/stores/pipeline-health.ts` | Create | Zustand: `intervalMs` (1000/2000/5000/null) + `selectedPipeline` |
| `console/src/lib/pipeline-health.ts` | Create | Pure functions: health classification, funnel stage spec, drop annotations |
| `console/src/lib/pipeline-health.test.ts` | Create | Unit tests for the lib |
| `console/src/pages/pipeline-health.tsx` | Create | Page shell (header + 5 sections) |
| `console/src/components/pipeline-health/backpressure-section.tsx` | Create | Section ① — queue topology |
| `console/src/components/pipeline-health/funnel-section.tsx` | Create | Section ② — throughput funnel |
| `console/src/components/pipeline-health/state-gauges-section.tsx` | Create | Section ③ — state KPI cards |
| `console/src/components/pipeline-health/error-list-section.tsx` | Create | Section ④ — error red-list |
| `console/src/components/pipeline-health/all-metrics-table.tsx` | Create | Section ⑤ — collapsible full table |
| `console/src/components/pipeline-health/health-pill.tsx` | Create | Reusable green/yellow/red status pill |
| `console/src/app.tsx` | Modify | Mount `<Route path="/pipeline-health">` |
| `console/src/components/layout/sidebar.tsx` | Modify | Conditionally render the nav item |

---

## Phase 1 — Backend foundation

### Task 1: Add `ConsoleConfig` to `ts-common`

**Files:**
- Modify: `server/ts-common/src/config.rs`

- [ ] **Step 1: Write the failing test**

Append to the bottom of the existing `phase2_tests` module in `server/ts-common/src/config.rs` (just before the closing `}`):

```rust
    #[test]
    fn console_features_pipeline_health_defaults_false() {
        let cfg = AppConfig::from_toml("");
        assert!(!cfg.console.features.pipeline_health);
    }

    #[test]
    fn console_features_parses_pipeline_health_true() {
        let toml = r#"
            [console.features]
            pipeline_health = true
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert!(cfg.console.features.pipeline_health);
    }

    #[test]
    fn console_section_partial_unknown_keys_ignored() {
        // Forward-compat: an old binary should not break when newer config
        // adds unrelated keys under [console] or [console.features].
        let toml = r#"
            [console.features]
            pipeline_health = true
            future_flag = "ignored"
        "#;
        let cfg = AppConfig::from_toml(toml);
        assert!(cfg.console.features.pipeline_health);
    }
```

- [ ] **Step 2: Run tests and confirm they fail to compile**

```
cargo test -p ts-common --lib console_
```

Expected: build error — `AppConfig` has no field named `console`, `ConsoleConfig`/`ConsoleFeatures` not in scope.

- [ ] **Step 3: Add the structs and the `AppConfig` field**

In `server/ts-common/src/config.rs`:

(a) Add to `AppConfig` struct, just under the `pub api: ApiConfig,` line:

```rust
    pub console: ConsoleConfig,
```

(b) Add to `RawAppConfig` (right after the existing `api` field):

```rust
    #[serde(default)]
    console: ConsoleConfig,
```

(c) Add to `RawAppConfig::resolve()`'s returned `AppConfig { ... }` literal, just after `api: self.api,`:

```rust
            console: self.console,
```

(d) Add the new structs anywhere in the file after `ApiConfig` (e.g. right before `pub struct TurnConfig`):

```rust
/// Console UI flags surfaced to the React app via `/api/server-info`.
///
/// Forward-compat: deliberately does NOT use `#[serde(deny_unknown_fields)]`
/// so a newer Console writing future fields here doesn't break an older binary.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ConsoleConfig {
    #[serde(default)]
    pub features: ConsoleFeatures,
}

/// Feature flags read by the Console at boot. New flags default to `false`
/// so a newer Console hitting an older binary degrades safely.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ConsoleFeatures {
    /// Show the developer-only "Pipeline Health" page.
    #[serde(default)]
    pub pipeline_health: bool,
}
```

- [ ] **Step 4: Run tests and confirm they pass**

```
cargo test -p ts-common --lib console_
```

Expected: 3 tests pass. Also run the full `ts-common` test set:

```
cargo test -p ts-common
```

Expected: all pre-existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add server/ts-common/src/config.rs
git commit -m "feat(config): add [console.features] pipeline_health flag"
```

---

### Task 2: Wire `[console.features]` into `default.toml`

**Files:**
- Modify: `server/config/default.toml`

- [ ] **Step 1: Add the new section**

Append at the end of `server/config/default.toml`:

```toml

# ---------------------------------------------------------------------------
# Console UI feature flags (surfaced to the SPA via /api/server-info)
# ---------------------------------------------------------------------------
[console.features]
# Show the developer-only "Pipeline Health" page in the Console nav.
# Default off — it exposes pipeline-internal counters/queues that are not
# meant for end users. Flip to `true` when diagnosing capture/protocol/
# storage issues.
pipeline_health = false
```

- [ ] **Step 2: Smoke-test the load**

```
cargo run -p tokenscope -- --config server/config/default.toml --help 2>&1 | head -3
```

Expected: prints CLI help. (We're checking the binary still parses the config crate; a parse error would be flagged when commands run, not on `--help`. To exercise loading, instead run:)

```
cargo test -p ts-common --lib
```

Expected: all tests pass — including the new console tests.

- [ ] **Step 3: Commit**

```bash
git add server/config/default.toml
git commit -m "config: register [console.features] in default.toml"
```

---

### Task 3: Define API context structs in `ts-api`

**Files:**
- Modify: `server/ts-api/src/lib.rs`

- [ ] **Step 1: Add the context structs**

Open `server/ts-api/src/lib.rs`. Just below the `use` block (before `pub async fn bind`), add:

```rust
use std::sync::Arc as _Arc; // already in scope; this comment is a marker — do not actually add this line
```

(The `Arc` import already exists; you just need to add the context structs and a metrics-system import. Replace the existing import line `use std::sync::Arc;` with the block below if it isn't already grouped:)

```rust
use std::sync::Arc;

use ts_common::config::ConsoleFeatures;
use ts_common::internal_metrics::MetricsSvc;
```

Then, immediately after the `use` block and before `pub async fn bind`, add:

```rust
/// Carriers for `/api/internal-metrics` — every per-pipeline `MetricsSvc`
/// plus the cross-pipeline (storage) one. Build this in `main.rs` after
/// `MetricsSystem::start()`.
#[derive(Clone)]
pub struct ApiMetricsContext {
    pub pipelines: Vec<(String, Arc<MetricsSvc>)>,
    pub global: Arc<MetricsSvc>,
}

/// Carrier for `/api/server-info`.
#[derive(Clone)]
pub struct ServerInfoContext {
    pub version: &'static str,
    pub console_features: ConsoleFeatures,
}
```

- [ ] **Step 2: Verify it compiles**

```
cargo check -p ts-api
```

Expected: clean build (the structs are unused but that's fine; we'll wire them up in Task 4–6).

- [ ] **Step 3: Commit**

```bash
git add server/ts-api/src/lib.rs
git commit -m "feat(ts-api): add ApiMetricsContext and ServerInfoContext"
```

---

### Task 4: Implement `GET /api/server-info`

**Files:**
- Create: `server/ts-api/src/routes/server_info.rs`
- Modify: `server/ts-api/src/routes/mod.rs`
- Modify: `server/ts-api/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `server/ts-api/src/routes/server_info.rs` with:

```rust
//! `GET /api/server-info` — version + Console feature flags.

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::response::ApiResponse;
use crate::ServerInfoContext;

#[derive(Serialize)]
struct ServerInfoResponse {
    version: &'static str,
    console: ConsoleSection,
}

#[derive(Serialize)]
struct ConsoleSection {
    features: ts_common::config::ConsoleFeatures,
}

pub async fn server_info(State(ctx): State<ServerInfoContext>) -> impl IntoResponse {
    ApiResponse::ok(ServerInfoResponse {
        version: ctx.version,
        console: ConsoleSection {
            features: ctx.console_features,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use ts_common::config::ConsoleFeatures;

    #[tokio::test]
    async fn returns_version_and_features() {
        let ctx = ServerInfoContext {
            version: "9.9.9",
            console_features: ConsoleFeatures {
                pipeline_health: true,
            },
        };
        let resp = server_info(State(ctx)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 0);
        assert_eq!(v["data"]["version"], "9.9.9");
        assert_eq!(v["data"]["console"]["features"]["pipeline_health"], true);
    }

    #[tokio::test]
    async fn defaults_render_pipeline_health_false() {
        let ctx = ServerInfoContext {
            version: "0.0.0",
            console_features: ConsoleFeatures::default(),
        };
        let resp = server_info(State(ctx)).await.into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"]["console"]["features"]["pipeline_health"], false);
    }
}
```

Add `pub mod server_info;` to `server/ts-api/src/routes/mod.rs` (preserve alphabetical ordering — insert after `pub mod metrics;`):

```rust
pub mod agent_sessions;
pub mod agent_turns;
pub mod filters;
pub mod http_exchanges;
pub mod llm_calls;
pub mod metrics;
pub mod server_info;
```

- [ ] **Step 2: Run the test to confirm it fails**

```
cargo test -p ts-api routes::server_info::tests --lib
```

Expected: fail or not-compile. The handler exists but is unused at the router level; the in-file unit tests should compile and pass once `ConsoleFeatures` derives `Serialize` (we already added that in Task 1) and `ApiResponse::ok` accepts the typed payload.

If both tests pass already, that's fine — proceed. The next step is to mount the route.

- [ ] **Step 3: Mount the route in `router()`**

In `server/ts-api/src/lib.rs`, change the `pub fn router(...)` signature and body. Replace the existing function with:

```rust
/// Build the API router (without serving). Useful for composing with other layers.
pub fn router(
    storage: Arc<dyn StorageBackend>,
    server_info: ServerInfoContext,
) -> Router {
    let metrics_routes = Router::new()
        .route("/api/server-info", get(routes::server_info::server_info))
        .with_state(server_info);

    Router::new()
        .route("/api/filters/wire-apis", get(routes::filters::wire_apis))
        .route("/api/filters/models", get(routes::filters::models))
        .route("/api/filters/server-ips", get(routes::filters::server_ips))
        .route(
            "/api/filters/finish-reasons",
            get(routes::filters::finish_reasons),
        )
        .route("/api/metrics/timeseries", get(routes::metrics::timeseries))
        .route("/api/metrics/summary", get(routes::metrics::summary))
        .route("/api/metrics/models", get(routes::metrics::models))
        .route(
            "/api/metrics/finish-reasons",
            get(routes::metrics::finish_reasons),
        )
        .route("/api/llm-calls", get(routes::llm_calls::list))
        .route("/api/llm-calls/{id}", get(routes::llm_calls::detail))
        .route("/api/http-exchanges", get(routes::http_exchanges::list))
        .route(
            "/api/http-exchanges/{id}",
            get(routes::http_exchanges::detail),
        )
        .route("/api/agent-turns", get(routes::agent_turns::list))
        .route("/api/agent-turns/{id}", get(routes::agent_turns::detail))
        .route(
            "/api/agent-turns/{id}/calls",
            get(routes::agent_turns::calls),
        )
        .route("/api/agent-sessions", get(routes::agent_sessions::list))
        .route(
            "/api/agent-sessions/{source_id}/{session_id}",
            get(routes::agent_sessions::detail),
        )
        .route(
            "/api/agent-sessions/{source_id}/{session_id}/turns",
            get(routes::agent_sessions::turns),
        )
        .with_state(storage)
        .merge(metrics_routes)
        .layer(CorsLayer::permissive())
}
```

> Note: we route the storage-backed handlers under one `with_state` layer and the new contexts under separate sub-routers, then `merge`. Axum's typed-state requires this split because the new endpoints don't take `Arc<dyn StorageBackend>`.

- [ ] **Step 4: Run handler tests**

```
cargo test -p ts-api routes::server_info::tests --lib
```

Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add server/ts-api/src/routes/mod.rs server/ts-api/src/routes/server_info.rs server/ts-api/src/lib.rs
git commit -m "feat(ts-api): GET /api/server-info exposes console feature flags"
```

---

### Task 5: Implement `GET /api/internal-metrics`

**Files:**
- Create: `server/ts-api/src/routes/internal_metrics.rs`
- Modify: `server/ts-api/src/routes/mod.rs`
- Modify: `server/ts-api/src/lib.rs`

- [ ] **Step 1: Write the handler with unit test**

Create `server/ts-api/src/routes/internal_metrics.rs`:

```rust
//! `GET /api/internal-metrics` — current snapshot of every registered
//! internal metric across all pipelines plus the global (storage) view.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use ts_common::internal_metrics::{Metric, MetricKind, MetricsSvc};

use crate::response::ApiResponse;
use crate::ApiMetricsContext;

#[derive(Serialize)]
struct InternalMetricsResponse {
    ts: i64,
    pipelines: Vec<PipelineSnapshot>,
    global: SnapshotPayload,
}

#[derive(Serialize)]
struct PipelineSnapshot {
    name: String,
    #[serde(flatten)]
    snapshot: SnapshotPayload,
}

#[derive(Serialize)]
struct SnapshotPayload {
    metrics: Vec<MetricRecord>,
}

#[derive(Serialize)]
struct MetricRecord {
    name: &'static str,
    group: &'static str,
    kind: &'static str,
    value: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    capacity: Option<u64>,
}

fn render_snapshot(svc: &MetricsSvc) -> SnapshotPayload {
    svc.sample_probes();
    let snap = svc.snapshot();
    let caps = svc.capacities();
    let metrics = snap
        .values
        .iter()
        .map(|(metric, &value)| {
            let spec = metric.spec();
            let kind = match spec.kind {
                MetricKind::Counter => "counter",
                MetricKind::Gauge => "gauge",
            };
            MetricRecord {
                name: spec.short_name,
                group: spec.group.as_str(),
                kind,
                value,
                capacity: caps.get(metric).copied(),
            }
        })
        .collect::<Vec<_>>();
    SnapshotPayload { metrics }
}

pub async fn internal_metrics(State(ctx): State<ApiMetricsContext>) -> impl IntoResponse {
    let pipelines = ctx
        .pipelines
        .iter()
        .map(|(name, svc)| PipelineSnapshot {
            name: name.clone(),
            snapshot: render_snapshot(svc),
        })
        .collect::<Vec<_>>();
    let global = render_snapshot(&ctx.global);

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    ApiResponse::ok(InternalMetricsResponse {
        ts,
        pipelines,
        global,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use ts_common::internal_metrics::{Metric, MetricsSystem};

    fn build_pipeline_svc() -> Arc<MetricsSvc> {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[Metric::CapturePacketsReceived, Metric::NetPacketsParsed],
        );
        sys.register_queue_probe_capped(Metric::QueueDepthRaw, 4096, || 4000);
        let svc = sys.start();
        w.counter(Metric::CapturePacketsReceived).add(123);
        w.counter(Metric::NetPacketsParsed).add(120);
        svc
    }

    fn build_global_svc() -> Arc<MetricsSvc> {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker("storage", &[Metric::StorageFlushedCalls]);
        sys.register_queue_probe_capped(Metric::StorageQueueDepthCalls, 1024, || 7);
        let svc = sys.start();
        w.counter(Metric::StorageFlushedCalls).add(50);
        svc
    }

    #[tokio::test]
    async fn snapshot_includes_pipeline_and_global() {
        let ctx = ApiMetricsContext {
            pipelines: vec![("default".to_string(), build_pipeline_svc())],
            global: build_global_svc(),
        };
        let resp = internal_metrics(State(ctx)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 0);
        assert!(v["data"]["ts"].as_i64().unwrap() > 0);

        let pipelines = v["data"]["pipelines"].as_array().unwrap();
        assert_eq!(pipelines.len(), 1);
        assert_eq!(pipelines[0]["name"], "default");

        let metrics = pipelines[0]["metrics"].as_array().unwrap();
        let pkts = metrics
            .iter()
            .find(|m| m["name"] == "pkts_received")
            .expect("pkts_received in pipeline snapshot");
        assert_eq!(pkts["value"], 123);
        assert_eq!(pkts["kind"], "counter");
        assert_eq!(pkts["group"], "capture");
        assert!(pkts.get("capacity").map(|c| c.is_null()).unwrap_or(true));

        let q_raw = metrics
            .iter()
            .find(|m| m["name"] == "q_raw_pkts")
            .expect("q_raw_pkts in pipeline snapshot");
        assert_eq!(q_raw["value"], 4000);
        assert_eq!(q_raw["capacity"], 4096);
        assert_eq!(q_raw["kind"], "gauge");

        let global_metrics = v["data"]["global"]["metrics"].as_array().unwrap();
        let flushed = global_metrics
            .iter()
            .find(|m| m["name"] == "flushed_calls")
            .expect("flushed_calls in global snapshot");
        assert_eq!(flushed["value"], 50);
    }

    #[tokio::test]
    async fn empty_pipelines_yields_empty_array() {
        let ctx = ApiMetricsContext {
            pipelines: vec![],
            global: build_global_svc(),
        };
        let resp = internal_metrics(State(ctx)).await.into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"]["pipelines"].as_array().unwrap().len(), 0);
        assert!(!v["data"]["global"]["metrics"].as_array().unwrap().is_empty());
    }
}
```

Add `pub mod internal_metrics;` to `server/ts-api/src/routes/mod.rs` so the module list reads:

```rust
pub mod agent_sessions;
pub mod agent_turns;
pub mod filters;
pub mod http_exchanges;
pub mod internal_metrics;
pub mod llm_calls;
pub mod metrics;
pub mod server_info;
```

- [ ] **Step 2: Run tests and confirm they fail**

```
cargo test -p ts-api routes::internal_metrics::tests --lib
```

Expected: tests compile and pass (the handler is self-contained). If they don't compile, fix the import paths.

- [ ] **Step 3: Mount the route in `router()`**

In `server/ts-api/src/lib.rs`, update the function signature again to accept the metrics context, and merge the new route into the metrics-state sub-router. Replace the `pub fn router(...)` with:

```rust
pub fn router(
    storage: Arc<dyn StorageBackend>,
    metrics: ApiMetricsContext,
    server_info: ServerInfoContext,
) -> Router {
    let server_info_routes = Router::new()
        .route("/api/server-info", get(routes::server_info::server_info))
        .with_state(server_info);

    let internal_metrics_routes = Router::new()
        .route(
            "/api/internal-metrics",
            get(routes::internal_metrics::internal_metrics),
        )
        .with_state(metrics);

    Router::new()
        .route("/api/filters/wire-apis", get(routes::filters::wire_apis))
        .route("/api/filters/models", get(routes::filters::models))
        .route("/api/filters/server-ips", get(routes::filters::server_ips))
        .route(
            "/api/filters/finish-reasons",
            get(routes::filters::finish_reasons),
        )
        .route("/api/metrics/timeseries", get(routes::metrics::timeseries))
        .route("/api/metrics/summary", get(routes::metrics::summary))
        .route("/api/metrics/models", get(routes::metrics::models))
        .route(
            "/api/metrics/finish-reasons",
            get(routes::metrics::finish_reasons),
        )
        .route("/api/llm-calls", get(routes::llm_calls::list))
        .route("/api/llm-calls/{id}", get(routes::llm_calls::detail))
        .route("/api/http-exchanges", get(routes::http_exchanges::list))
        .route(
            "/api/http-exchanges/{id}",
            get(routes::http_exchanges::detail),
        )
        .route("/api/agent-turns", get(routes::agent_turns::list))
        .route("/api/agent-turns/{id}", get(routes::agent_turns::detail))
        .route(
            "/api/agent-turns/{id}/calls",
            get(routes::agent_turns::calls),
        )
        .route("/api/agent-sessions", get(routes::agent_sessions::list))
        .route(
            "/api/agent-sessions/{source_id}/{session_id}",
            get(routes::agent_sessions::detail),
        )
        .route(
            "/api/agent-sessions/{source_id}/{session_id}/turns",
            get(routes::agent_sessions::turns),
        )
        .with_state(storage)
        .merge(server_info_routes)
        .merge(internal_metrics_routes)
        .layer(CorsLayer::permissive())
}
```

- [ ] **Step 4: Run all `ts-api` tests**

```
cargo test -p ts-api
```

Expected: all tests pass (existing + 2 new from server_info + 2 new from internal_metrics).

- [ ] **Step 5: Commit**

```bash
git add server/ts-api/src/routes/mod.rs server/ts-api/src/routes/internal_metrics.rs server/ts-api/src/lib.rs
git commit -m "feat(ts-api): GET /api/internal-metrics returns full snapshot"
```

---

### Task 6: Wire context construction into `main.rs`

**Files:**
- Modify: `server/app/tokenscope/src/main.rs`

- [ ] **Step 1: Identify the construction point**

Open `server/app/tokenscope/src/main.rs`. Today:

- `ts_api::bind()` is called near line ~233 (inside the `if let Ok(listener) = ts_api::bind(&config.api).await { ... }` arm).
- `ts_api::router(api_storage)` is called immediately after, on line 243, and `axum::serve(...)` is spawned right there.
- Per-pipeline `MetricsSystem::start()` happens **later**, around lines 342–380, when `pipeline_reporter_handles` and `global_reporter_handle` are constructed.

We must invert this: keep `ts_api::bind()` early so port-bind errors abort startup fast, but move `ts_api::router(...)` + `tokio::spawn(serve)` to **after** the metrics finalization so the new `ApiMetricsContext` has populated `Arc<MetricsSvc>` values.

- [ ] **Step 2: Replace the existing bind + spawn block with just the bind**

Locate the block that today reads:

```rust
    let api_handle = match ts_api::bind(&config.api).await {
        Ok(listener) => {
            let api_storage = storage.clone();
            let api_cancel = cancel.clone();
            Some(tokio::spawn(async move {
                let router = ts_api::router(api_storage);
                #[cfg(feature = "console")]
                let router = router.fallback(console::static_handler);
                let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                    api_cancel.cancelled().await;
                });
                if let Err(e) = server.await {
                    tracing::error!("API server error: {e}");
                } else {
                    tracing::info!("API server stopped");
                }
            }))
        }
        Err(e) => {
            tracing::warn!("API server disabled: {e}");
            None
        }
    };
```

Replace the entire block with **only** the bind, holding the listener for later:

```rust
    let api_listener: Option<tokio::net::TcpListener> = match ts_api::bind(&config.api).await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!("API server disabled: {e}");
            None
        }
    };
    let mut api_handle: Option<tokio::task::JoinHandle<()>> = None;
```

The `api_handle` declaration is `mut` and starts as `None` — we'll assign to it once metrics are finalized. Anywhere `api_handle` is later awaited during shutdown stays unchanged.

- [ ] **Step 3: Spawn the API server after metrics finalization**

After the existing block that finalizes metrics (where `pipeline_reporter_handles` and `global_reporter_handle` are constructed — around lines 342–380), but *before* the `let mut capture_tasks: JoinSet<()> = JoinSet::new();` that follows, gather the same `MetricsSvc` Arcs into the API context. The reporter blocks consume the `MetricsSystem` via `sys.start()` which returns `Arc<MetricsSvc>`. We need that Arc again for the API.

Restructure the per-pipeline reporter loop to also collect `(pipeline_name, Arc<MetricsSvc>)` for the API. Replace the existing `let pipeline_reporter_handles: Vec<_> = per_pipeline_metrics.into_iter().zip(...).filter_map(...).collect();` with:

```rust
        let mut api_pipeline_metrics: Vec<(String, std::sync::Arc<ts_common::internal_metrics::MetricsSvc>)> = Vec::new();
        let pipeline_reporter_handles: Vec<_> = per_pipeline_metrics
            .into_iter()
            .zip(effective_pipelines.iter())
            .filter_map(|(sys, def)| {
                let svc = sys.start();
                api_pipeline_metrics.push((def.name.clone(), svc.clone()));
                reporter_enabled.then(|| {
                    let label = format!("pipeline.{}", def.name);
                    let handle = MetricsReporter::start(
                        svc,
                        &label,
                        Duration::from_secs(config.internal_metrics.interval_secs),
                    );
                    tracing::info!(
                        "internal metrics reporter started for {label} (interval={}s)",
                        config.internal_metrics.interval_secs
                    );
                    handle
                })
            })
            .collect();
```

Then capture the global svc Arc similarly. Replace:

```rust
        let global_reporter_handle = {
            let svc = shared_metrics.start();
            reporter_enabled.then(|| {
                let handle = MetricsReporter::start(
                    svc,
                    "global",
                    Duration::from_secs(config.internal_metrics.interval_secs),
                );
                ...
            })
        };
```

with:

```rust
        let api_global_metrics = shared_metrics.start();
        let global_reporter_handle = {
            let svc = api_global_metrics.clone();
            reporter_enabled.then(|| {
                let handle = MetricsReporter::start(
                    svc,
                    "global",
                    Duration::from_secs(config.internal_metrics.interval_secs),
                );
                tracing::info!(
                    "internal metrics reporter started for global (interval={}s)",
                    config.internal_metrics.interval_secs
                );
                handle
            })
        };
```

Now spawn the API server, immediately after the global reporter block. Note we *assign* to the previously-declared `mut api_handle` (no `let`):

```rust
        api_handle = match api_listener {
            Some(listener) => {
                let api_storage = storage.clone();
                let api_cancel = cancel.clone();
                let api_metrics = ts_api::ApiMetricsContext {
                    pipelines: api_pipeline_metrics,
                    global: api_global_metrics.clone(),
                };
                let api_server_info = ts_api::ServerInfoContext {
                    version: env!("CARGO_PKG_VERSION"),
                    console_features: config.console.features.clone(),
                };
                Some(tokio::spawn(async move {
                    let router = ts_api::router(api_storage, api_metrics, api_server_info);
                    #[cfg(feature = "console")]
                    let router = router.fallback(console::static_handler);
                    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                        api_cancel.cancelled().await;
                    });
                    if let Err(e) = server.await {
                        tracing::error!("API server error: {e}");
                    } else {
                        tracing::info!("API server stopped");
                    }
                }))
            }
            None => None,
        };
```

> Make sure to remove any leftover earlier definition of `api_handle`. There must be exactly one binding so the `await` on shutdown sees the right handle.

- [ ] **Step 4: Build the workspace**

```
cargo build --workspace
```

Expected: clean build. Fix any imports — likely you'll need `use std::sync::Arc;` already present, and the `ts_api::{ApiMetricsContext, ServerInfoContext}` types reachable.

- [ ] **Step 5: Run all tests**

```
just test all
```

Expected: green. The pipeline e2e tests should not regress.

- [ ] **Step 6: Smoke run**

Spin up the binary briefly:

```
cargo run -p tokenscope -- --config server/config/default.toml &
sleep 2
curl -s http://localhost:8080/api/server-info | head -c 200
echo
curl -s http://localhost:8080/api/internal-metrics | head -c 400
echo
kill %1
```

Expected: both endpoints return JSON. `server-info` shows `"pipeline_health": false`. `internal-metrics` returns whatever metrics the running pipeline registered (may be empty `metrics` arrays if no traffic flowing — that's fine).

- [ ] **Step 7: Commit**

```bash
git add server/app/tokenscope/src/main.rs
git commit -m "feat(app): wire MetricsSvc + console flags into the API"
```

---

## Phase 2 — Frontend types, hooks, store

### Task 7: Add API response types

**Files:**
- Modify: `console/src/types/api.ts`

- [ ] **Step 1: Add the new types**

Append to `console/src/types/api.ts`:

```ts
// ============================================================================
// /api/server-info
// ============================================================================

export type ConsoleFeatures = {
  pipeline_health?: boolean
}

export type ServerInfo = {
  version: string
  console: {
    features: ConsoleFeatures
  }
}

// ============================================================================
// /api/internal-metrics
// ============================================================================

export type MetricGroup =
  | "capture"
  | "protocol"
  | "llm"
  | "turn"
  | "metrics"
  | "storage"

export type MetricKind = "counter" | "gauge"

export type MetricRecord = {
  name: string
  group: MetricGroup
  kind: MetricKind
  value: number
  capacity?: number
}

export type PipelineMetricsSnapshot = {
  name: string
  metrics: MetricRecord[]
}

export type InternalMetricsResponse = {
  ts: number
  pipelines: PipelineMetricsSnapshot[]
  global: { metrics: MetricRecord[] }
}
```

- [ ] **Step 2: Confirm typecheck**

```
cd console && bun run tsc -b --noEmit
```

Expected: clean. (No code consumes these types yet.)

- [ ] **Step 3: Commit**

```bash
git add console/src/types/api.ts
git commit -m "feat(console): types for server-info and internal-metrics"
```

---

### Task 8: `useServerInfo` hook

**Files:**
- Create: `console/src/hooks/use-server-info.ts`

- [ ] **Step 1: Write the hook**

```ts
import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import type { ServerInfo } from "@/types/api"

/**
 * Fetch server info once at boot. Result feeds:
 *   - sidebar visibility for developer-only pages
 *   - per-page guards (e.g. /pipeline-health redirects when feature off)
 *
 * `staleTime: Infinity` — server feature flags change only on restart,
 * so we never need to refetch within a session.
 */
export function useServerInfo() {
  return useQuery({
    queryKey: ["server-info"],
    queryFn: () => apiFetch<ServerInfo>("/api/server-info"),
    staleTime: Infinity,
    retry: 0,
  })
}
```

- [ ] **Step 2: Typecheck**

```
cd console && bun run tsc -b --noEmit
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add console/src/hooks/use-server-info.ts
git commit -m "feat(console): useServerInfo hook"
```

---

### Task 9: `usePipelineHealthStore` + `useInternalMetrics`

**Files:**
- Create: `console/src/stores/pipeline-health.ts`
- Create: `console/src/hooks/use-internal-metrics.ts`

- [ ] **Step 1: Write the store**

`console/src/stores/pipeline-health.ts`:

```ts
import { create } from "zustand"

type PipelineHealthState = {
  /** Polling interval in ms. `null` = paused. */
  intervalMs: number | null
  /** Selected pipeline name. `null` = use the first one returned. */
  selectedPipeline: string | null
  /** All-Metrics table filter chip ("all" | group name). */
  tableGroupFilter: string
  /** All-Metrics table "only ⚠" toggle. */
  tableOnlyWarn: boolean

  setIntervalMs: (ms: number | null) => void
  setSelectedPipeline: (name: string | null) => void
  setTableGroupFilter: (chip: string) => void
  setTableOnlyWarn: (on: boolean) => void
}

export const usePipelineHealthStore = create<PipelineHealthState>((set) => ({
  intervalMs: 2000,
  selectedPipeline: null,
  tableGroupFilter: "all",
  tableOnlyWarn: false,
  setIntervalMs: (intervalMs) => set({ intervalMs }),
  setSelectedPipeline: (selectedPipeline) => set({ selectedPipeline }),
  setTableGroupFilter: (tableGroupFilter) => set({ tableGroupFilter }),
  setTableOnlyWarn: (tableOnlyWarn) => set({ tableOnlyWarn }),
}))
```

- [ ] **Step 2: Write the polling hook**

`console/src/hooks/use-internal-metrics.ts`:

```ts
import { useQuery } from "@tanstack/react-query"
import { apiFetch } from "@/lib/api"
import { usePipelineHealthStore } from "@/stores/pipeline-health"
import type { InternalMetricsResponse } from "@/types/api"

/**
 * Poll /api/internal-metrics. Cadence comes from the store; `null` pauses.
 *
 * The hook is stateless re: deltas — consumers compute deltas using
 * the previous query result (`previousData`) plus the current `ts`.
 */
export function useInternalMetrics() {
  const intervalMs = usePipelineHealthStore((s) => s.intervalMs)

  return useQuery({
    queryKey: ["internal-metrics"],
    queryFn: () => apiFetch<InternalMetricsResponse>("/api/internal-metrics"),
    refetchInterval: intervalMs ?? false,
    refetchIntervalInBackground: false,
    // Keep the previous frame visible during refetches so the page doesn't
    // flicker, and so consumers can compute (current - previous) deltas.
    placeholderData: (prev) => prev,
    retry: 1,
  })
}
```

- [ ] **Step 3: Typecheck**

```
cd console && bun run tsc -b --noEmit
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add console/src/stores/pipeline-health.ts console/src/hooks/use-internal-metrics.ts
git commit -m "feat(console): pipeline-health store + useInternalMetrics polling hook"
```

---

## Phase 3 — Pure logic library (`pipeline-health.ts`)

### Task 10: Health classification (TDD)

**Files:**
- Create: `console/src/lib/pipeline-health.ts`
- Create: `console/src/lib/pipeline-health.test.ts`

- [ ] **Step 1: Write the failing test**

`console/src/lib/pipeline-health.test.ts`:

```ts
import { describe, it, expect } from "vitest"
import { classifyHealth } from "./pipeline-health"
import type { MetricRecord } from "@/types/api"

const counter = (
  name: string,
  group: MetricRecord["group"],
  value: number,
): MetricRecord => ({ name, group, kind: "counter", value })

const cappedGauge = (
  name: string,
  group: MetricRecord["group"],
  value: number,
  capacity: number,
): MetricRecord => ({ name, group, kind: "gauge", value, capacity })

const noPrev: Record<string, number> = {}

describe("classifyHealth", () => {
  it("returns healthy when nothing's wrong", () => {
    const all = [
      counter("pkts_received", "capture", 100),
      counter("pkts_dropped_kernel", "capture", 0),
      cappedGauge("q_raw_pkts", "protocol", 100, 4096),
    ]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("healthy")
    expect(h.findings).toHaveLength(0)
  })

  it("flags critical on kernel drop delta > 0", () => {
    const all = [counter("pkts_dropped_kernel", "capture", 5)]
    const prev = { pkts_dropped_kernel: 2 }
    const h = classifyHealth(all, prev)
    expect(h.level).toBe("critical")
    expect(h.findings.some((f) => f.metric === "pkts_dropped_kernel")).toBe(true)
  })

  it("flags critical when any capped gauge >= 95%", () => {
    const all = [cappedGauge("q_raw_pkts", "protocol", 3900, 4096)]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("critical")
    expect(h.findings.some((f) => f.metric === "q_raw_pkts")).toBe(true)
  })

  it("flags warning when capped gauge >= 90% but < 95%", () => {
    const all = [cappedGauge("q_raw_pkts", "protocol", 3700, 4096)]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("warning")
  })

  it("flags warning when tcp_ooo_dropped delta > 0", () => {
    const all = [counter("tcp_ooo_dropped", "protocol", 7)]
    const prev = { tcp_ooo_dropped: 4 }
    const h = classifyHealth(all, prev)
    expect(h.level).toBe("warning")
  })

  it("flags warning on sticky cumulative > 0 even with delta = 0", () => {
    const all = [counter("tcp_ooo_dropped", "protocol", 7)]
    const prev = { tcp_ooo_dropped: 7 }
    const h = classifyHealth(all, prev)
    expect(h.level).toBe("warning")
  })

  it("critical wins over warning", () => {
    const all = [
      cappedGauge("q_raw_pkts", "protocol", 4000, 4096), // critical
      counter("tcp_ooo_dropped", "protocol", 5), // warning
    ]
    const h = classifyHealth(all, noPrev)
    expect(h.level).toBe("critical")
  })
})
```

- [ ] **Step 2: Run the test to confirm it fails**

```
cd console && bun test pipeline-health.test
```

Expected: FAIL — module `./pipeline-health` does not export `classifyHealth`.

> If `bun test` is not the project's runner, use `bun x vitest run src/lib/pipeline-health.test.ts`. The console uses Vitest via the existing `turn-index.test.ts` — run whatever invocation matches that file's existing test command. If unsure, run `bun run` to see the script list, or `bunx vitest run`.

- [ ] **Step 3: Write the minimal implementation**

`console/src/lib/pipeline-health.ts`:

```ts
import type { MetricRecord } from "@/types/api"

export type HealthLevel = "healthy" | "warning" | "critical"

export type HealthFinding = {
  level: "warning" | "critical"
  metric: string
  message: string
}

export type Health = {
  level: HealthLevel
  findings: HealthFinding[]
}

const CRITICAL_DELTA_COUNTERS = new Set([
  "pkts_dropped_kernel",
  "flush_errors",
  "read_errors",
  "dump_errors",
  "batches_dropped_zmq",
])

const WARNING_DELTA_COUNTERS = new Set([
  "tcp_ooo_dropped",
  "http_resyncs",
  "turns_discarded_no_user_start",
  "calls_dropped_late",
  "heartbeats_dropped",
])

const WARNING_THRESHOLD = 0.9
const CRITICAL_THRESHOLD = 0.95

/**
 * Classify the current snapshot into healthy/warning/critical.
 *
 * `prev` is a `metric_name -> previous_value` lookup used to detect deltas.
 * Pass `{}` on the first frame — no critical/warning will be produced for
 * delta-based rules until a second frame arrives, but cumulative rules
 * still fire (e.g. tcp_ooo_dropped > 0 stays warning even on first frame).
 */
export function classifyHealth(
  metrics: MetricRecord[],
  prev: Record<string, number>,
): Health {
  const findings: HealthFinding[] = []

  for (const m of metrics) {
    const delta = m.value - (prev[m.name] ?? m.value)

    if (m.kind === "counter") {
      if (CRITICAL_DELTA_COUNTERS.has(m.name) && delta > 0) {
        findings.push({
          level: "critical",
          metric: m.name,
          message: `${m.name} +${delta} since last sample`,
        })
      } else if (WARNING_DELTA_COUNTERS.has(m.name)) {
        if (delta > 0) {
          findings.push({
            level: "warning",
            metric: m.name,
            message: `${m.name} +${delta} since last sample`,
          })
        } else if (m.value > 0) {
          findings.push({
            level: "warning",
            metric: m.name,
            message: `${m.name} cumulative ${m.value} (no recent change)`,
          })
        }
      }
    }

    if (m.kind === "gauge" && m.capacity && m.capacity > 0) {
      const ratio = m.value / m.capacity
      if (ratio >= CRITICAL_THRESHOLD) {
        findings.push({
          level: "critical",
          metric: m.name,
          message: `${m.name} ${m.value}/${m.capacity} (${Math.round(ratio * 100)}%)`,
        })
      } else if (ratio >= WARNING_THRESHOLD) {
        findings.push({
          level: "warning",
          metric: m.name,
          message: `${m.name} ${m.value}/${m.capacity} (${Math.round(ratio * 100)}%)`,
        })
      }
    }
  }

  const level: HealthLevel = findings.some((f) => f.level === "critical")
    ? "critical"
    : findings.length > 0
      ? "warning"
      : "healthy"

  return { level, findings }
}
```

- [ ] **Step 4: Run tests to confirm they pass**

```
cd console && bunx vitest run src/lib/pipeline-health.test.ts
```

Expected: 7 tests pass.

- [ ] **Step 5: Commit**

```bash
git add console/src/lib/pipeline-health.ts console/src/lib/pipeline-health.test.ts
git commit -m "feat(console): pipeline-health classifyHealth + tests"
```

---

### Task 11: Funnel stage spec + drop-annotation logic (TDD)

**Files:**
- Modify: `console/src/lib/pipeline-health.ts`
- Modify: `console/src/lib/pipeline-health.test.ts`

- [ ] **Step 1: Write failing tests**

Append to `console/src/lib/pipeline-health.test.ts`:

```ts
import { computeFunnel, FUNNEL_STAGES } from "./pipeline-health"

describe("computeFunnel", () => {
  const fixture = (): MetricRecord[] => [
    counter("pkts_received", "capture", 12401),
    counter("pkts_parsed", "protocol", 12373),
    counter("pkts_dropped_not_ip", "protocol", 23),
    counter("pkts_dropped_not_tcp", "protocol", 5),
    counter("pkts_dropped_malformed", "protocol", 0),
    counter("http_exchanges_joined", "protocol", 6400),
    counter("http_exchanges_unpaired", "protocol", 2),
    counter("http_exchanges_expired", "protocol", 0),
    counter("wires_detected", "llm", 88),
    counter("wires_ignored", "llm", 6312),
    counter("calls_with_agent", "llm", 87),
    counter("calls_without_agent", "llm", 1),
    counter("calls_ingested", "turn", 87),
    counter("calls_dropped_late", "turn", 0),
    counter("calls_auxiliary", "turn", 5),
    counter("turns_completed", "turn", 22),
    counter("turns_discarded_no_user_start", "turn", 1),
    counter("flushed_calls", "storage", 87),
    counter("buf_calls", "storage", 87),
  ]

  it("emits one row per FUNNEL_STAGES entry, in order", () => {
    const rows = computeFunnel(fixture())
    expect(rows.map((r) => r.label)).toEqual(FUNNEL_STAGES.map((s) => s.label))
  })

  it("widthRatio of pkts_received is 1.0", () => {
    const rows = computeFunnel(fixture())
    const root = rows[0]
    expect(root.label).toBe("pkts_received")
    expect(root.widthRatio).toBeCloseTo(1.0)
  })

  it("widthRatio of wires_detected is 88/12401", () => {
    const rows = computeFunnel(fixture())
    const wires = rows.find((r) => r.label === "wires_detected")!
    expect(wires.widthRatio).toBeCloseTo(88 / 12401, 5)
  })

  it("annotates pkts_parsed with not_ip / not_tcp / malformed counts", () => {
    const rows = computeFunnel(fixture())
    const parsed = rows.find((r) => r.label === "pkts_parsed")!
    expect(parsed.dropAnnotation).toContain("not_ip 23")
    expect(parsed.dropAnnotation).toContain("not_tcp 5")
    expect(parsed.dropAnnotation).toContain("malformed 0")
    expect(parsed.dropAnnotation).toMatch(/-28/)
  })

  it("annotates wires_detected with wires_ignored count", () => {
    const rows = computeFunnel(fixture())
    const wires = rows.find((r) => r.label === "wires_detected")!
    expect(wires.dropAnnotation).toContain("wires_ignored 6312")
  })

  it("flags turns_completed as fan-in (not a drop)", () => {
    const rows = computeFunnel(fixture())
    const turns = rows.find((r) => r.label === "turns_completed")!
    expect(turns.dropAnnotation?.toLowerCase()).toContain("fan-in")
  })

  it("widthRatio is 0 when pkts_received is missing or zero", () => {
    const rows = computeFunnel([])
    expect(rows.every((r) => r.widthRatio === 0)).toBe(true)
  })
})
```

- [ ] **Step 2: Run tests to confirm they fail**

```
cd console && bunx vitest run src/lib/pipeline-health.test.ts
```

Expected: failures — `computeFunnel` and `FUNNEL_STAGES` are not exported.

- [ ] **Step 3: Implement**

Append to `console/src/lib/pipeline-health.ts`:

```ts
export type FunnelStageLabel =
  | "pkts_received"
  | "pkts_parsed"
  | "http_exchanges_joined"
  | "wires_detected"
  | "calls_with_agent"
  | "calls_ingested"
  | "turns_completed"
  | "flushed_calls"

export type FunnelStageSpec = {
  label: FunnelStageLabel
  /** The metric name that supplies `value` for this row. */
  source: string
  /** Annotation generator — given the snapshot map, produce a drop note. */
  annotate: (snap: Record<string, number>) => string | null
}

export const FUNNEL_STAGES: FunnelStageSpec[] = [
  {
    label: "pkts_received",
    source: "pkts_received",
    annotate: () => null,
  },
  {
    label: "pkts_parsed",
    source: "pkts_parsed",
    annotate: (snap) => {
      const not_ip = snap.pkts_dropped_not_ip ?? 0
      const not_tcp = snap.pkts_dropped_not_tcp ?? 0
      const malformed = snap.pkts_dropped_malformed ?? 0
      const total = not_ip + not_tcp + malformed
      return `-${total} (not_ip ${not_ip}, not_tcp ${not_tcp}, malformed ${malformed})`
    },
  },
  {
    label: "http_exchanges_joined",
    source: "http_exchanges_joined",
    annotate: (snap) => {
      const unpaired = snap.http_exchanges_unpaired ?? 0
      const expired = snap.http_exchanges_expired ?? 0
      return `subset that is HTTP; -${
        unpaired + expired
      } (unpaired ${unpaired}, expired ${expired})`
    },
  },
  {
    label: "wires_detected",
    source: "wires_detected",
    annotate: (snap) => {
      const ignored = snap.wires_ignored ?? 0
      return `subset matching an LLM wire-API; rest are wires_ignored ${ignored}`
    },
  },
  {
    label: "calls_with_agent",
    source: "calls_with_agent",
    annotate: (snap) => `-${snap.calls_without_agent ?? 0} calls_without_agent`,
  },
  {
    label: "calls_ingested",
    source: "calls_ingested",
    annotate: (snap) => {
      const dropped_late = snap.calls_dropped_late ?? 0
      const aux = snap.calls_auxiliary ?? 0
      return `-${dropped_late} dropped_late, +${aux} auxiliary (not part of any turn)`
    },
  },
  {
    label: "turns_completed",
    source: "turns_completed",
    annotate: () => "fan-in: multiple calls per turn — this is not a drop",
  },
  {
    label: "flushed_calls",
    source: "flushed_calls",
    annotate: (snap) => {
      const buf = snap.buf_calls ?? 0
      const flushed = snap.flushed_calls ?? 0
      const not_flushed = Math.max(0, buf - flushed)
      return `${not_flushed} not yet flushed`
    },
  },
]

export type FunnelRow = {
  label: FunnelStageLabel
  value: number
  widthRatio: number
  dropAnnotation: string | null
}

export function computeFunnel(metrics: MetricRecord[]): FunnelRow[] {
  const snap: Record<string, number> = {}
  for (const m of metrics) snap[m.name] = m.value

  const root = snap.pkts_received ?? 0

  return FUNNEL_STAGES.map((stage) => {
    const value = snap[stage.source] ?? 0
    const widthRatio = root > 0 ? value / root : 0
    return {
      label: stage.label,
      value,
      widthRatio,
      dropAnnotation: stage.annotate(snap),
    }
  })
}
```

- [ ] **Step 4: Run tests to confirm they pass**

```
cd console && bunx vitest run src/lib/pipeline-health.test.ts
```

Expected: all funnel + classify tests pass (14 total).

- [ ] **Step 5: Commit**

```bash
git add console/src/lib/pipeline-health.ts console/src/lib/pipeline-health.test.ts
git commit -m "feat(console): computeFunnel + FUNNEL_STAGES with drop annotations"
```

---

## Phase 4 — Page shell, route, sidebar gating

### Task 12: Page skeleton + route + sidebar nav (gated)

**Files:**
- Create: `console/src/pages/pipeline-health.tsx`
- Create: `console/src/components/pipeline-health/health-pill.tsx`
- Modify: `console/src/app.tsx`
- Modify: `console/src/components/layout/sidebar.tsx`

- [ ] **Step 1: Add the health-pill helper**

`console/src/components/pipeline-health/health-pill.tsx`:

```tsx
import { cn } from "@/lib/utils"
import type { HealthLevel } from "@/lib/pipeline-health"

type Props = { level: HealthLevel; count?: number }

const LABELS: Record<HealthLevel, string> = {
  healthy: "Healthy",
  warning: "Warning",
  critical: "Critical",
}

const STYLES: Record<HealthLevel, string> = {
  healthy:
    "bg-emerald-100 text-emerald-700 dark:bg-emerald-950 dark:text-emerald-300",
  warning:
    "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300",
  critical: "bg-red-100 text-red-700 dark:bg-red-950 dark:text-red-300",
}

export function HealthPill({ level, count }: Props) {
  const label =
    level === "healthy" || count === undefined || count === 0
      ? LABELS[level]
      : `${count} ${level === "critical" ? "critical" : "warnings"}`
  return (
    <span
      className={cn(
        "inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium",
        STYLES[level],
      )}
    >
      {level === "critical" && "✗ "}
      {level === "warning" && "⚠ "}
      {label}
    </span>
  )
}
```

- [ ] **Step 2: Add the page skeleton**

`console/src/pages/pipeline-health.tsx`:

```tsx
import { Loader2 } from "lucide-react"
import { Navigate } from "react-router"
import { useServerInfo } from "@/hooks/use-server-info"
import { useInternalMetrics } from "@/hooks/use-internal-metrics"
import { usePipelineHealthStore } from "@/stores/pipeline-health"
import { classifyHealth } from "@/lib/pipeline-health"
import { HealthPill } from "@/components/pipeline-health/health-pill"

export function PipelineHealthPage() {
  const { data: info, isLoading: infoLoading } = useServerInfo()
  const enabled = info?.console.features.pipeline_health === true

  if (infoLoading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  if (!enabled) return <Navigate to="/" replace />

  return <PipelineHealthBody />
}

function PipelineHealthBody() {
  const { data, isLoading } = useInternalMetrics()
  const intervalMs = usePipelineHealthStore((s) => s.intervalMs)
  const setIntervalMs = usePipelineHealthStore((s) => s.setIntervalMs)
  const selectedPipeline = usePipelineHealthStore((s) => s.selectedPipeline)
  const setSelectedPipeline = usePipelineHealthStore(
    (s) => s.setSelectedPipeline,
  )

  if (isLoading || !data) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  const pipelineNames = data.pipelines.map((p) => p.name)
  const activeName =
    selectedPipeline && pipelineNames.includes(selectedPipeline)
      ? selectedPipeline
      : (pipelineNames[0] ?? null)
  const active = data.pipelines.find((p) => p.name === activeName)

  const allMetrics = [
    ...(active?.metrics ?? []),
    ...data.global.metrics,
  ]
  // First frame has no prev — pass empty map so delta-based rules sit quiet
  // until the second frame.
  const health = classifyHealth(allMetrics, {})

  return (
    <div className="flex flex-col gap-4 p-4">
      {/* ===== Header ===== */}
      <div className="flex items-center gap-3 rounded-lg border border-border bg-card p-3">
        <span className="text-sm font-semibold">Pipeline Health</span>

        {pipelineNames.length > 1 ? (
          <select
            className="h-7 rounded-md border border-input bg-background px-2 text-xs"
            value={activeName ?? ""}
            onChange={(e) => setSelectedPipeline(e.target.value || null)}
          >
            {pipelineNames.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        ) : (
          <span className="rounded-md bg-muted px-2 py-0.5 text-xs text-muted-foreground">
            {activeName ?? "—"}
          </span>
        )}

        <HealthPill level={health.level} count={health.findings.length} />

        <div className="ml-auto flex items-center gap-1">
          {[1000, 2000, 5000, null].map((ms) => (
            <button
              key={String(ms)}
              onClick={() => setIntervalMs(ms)}
              className={`h-7 rounded-md px-2 text-xs ${
                intervalMs === ms
                  ? "bg-foreground text-background"
                  : "bg-muted text-muted-foreground hover:bg-muted/70"
              }`}
            >
              {ms === null ? "Pause" : `${ms / 1000}s`}
            </button>
          ))}
        </div>
      </div>

      {/* ===== Sections ===== */}
      <div className="text-sm text-muted-foreground">
        Sections render here — see Tasks 14–18.
      </div>
    </div>
  )
}
```

- [ ] **Step 3: Mount the route**

In `console/src/app.tsx`, add the import and `<Route>`:

```tsx
import { PipelineHealthPage } from "@/pages/pipeline-health"
```

and inside the existing `<Route element={<AppLayout />}>` block add:

```tsx
            <Route path="/pipeline-health" element={<PipelineHealthPage />} />
```

- [ ] **Step 4: Conditionally render the sidebar nav**

In `console/src/components/layout/sidebar.tsx`:

(a) Add the import at the top of the imports block:

```ts
import { Activity } from "lucide-react"
import { useServerInfo } from "@/hooks/use-server-info"
```

(b) Convert `navItems` to be derived inside the component so it can read the feature flag. Replace the `const navItems = [...]` array (above the `Sidebar` function) with a local builder inside `Sidebar()`. Specifically, **delete** the `const navItems = [ ... ]` block, and at the start of `Sidebar()` (right after `const [searchParams] = useSearchParams()`), add:

```ts
  const { data: info } = useServerInfo()
  const showPipelineHealth = info?.console.features.pipeline_health === true

  const navItems = [
    { to: "/", icon: LayoutDashboard, label: "Overview" },
    { to: "/performance", icon: Gauge, label: "Performance" },
    { to: "/traffic", icon: BarChart3, label: "Traffic" },
    { to: "/errors", icon: AlertTriangle, label: "Errors" },
    { to: "/models", icon: Cpu, label: "Models" },
    { to: "/agent-sessions", icon: MessageSquare, label: "Agent Sessions" },
    { to: "/agent-turns", icon: MessagesSquare, label: "Agent Turns" },
    { to: "/llm-calls", icon: Sparkles, label: "LLM Calls" },
    { to: "/http-exchanges", icon: Network, label: "HTTP Exchanges" },
    ...(showPipelineHealth
      ? [{ to: "/pipeline-health", icon: Activity, label: "Pipeline Health" }]
      : []),
  ]
```

- [ ] **Step 5: Build + smoke test**

Build the console:

```
cd console && bun run tsc -b --noEmit
```

Expected: clean. Then run the dev server (in another terminal you already have a backend running with `pipeline_health = true`):

```
just dev console
```

Open `http://localhost:5173/pipeline-health` in a browser.

Expected: page loads, header shows pipeline picker + health pill + refresh control. Sections area shows the placeholder text. With backend `pipeline_health = false`, the page redirects to `/`.

- [ ] **Step 6: Commit**

```bash
git add \
  console/src/pages/pipeline-health.tsx \
  console/src/components/pipeline-health/health-pill.tsx \
  console/src/app.tsx \
  console/src/components/layout/sidebar.tsx
git commit -m "feat(console): /pipeline-health page shell + gated nav"
```

---

### Task 13: Backpressure section

**Files:**
- Create: `console/src/components/pipeline-health/backpressure-section.tsx`
- Modify: `console/src/pages/pipeline-health.tsx`

- [ ] **Step 1: Implement the section**

`console/src/components/pipeline-health/backpressure-section.tsx`:

```tsx
import { cn } from "@/lib/utils"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

// Display order — left to right along the data flow.
const QUEUE_ORDER = [
  "q_raw_pkts",
  "q_parsed_pkts",
  "q_http_parse_events",
  "q_http_joiner_events",
  "q_agent_calls",
  "q_llm_events",
] as const

const STORAGE_QUEUES = ["q_calls", "q_turns", "q_metrics", "q_exchanges"] as const

function classifyQueue(value: number, capacity: number) {
  if (capacity <= 0) return "ok" as const
  const r = value / capacity
  if (r >= 0.95) return "bad" as const
  if (r >= 0.9) return "warn" as const
  return "ok" as const
}

const STAGE_STYLES = {
  ok: "bg-card border-border",
  warn: "bg-amber-50 border-amber-300 dark:bg-amber-950/40 dark:border-amber-600",
  bad: "bg-red-50 border-red-300 dark:bg-red-950/40 dark:border-red-600",
} as const

const BAR_STYLES = {
  ok: "bg-emerald-500",
  warn: "bg-amber-500",
  bad: "bg-red-500",
} as const

function QueueCell({
  name,
  value,
  capacity,
}: {
  name: string
  value: number
  capacity: number
}) {
  const cls = classifyQueue(value, capacity)
  const pct = capacity > 0 ? Math.round((value / capacity) * 100) : 0
  return (
    <div
      className={cn(
        "min-w-[140px] rounded-md border p-2",
        STAGE_STYLES[cls],
      )}
    >
      <div className="text-xs font-semibold text-foreground">{name}</div>
      <div className="text-xs tabular-nums text-muted-foreground">
        {value.toLocaleString()} / {capacity.toLocaleString()} ({pct}%)
      </div>
      <div className="mt-1 h-1 overflow-hidden rounded-full bg-muted">
        <div
          className={cn("h-full", BAR_STYLES[cls])}
          style={{ width: `${Math.min(100, pct)}%` }}
        />
      </div>
    </div>
  )
}

export function BackpressureSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]
  const byName = new Map(all.map((m) => [m.name, m]))

  const cells: Array<{ name: string; value: number; capacity: number }> = []
  for (const name of QUEUE_ORDER) {
    const m = byName.get(name)
    if (m && m.kind === "gauge" && m.capacity) {
      cells.push({ name: m.name, value: m.value, capacity: m.capacity })
    }
  }

  // Storage queues: aggregate to one summary cell ("worst-of"), keep individual
  // queues visible in the all-metrics table (Section ⑤).
  const storageCells = STORAGE_QUEUES.map((n) => byName.get(n)).filter(
    (m): m is MetricRecord =>
      !!m && m.kind === "gauge" && typeof m.capacity === "number",
  )
  let storageSummary: { value: number; capacity: number } | null = null
  if (storageCells.length > 0) {
    let worstRatio = -1
    for (const m of storageCells) {
      const r = m.value / (m.capacity ?? 1)
      if (r > worstRatio) {
        worstRatio = r
        storageSummary = { value: m.value, capacity: m.capacity ?? 0 }
      }
    }
  }

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ① Backpressure
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Queue depth at every stage along the pipeline. The first cell to redden
        is the bottleneck.
      </p>
      <div className="flex items-stretch gap-1 overflow-x-auto">
        {cells.map((c, i) => (
          <div key={c.name} className="flex items-stretch gap-1">
            <QueueCell {...c} />
            {(i < cells.length - 1 || storageSummary) && (
              <div className="self-center px-1 text-muted-foreground">→</div>
            )}
          </div>
        ))}
        {storageSummary && (
          <QueueCell
            name="storage queues (worst)"
            value={storageSummary.value}
            capacity={storageSummary.capacity}
          />
        )}
      </div>
    </section>
  )
}
```

- [ ] **Step 2: Wire it into the page**

In `console/src/pages/pipeline-health.tsx`, add the import:

```tsx
import { BackpressureSection } from "@/components/pipeline-health/backpressure-section"
```

Replace the placeholder `<div className="text-sm text-muted-foreground">Sections render here — see Tasks 14–18.</div>` with:

```tsx
      <BackpressureSection
        pipelineMetrics={active?.metrics ?? []}
        globalMetrics={data.global.metrics}
      />
```

- [ ] **Step 3: Build + smoke**

```
cd console && bun run tsc -b --noEmit
```

Expected: clean. Manually verify in browser: queue cells render in order, percentage + bar render correctly, storage worst-of cell appears.

- [ ] **Step 4: Commit**

```bash
git add console/src/components/pipeline-health/backpressure-section.tsx console/src/pages/pipeline-health.tsx
git commit -m "feat(console): pipeline-health backpressure section"
```

---

### Task 14: Throughput funnel section

**Files:**
- Create: `console/src/components/pipeline-health/funnel-section.tsx`
- Modify: `console/src/pages/pipeline-health.tsx`

- [ ] **Step 1: Implement the section**

`console/src/components/pipeline-health/funnel-section.tsx`:

```tsx
import { computeFunnel } from "@/lib/pipeline-health"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

export function FunnelSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]
  const rows = computeFunnel(all)

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ② Throughput Funnel
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Records flowing from packet capture to storage flush. Each bar's length
        is proportional to the count; drops/fan-ins are noted directly under
        each stage.
      </p>
      <div className="flex flex-col gap-1.5">
        {rows.map((row) => (
          <div key={row.label} className="grid grid-cols-[180px_1fr_120px] items-center gap-2">
            <div className="text-xs font-semibold text-foreground">
              {row.label}
            </div>
            <div className="h-3.5 rounded bg-muted">
              <div
                className="h-full rounded bg-blue-500/80"
                style={{ width: `${Math.max(2, row.widthRatio * 100)}%` }}
              />
            </div>
            <div className="text-right text-xs font-semibold tabular-nums text-foreground">
              {row.value.toLocaleString()}
            </div>
            {row.dropAnnotation && (
              <div className="col-start-2 col-end-3 text-[10px] text-amber-700 dark:text-amber-400">
                ↳ {row.dropAnnotation}
              </div>
            )}
          </div>
        ))}
      </div>
    </section>
  )
}
```

- [ ] **Step 2: Mount in the page**

In `console/src/pages/pipeline-health.tsx`, add the import:

```tsx
import { FunnelSection } from "@/components/pipeline-health/funnel-section"
```

and place it directly below the `<BackpressureSection ... />`:

```tsx
      <FunnelSection
        pipelineMetrics={active?.metrics ?? []}
        globalMetrics={data.global.metrics}
      />
```

- [ ] **Step 3: Build + smoke**

```
cd console && bun run tsc -b --noEmit
```

Expected: clean. In browser, verify each funnel row renders, drops show under stages with non-zero drop counts, and bars shrink proportionally.

- [ ] **Step 4: Commit**

```bash
git add console/src/components/pipeline-health/funnel-section.tsx console/src/pages/pipeline-health.tsx
git commit -m "feat(console): pipeline-health throughput funnel section"
```

---

### Task 15: State gauges section

**Files:**
- Create: `console/src/components/pipeline-health/state-gauges-section.tsx`
- Modify: `console/src/pages/pipeline-health.tsx`

- [ ] **Step 1: Implement the section**

`console/src/components/pipeline-health/state-gauges-section.tsx`:

```tsx
import { cn } from "@/lib/utils"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
}

const STATE_METRICS = [
  "flows_active",
  "turns_active",
  "tcp_ooo_buffered",
  "flows_expired",
  "heartbeats_emitted",
  "batches_received",
  "http_resyncs",
] as const

function StateCard({ label, value }: { label: string; value: number | undefined }) {
  return (
    <div
      className={cn(
        "flex flex-col gap-0.5 rounded-md border border-border bg-muted/30 p-2.5",
      )}
    >
      <div className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="text-base font-bold tabular-nums text-foreground">
        {value === undefined ? "—" : value.toLocaleString()}
      </div>
    </div>
  )
}

export function StateGaugesSection({ pipelineMetrics, globalMetrics }: Props) {
  const all = [...pipelineMetrics, ...globalMetrics]
  const byName = new Map(all.map((m) => [m.name, m]))
  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ③ State Gauges
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Instance-level state — counts not arranged along the pipeline.
      </p>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 md:grid-cols-5 lg:grid-cols-7">
        {STATE_METRICS.map((name) => (
          <StateCard key={name} label={name} value={byName.get(name)?.value} />
        ))}
      </div>
    </section>
  )
}
```

- [ ] **Step 2: Mount and smoke**

In `console/src/pages/pipeline-health.tsx`:

```tsx
import { StateGaugesSection } from "@/components/pipeline-health/state-gauges-section"
```

Place below `<FunnelSection .../>`:

```tsx
      <StateGaugesSection
        pipelineMetrics={active?.metrics ?? []}
        globalMetrics={data.global.metrics}
      />
```

```
cd console && bun run tsc -b --noEmit
```

- [ ] **Step 3: Commit**

```bash
git add console/src/components/pipeline-health/state-gauges-section.tsx console/src/pages/pipeline-health.tsx
git commit -m "feat(console): pipeline-health state gauges section"
```

---

### Task 16: Error red-list section

**Files:**
- Create: `console/src/components/pipeline-health/error-list-section.tsx`
- Modify: `console/src/pages/pipeline-health.tsx`

- [ ] **Step 1: Implement**

`console/src/components/pipeline-health/error-list-section.tsx`:

```tsx
import { cn } from "@/lib/utils"
import type { MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
  prevByName: Record<string, number>
}

const CRITICAL = new Set([
  "pkts_dropped_kernel",
  "flush_errors",
  "read_errors",
  "dump_errors",
  "batches_dropped_zmq",
])

const WARNING = new Set([
  "tcp_ooo_dropped",
  "http_resyncs",
  "turns_discarded_no_user_start",
  "calls_dropped_late",
  "heartbeats_dropped",
])

const EXPLANATIONS: Record<string, string> = {
  pkts_dropped_kernel:
    "Kernel ring buffer overflowed — capture is falling behind. Increase buffer size or reduce filter scope.",
  flush_errors: "Storage backend rejected a flush. Check the storage logs.",
  read_errors: "Pcap source returned an error during read.",
  dump_errors: "Packet dumper failed to write a frame to disk.",
  batches_dropped_zmq: "ZMQ batches dropped due to HWM. Receiver is slower than the probe.",
  tcp_ooo_dropped: "TCP segment received out of order and exceeded the buffer.",
  http_resyncs: "HTTP parser had to resync — typically due to a snaplen-truncated frame.",
  turns_discarded_no_user_start:
    "A call was assigned an agent but no user-message start was ever seen — typically mid-stream capture.",
  calls_dropped_late: "Call arrived after its turn was finalized — partition timing issue.",
  heartbeats_dropped:
    "Capture-source heartbeat could not be enqueued (channel full).",
}

type Finding = {
  level: "critical" | "warning"
  metric: string
  value: number
  delta: number
}

function buildFindings(
  metrics: MetricRecord[],
  prev: Record<string, number>,
): Finding[] {
  const out: Finding[] = []
  for (const m of metrics) {
    if (m.kind !== "counter") continue
    const delta = m.value - (prev[m.name] ?? m.value)
    if (CRITICAL.has(m.name)) {
      if (m.value > 0 || delta > 0) {
        out.push({ level: "critical", metric: m.name, value: m.value, delta })
      }
    } else if (WARNING.has(m.name)) {
      if (m.value > 0 || delta > 0) {
        out.push({ level: "warning", metric: m.name, value: m.value, delta })
      }
    }
  }
  // critical first, then by largest delta
  return out.sort(
    (a, b) =>
      Number(b.level === "critical") - Number(a.level === "critical") ||
      b.delta - a.delta,
  )
}

export function ErrorListSection({
  pipelineMetrics,
  globalMetrics,
  prevByName,
}: Props) {
  const findings = buildFindings(
    [...pipelineMetrics, ...globalMetrics],
    prevByName,
  )

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ④ Errors
      </h3>
      <p className="mb-3 text-xs text-muted-foreground">
        Counters that should normally be zero. Anything here is worth a look.
      </p>
      {findings.length === 0 ? (
        <div className="rounded-md border border-emerald-300 bg-emerald-50 p-2 text-sm text-emerald-700 dark:border-emerald-600 dark:bg-emerald-950/40 dark:text-emerald-300">
          ✓ No errors recorded.
        </div>
      ) : (
        <div className="flex flex-col gap-1.5">
          {findings.map((f) => (
            <div
              key={f.metric}
              className={cn(
                "flex items-start gap-3 rounded-md border p-2",
                f.level === "critical"
                  ? "border-red-300 bg-red-50 dark:border-red-700 dark:bg-red-950/40"
                  : "border-amber-300 bg-amber-50 dark:border-amber-700 dark:bg-amber-950/40",
              )}
            >
              <span
                className={cn(
                  "mt-0.5 inline-block w-12 shrink-0 text-center text-[10px] font-bold uppercase",
                  f.level === "critical"
                    ? "text-red-700 dark:text-red-300"
                    : "text-amber-700 dark:text-amber-300",
                )}
              >
                {f.level}
              </span>
              <div className="flex-1">
                <div className="font-mono text-sm">
                  {f.metric}{" "}
                  <span className="text-muted-foreground">
                    = {f.value.toLocaleString()} (Δ {f.delta >= 0 ? "+" : ""}
                    {f.delta})
                  </span>
                </div>
                <div className="mt-0.5 text-xs text-muted-foreground">
                  {EXPLANATIONS[f.metric] ?? ""}
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
    </section>
  )
}
```

- [ ] **Step 2: Track previous frame in the page**

The error section needs the previous snapshot to compute deltas. Update `pipeline-health.tsx` to keep prev metrics in a ref. At the top of `PipelineHealthBody`, after the existing hook calls, add:

```tsx
import { useEffect, useRef } from "react"
```

Then near the top of the function:

```tsx
  const prevRef = useRef<Record<string, number>>({})
  // After each render, snapshot the current values for next-render delta.
  useEffect(() => {
    if (!data) return
    const next: Record<string, number> = {}
    for (const p of data.pipelines) for (const m of p.metrics) next[m.name] = m.value
    for (const m of data.global.metrics) next[m.name] = m.value
    prevRef.current = next
  })
  const prevByName = prevRef.current
```

(You can put `useEffect` right above the `if (isLoading || !data)` early return, since the effect runs after render and is safe to register unconditionally — but the deltas are read while rendering, so use the **current** value of `prevRef.current`, which is the snapshot from the *previous* render.)

Add the import:

```tsx
import { ErrorListSection } from "@/components/pipeline-health/error-list-section"
```

Place under `<StateGaugesSection ... />`:

```tsx
      <ErrorListSection
        pipelineMetrics={active?.metrics ?? []}
        globalMetrics={data.global.metrics}
        prevByName={prevByName}
      />
```

- [ ] **Step 3: Build + smoke**

```
cd console && bun run tsc -b --noEmit
```

Expected: clean. In browser: when a counter crosses zero, the section shows the row; static cumulative > 0 still highlights yellow.

- [ ] **Step 4: Commit**

```bash
git add console/src/components/pipeline-health/error-list-section.tsx console/src/pages/pipeline-health.tsx
git commit -m "feat(console): pipeline-health error red-list section"
```

---

### Task 17: All metrics fallback table

**Files:**
- Create: `console/src/components/pipeline-health/all-metrics-table.tsx`
- Modify: `console/src/pages/pipeline-health.tsx`

- [ ] **Step 1: Implement the table**

`console/src/components/pipeline-health/all-metrics-table.tsx`:

```tsx
import { useMemo, useState } from "react"
import { cn } from "@/lib/utils"
import { usePipelineHealthStore } from "@/stores/pipeline-health"
import type { MetricGroup, MetricRecord } from "@/types/api"

type Props = {
  pipelineMetrics: MetricRecord[]
  globalMetrics: MetricRecord[]
  prevByName: Record<string, number>
  ts: number
  prevTs: number | null
}

const GROUPS: Array<MetricGroup | "all"> = [
  "all",
  "capture",
  "protocol",
  "llm",
  "turn",
  "metrics",
  "storage",
]

const CRITICAL_DELTA = new Set([
  "pkts_dropped_kernel",
  "flush_errors",
  "read_errors",
  "dump_errors",
  "batches_dropped_zmq",
])
const WARNING_DELTA = new Set([
  "tcp_ooo_dropped",
  "http_resyncs",
  "turns_discarded_no_user_start",
  "calls_dropped_late",
  "heartbeats_dropped",
])

type SortKey = "group" | "name" | "value" | "delta" | "cap"
type SortDir = "asc" | "desc"

export function AllMetricsTable({
  pipelineMetrics,
  globalMetrics,
  prevByName,
  ts,
  prevTs,
}: Props) {
  const groupFilter = usePipelineHealthStore((s) => s.tableGroupFilter)
  const onlyWarn = usePipelineHealthStore((s) => s.tableOnlyWarn)
  const setGroupFilter = usePipelineHealthStore((s) => s.setTableGroupFilter)
  const setOnlyWarn = usePipelineHealthStore((s) => s.setTableOnlyWarn)

  const [sortKey, setSortKey] = useState<SortKey>("group")
  const [sortDir, setSortDir] = useState<SortDir>("asc")

  const dt = prevTs && ts > prevTs ? ts - prevTs : 0

  const rows = useMemo(() => {
    const all = [...pipelineMetrics, ...globalMetrics]
    return all.map((m) => {
      const prev = prevByName[m.name]
      const delta = m.kind === "counter" && typeof prev === "number" ? m.value - prev : null
      const ratio =
        m.kind === "gauge" && m.capacity && m.capacity > 0 ? m.value / m.capacity : null
      const warnLevel: "critical" | "warning" | null =
        (CRITICAL_DELTA.has(m.name) && (delta ?? 0) > 0) ||
        (ratio !== null && ratio >= 0.95)
          ? "critical"
          : (WARNING_DELTA.has(m.name) && ((delta ?? 0) > 0 || m.value > 0)) ||
              (ratio !== null && ratio >= 0.9)
            ? "warning"
            : null
      return { m, delta, ratio, warnLevel }
    })
  }, [pipelineMetrics, globalMetrics, prevByName])

  const filtered = rows.filter(
    (r) =>
      (groupFilter === "all" || r.m.group === groupFilter) &&
      (!onlyWarn || r.warnLevel !== null),
  )

  const sorted = [...filtered].sort((a, b) => {
    const dir = sortDir === "asc" ? 1 : -1
    switch (sortKey) {
      case "group":
        return dir * (a.m.group.localeCompare(b.m.group) || a.m.name.localeCompare(b.m.name))
      case "name":
        return dir * a.m.name.localeCompare(b.m.name)
      case "value":
        return dir * (a.m.value - b.m.value)
      case "delta":
        return dir * ((a.delta ?? 0) - (b.delta ?? 0))
      case "cap":
        return dir * ((a.ratio ?? -1) - (b.ratio ?? -1))
    }
  })

  const sortBtn = (key: SortKey, label: string) => (
    <button
      onClick={() => {
        if (sortKey === key) setSortDir(sortDir === "asc" ? "desc" : "asc")
        else {
          setSortKey(key)
          setSortDir("asc")
        }
      }}
      className="text-left font-semibold"
    >
      {label}
      {sortKey === key && (sortDir === "asc" ? " ↑" : " ↓")}
    </button>
  )

  return (
    <section className="rounded-lg border border-border bg-card p-4">
      <h3 className="mb-1 text-xs font-bold uppercase tracking-wider text-muted-foreground">
        ⑤ All Metrics
      </h3>
      <details>
        <summary className="cursor-pointer text-xs font-medium text-blue-600 hover:underline dark:text-blue-400">
          Show all {pipelineMetrics.length + globalMetrics.length} metrics
        </summary>

        <div className="mt-3 flex flex-wrap items-center gap-1.5">
          {GROUPS.map((g) => (
            <button
              key={g}
              onClick={() => setGroupFilter(g)}
              className={cn(
                "rounded-full border px-2.5 py-0.5 text-xs",
                groupFilter === g
                  ? "border-foreground bg-foreground text-background"
                  : "border-border bg-card text-muted-foreground hover:bg-muted",
              )}
            >
              {g}
            </button>
          ))}
          <button
            onClick={() => setOnlyWarn(!onlyWarn)}
            className={cn(
              "ml-auto rounded-full border px-2.5 py-0.5 text-xs",
              onlyWarn
                ? "border-amber-400 bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300"
                : "border-border bg-card text-muted-foreground hover:bg-muted",
            )}
          >
            ⚠ only
          </button>
        </div>

        <table className="mt-2 w-full text-xs">
          <thead>
            <tr className="border-b border-border">
              <th className="px-2 py-1 text-left">{sortBtn("group", "group")}</th>
              <th className="px-2 py-1 text-left">{sortBtn("name", "metric")}</th>
              <th className="px-2 py-1 text-left">kind</th>
              <th className="px-2 py-1 text-right">{sortBtn("value", "value")}</th>
              <th className="px-2 py-1 text-right">{sortBtn("delta", "Δ/s")}</th>
              <th className="px-2 py-1 text-right">{sortBtn("cap", "cap%")}</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map(({ m, delta, ratio, warnLevel }) => (
              <tr
                key={m.name}
                className={cn(
                  "border-b border-border/60",
                  warnLevel === "critical" &&
                    "bg-red-50 dark:bg-red-950/40",
                  warnLevel === "warning" &&
                    "bg-amber-50 dark:bg-amber-950/40",
                )}
              >
                <td className="px-2 py-0.5">{m.group}</td>
                <td className="px-2 py-0.5 font-mono">{m.name}</td>
                <td className="px-2 py-0.5">{m.kind}</td>
                <td className="px-2 py-0.5 text-right tabular-nums">
                  {m.value.toLocaleString()}
                </td>
                <td className="px-2 py-0.5 text-right tabular-nums">
                  {m.kind === "counter" && delta !== null && dt > 0
                    ? `${delta >= 0 ? "+" : ""}${(delta / dt).toFixed(1)}`
                    : "—"}
                </td>
                <td className="px-2 py-0.5 text-right tabular-nums">
                  {ratio !== null ? `${Math.round(ratio * 100)}%` : "—"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </details>
    </section>
  )
}
```

- [ ] **Step 2: Track previous timestamp in the page**

In `console/src/pages/pipeline-health.tsx`, alongside `prevRef`, add a `prevTsRef`:

```tsx
  const prevTsRef = useRef<number | null>(null)
  useEffect(() => {
    if (!data) return
    prevTsRef.current = data.ts
    // (the prevRef effect from Task 16 is unchanged)
  })
```

(Combine both effects into one if you prefer; what matters is that *after* this render, `prevTsRef.current` becomes the current `data.ts`, so on the next render it's the previous frame's `ts`.)

Then read its current value (= previous frame's ts) before the first render-time `effect` write:

```tsx
  const prevTs = prevTsRef.current
```

Mount the table:

```tsx
import { AllMetricsTable } from "@/components/pipeline-health/all-metrics-table"
```

```tsx
      <AllMetricsTable
        pipelineMetrics={active?.metrics ?? []}
        globalMetrics={data.global.metrics}
        prevByName={prevByName}
        ts={data.ts}
        prevTs={prevTs}
      />
```

- [ ] **Step 3: Build + smoke**

```
cd console && bun run tsc -b --noEmit
```

Expected: clean. In browser, expand the `<details>` panel, exercise filter chips, sorting. Confirm warning rows highlight.

- [ ] **Step 4: Commit**

```bash
git add console/src/components/pipeline-health/all-metrics-table.tsx console/src/pages/pipeline-health.tsx
git commit -m "feat(console): pipeline-health all-metrics fallback table"
```

---

## Phase 5 — Final pass

### Task 18: Top-to-bottom verification

**Files:** none (verification only)

- [ ] **Step 1: Quality gates**

```
just quality all
```

Expected: green. Fix any clippy/eslint complaints before continuing.

- [ ] **Step 2: Full backend test suite**

```
just test all
```

Expected: green.

- [ ] **Step 3: Frontend test suite**

```
cd console && bunx vitest run
```

Expected: green. Existing `turn-index.test.ts` plus the new `pipeline-health.test.ts` (14+ assertions) all pass.

- [ ] **Step 4: Manual end-to-end smoke**

In one terminal, run with the flag flipped on:

```
TS_CONSOLE__FEATURES__PIPELINE_HEALTH=true cargo run -p tokenscope -- --config server/config/default.toml -i lo0
```

(Substitute a valid loopback interface for your platform; on macOS `lo0`.)

In another terminal:

```
cd console && bun run dev
```

Open `http://localhost:5173/pipeline-health`. Verify:
1. Header shows pipeline picker, healthy pill, refresh control.
2. Backpressure cells render in order with green bars.
3. Funnel rows render; values increase as you drive traffic; drop annotations show 0s.
4. State gauges populate.
5. Errors section says "✓ No errors recorded."
6. Generate some traffic (e.g. `curl https://api.openai.com/...` if traffic is observable) — counters tick.
7. Click Pause — counters stop refreshing. Click 1s — refresh accelerates.
8. Toggle the flag off (restart with `TS_CONSOLE__FEATURES__PIPELINE_HEALTH=false`); the sidebar entry disappears, direct URL `/pipeline-health` redirects to `/`.

- [ ] **Step 5: Commit any quality fixups + tag the work**

```bash
git status
# If there are unstaged fixups:
git add -A && git commit -m "chore: lint/format fixups for pipeline-health"
```

---

## Out-of-scope reminder

Anything in §2 of the spec — storage of internal metrics, historical sparklines, Prometheus `/metrics`, alerting, multi-pipeline aggregate health header — is explicitly **not** part of this plan. Do not pull those in opportunistically. If the page exposes a real need for one of them, raise a follow-up spec.
