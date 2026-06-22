//! Integration tests for `/api/metrics/*` and `/api/llm-calls*` endpoints
//! that need a real `DuckDbBackend`.
//!
//! Consolidated into one integration test file (one test binary) to keep
//! the `h-api` lib unit-test binary off `libduckdb-sys` without
//! multiplying the number of binaries that pay the ~50 MB DuckDB link cost
//! — each `tests/*.rs` file becomes its own binary.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use h_api::{router, ApiHealthContext, ApiMetricsContext, ApiRuntimeConfigContext};
use h_metrics::model::LlmFinishMetric;
use h_storage_duckdb::DuckDbBackend;

fn test_metrics_context() -> ApiMetricsContext {
    let sys = h_common::internal_metrics::MetricsSystem::new();
    ApiMetricsContext {
        pipelines: vec![],
        global: sys.start(),
        history: None,
    }
}

fn test_runtime_config_context() -> ApiRuntimeConfigContext {
    ApiRuntimeConfigContext {
        config: std::sync::Arc::new(h_common::config::AppConfig {
            pipelines: vec![],
            storage: h_common::config::StorageConfig::default(),
            internal_metrics: h_common::config::InternalMetricsConfig::default(),
            api: h_common::config::ApiConfig::default(),
            agent_classifier: h_common::config::ClassifierConfigToml::default(),
            body_cap: h_common::config::BodyCapConfig::default(),
        }),
        config_path: "test".to_string(),
        loaded_at_ms: 0,
        version: "test",
    }
}

fn test_health_context() -> ApiHealthContext {
    ApiHealthContext {
        started_at_ms: 0,
        version: "test",
        pipelines: vec![],
        drained: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

#[tokio::test]
async fn finish_reasons_endpoint_returns_one_series_per_raw_value() {
    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();

    // Seed a few raw provider values at the rolled-up (*, *, *) tier so
    // a default no-filter read picks them up, in a 1m bucket. Two rows
    // per reason → asserts grouping is by finish_reason, not just timestamp.
    let ts_a: i64 = 1_700_000_000_000_000;
    let ts_b: i64 = 1_700_000_060_000_000;
    let mk = |ts: i64, reason: &str, count: u64| LlmFinishMetric {
        timestamp_us: ts,
        source_id: String::new(),
        granularity: "1m".to_string(),
        wire_api: "*".to_string(),
        model: "*".to_string(),
        server_ip: "*".to_string(),
        finish_reason: reason.to_string(),
        count,
    };
    <DuckDbBackend as h_storage::StorageBackend>::write_finish_metrics(
        &backend,
        vec![
            mk(ts_a, "end_turn", 12),
            mk(ts_a, "tool_use", 4),
            mk(ts_a, "max_tokens", 1),
            mk(ts_b, "end_turn", 7),
            mk(ts_b, "pause_turn", 2),
        ],
    )
    .await
    .unwrap();

    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    // start/end are seconds (matches existing /api/metrics/* convention).
    let start_s = (ts_a / 1_000_000) - 1;
    let end_s = (ts_b / 1_000_000) + 60;
    let uri = format!("/api/metrics/finish-reasons?start={start_s}&end={end_s}&granularity=1m");
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let series = v["data"]["series"].as_array().expect("series array");
    let names: Vec<&str> = series
        .iter()
        .map(|s| s["finish_reason"].as_str().unwrap())
        .collect();
    // ORDER BY finish_reason ASC.
    assert_eq!(
        names,
        vec!["end_turn", "max_tokens", "pause_turn", "tool_use"]
    );

    let end_turn = &series[0];
    let points = end_turn["points"].as_array().unwrap();
    assert_eq!(points.len(), 2, "end_turn should have two buckets");
    // points are [[ts_us, count], ...] ordered by ts ascending.
    assert_eq!(points[0][0].as_i64().unwrap(), ts_a);
    assert_eq!(points[0][1].as_u64().unwrap(), 12);
    assert_eq!(points[1][0].as_i64().unwrap(), ts_b);
    assert_eq!(points[1][1].as_u64().unwrap(), 7);
}

#[tokio::test]
async fn finish_reasons_endpoint_accepts_csv_wire_api_filter() {
    // Two wire_apis in the same window; CSV `?wire_api=anthropic,openai-chat`
    // must include rows from both. Series with the same finish_reason
    // across wire_apis collapse into one (SUM at the (W, M, *) tier).
    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();

    let ts: i64 = 1_700_000_000_000_000;
    let mk = |wire: &str, model: &str, reason: &str, count: u64| LlmFinishMetric {
        timestamp_us: ts,
        source_id: String::new(),
        granularity: "1m".to_string(),
        wire_api: wire.to_string(),
        model: model.to_string(),
        server_ip: "*".to_string(),
        finish_reason: reason.to_string(),
        count,
    };
    <DuckDbBackend as h_storage::StorageBackend>::write_finish_metrics(
        &backend,
        vec![
            mk("anthropic", "claude-3", "end_turn", 9),
            mk("openai-chat", "gpt-4", "stop", 5),
            mk("openai-chat", "gpt-4o", "stop", 2),
            // Outside the CSV filter — must not contribute.
            mk("gemini", "gemini-pro", "stop", 100),
        ],
    )
    .await
    .unwrap();

    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    let start_s = (ts / 1_000_000) - 1;
    let end_s = (ts / 1_000_000) + 60;
    let uri = format!(
        "/api/metrics/finish-reasons?start={start_s}&end={end_s}&granularity=1m\
         &wire_api=anthropic,openai-chat"
    );
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let series = v["data"]["series"].as_array().expect("series array");

    let names: Vec<&str> = series
        .iter()
        .map(|s| s["finish_reason"].as_str().unwrap())
        .collect();
    // Both wire_apis contributed; gemini excluded.
    assert_eq!(names, vec!["end_turn", "stop"]);

    let stop = series
        .iter()
        .find(|s| s["finish_reason"] == "stop")
        .unwrap();
    let stop_points = stop["points"].as_array().unwrap();
    assert_eq!(stop_points.len(), 1);
    // openai-chat: 5 + 2 = 7. gemini's 100 must NOT be summed in.
    assert_eq!(stop_points[0][1].as_u64().unwrap(), 7);

    let end_turn = series
        .iter()
        .find(|s| s["finish_reason"] == "end_turn")
        .unwrap();
    let et_points = end_turn["points"].as_array().unwrap();
    assert_eq!(et_points.len(), 1);
    assert_eq!(et_points[0][1].as_u64().unwrap(), 9);
}

#[tokio::test]
async fn finish_reasons_endpoint_filters_by_server_ip() {
    // Per-server rows live in the (*, *, S) tier. With `?server_ip=10.0.0.1`
    // and no wire/model filter, only that server's rows should be summed.
    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();

    let ts: i64 = 1_700_000_000_000_000;
    let mk = |server: &str, reason: &str, count: u64| LlmFinishMetric {
        timestamp_us: ts,
        source_id: String::new(),
        granularity: "1m".to_string(),
        wire_api: "*".to_string(),
        model: "*".to_string(),
        server_ip: server.to_string(),
        finish_reason: reason.to_string(),
        count,
    };
    <DuckDbBackend as h_storage::StorageBackend>::write_finish_metrics(
        &backend,
        vec![
            mk("10.0.0.1", "end_turn", 5),
            mk("10.0.0.1", "tool_use", 2),
            mk("10.0.0.2", "end_turn", 7),
            // Cross-server rollup — must be excluded by the IN-list filter.
            mk("*", "end_turn", 99),
        ],
    )
    .await
    .unwrap();

    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    let start_s = (ts / 1_000_000) - 1;
    let end_s = (ts / 1_000_000) + 60;
    let uri = format!(
        "/api/metrics/finish-reasons?start={start_s}&end={end_s}&granularity=1m\
         &server_ip=10.0.0.1"
    );
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let series = v["data"]["series"].as_array().expect("series array");

    let names: Vec<&str> = series
        .iter()
        .map(|s| s["finish_reason"].as_str().unwrap())
        .collect();
    // Only 10.0.0.1's reasons; 10.0.0.2's end_turn=7 and the *-rollup's 99
    // must not appear.
    assert_eq!(names, vec!["end_turn", "tool_use"]);

    let end_turn = series
        .iter()
        .find(|s| s["finish_reason"] == "end_turn")
        .unwrap();
    assert_eq!(end_turn["points"][0][1].as_u64().unwrap(), 5);
}

/// `/api/metrics/timeseries` must anchor its X-axis on the full aligned
/// `[ceil(start/gran)*gran, ..., < end)` grid, regardless of which buckets
/// actually have data. Otherwise charts on the same page (e.g. `call_count`
/// vs `ttft_avg` while calls are still in flight and have no Complete yet)
/// end up on different time grids — recharts collapses each chart's X-axis
/// to whichever sub-range it sees, and the dashboards look inconsistent.
#[tokio::test]
async fn timeseries_endpoint_backfills_full_grid_for_sparse_data() {
    use h_metrics::model::LlmMetric;

    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();

    // Aligned 1m bucket. Only one row in the middle of a 5-bucket window —
    // the other four minutes must come back as NULL placeholders, not be
    // dropped from the response.
    let ts: i64 = 1_700_000_040_000_000; // multiple of 60_000_000 us
    let row = LlmMetric {
        timestamp_us: ts + 120_000_000, // bucket 3 of 5
        source_id: String::new(),
        granularity: "1m",
        wire_api: "*".to_string(),
        model: "*".to_string(),
        server_ip: "*".to_string(),
        call_count: 7,
        stream_count: 0,
        non_stream_count: 0,
        active_calls_sum: 0,
        active_calls_sample_count: 0,
        active_calls_max: 0,
        total_input_tokens: 0,
        input_token_count: 0,
        total_output_tokens: 0,
        output_token_count: 0,
        total_cache_read_input_tokens: 0,
        total_cache_creation_input_tokens: 0,
        error_count: 0,
        error_4xx_count: 0,
        error_429_count: 0,
        error_5xx_count: 0,
        ttft_sum: 0.0,
        ttft_count: 0,
        ttft_p50: None,
        ttft_p95: None,
        ttft_p99: None,
        ttft_stream_sum: 0.0,
        ttft_stream_count: 0,
        ttft_stream_p50: None,
        ttft_stream_p95: None,
        ttft_stream_p99: None,
        ttft_nonstream_sum: 0.0,
        ttft_nonstream_count: 0,
        ttft_nonstream_p50: None,
        ttft_nonstream_p95: None,
        ttft_nonstream_p99: None,
        e2e_sum: 0.0,
        e2e_count: 0,
        e2e_p50: None,
        e2e_p95: None,
        e2e_p99: None,
        tpot_sum: 0.0,
        tpot_count: 0,
        tpot_p50: None,
        tpot_p95: None,
        tpot_p99: None,
        tool_surface: None,
    };
    <DuckDbBackend as h_storage::StorageBackend>::write_metrics(&backend, vec![row])
        .await
        .unwrap();

    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    let start_s = ts / 1_000_000;
    let end_s = start_s + 300; // 5 minutes
    let uri = format!(
        "/api/metrics/timeseries?start={start_s}&end={end_s}&granularity=1m&fields=call_count"
    );
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let timestamps = v["data"]["timestamps"].as_array().expect("timestamps");
    let ts_secs: Vec<i64> = timestamps.iter().map(|t| t.as_i64().unwrap()).collect();
    assert_eq!(
        ts_secs,
        vec![
            start_s,
            start_s + 60,
            start_s + 120,
            start_s + 180,
            start_s + 240
        ],
        "X-axis must cover the full 5-bucket grid even when only one bucket has data"
    );

    let series = v["data"]["series"].as_array().expect("series");
    assert_eq!(series.len(), 1, "one field requested");
    let values = series[0]["values"].as_array().unwrap();
    assert!(values[0].is_null());
    assert!(values[1].is_null());
    assert_eq!(values[2].as_f64().unwrap(), 7.0);
    assert!(values[3].is_null());
    assert!(values[4].is_null());
}

/// When the entire window has no data, the response still carries the full
/// X-axis grid (with empty series). Charts then render an empty time range
/// instead of "No data available", matching siblings on the same page.
#[tokio::test]
async fn timeseries_endpoint_emits_grid_when_no_rows_exist() {
    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();
    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    let start_s = 1_700_000_040i64;
    let end_s = start_s + 180; // 3 minutes
    let uri = format!(
        "/api/metrics/timeseries?start={start_s}&end={end_s}&granularity=1m&fields=call_count"
    );
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let ts_secs: Vec<i64> = v["data"]["timestamps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_i64().unwrap())
        .collect();
    assert_eq!(ts_secs, vec![start_s, start_s + 60, start_s + 120]);
    assert_eq!(v["data"]["series"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn finish_reasons_endpoint_rejects_invalid_granularity() {
    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();
    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/metrics/finish-reasons?start=0&end=1&granularity=banana")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------- /api/llm-calls* ----------

#[tokio::test]
async fn invalid_status_code_returns_json_envelope() {
    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();
    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/llm-calls?start=0&end=1&status_code=200,abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["code"], 1001);
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("invalid status_code: abc"),
        "message: {}",
        v["message"]
    );
}

#[tokio::test]
async fn contains_params_parse() {
    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();
    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/llm-calls?start=0&end=1&client_ip=10.0.0.1&request_path=/v1/chat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

/// `/api/metrics/timeseries?tool_surface=...` must SUM only the rows whose
/// `tool_surface` column matches one of the CSV values, excluding other
/// surfaces. An invalid value returns 400 instead of silently degrading to
/// an empty result.
#[tokio::test]
async fn metrics_filters_by_tool_surface() {
    use h_metrics::model::LlmMetric;

    fn surface_row(ts_us: i64, surface: &str, call_count: u64) -> LlmMetric {
        LlmMetric {
            timestamp_us: ts_us,
            source_id: String::new(),
            granularity: "10s",
            // (*, *, *) tier — the read-path lands here when no
            // wire_api/model/server_ip filter is supplied.
            wire_api: "*".to_string(),
            model: "*".to_string(),
            server_ip: "*".to_string(),
            call_count,
            stream_count: 0,
            non_stream_count: 0,
            active_calls_sum: 0,
            active_calls_sample_count: 0,
            active_calls_max: 0,
            total_input_tokens: 0,
            input_token_count: 0,
            total_output_tokens: 0,
            output_token_count: 0,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            error_count: 0,
            error_4xx_count: 0,
            error_429_count: 0,
            error_5xx_count: 0,
            ttft_sum: 0.0,
            ttft_count: 0,
            ttft_p50: None,
            ttft_p95: None,
            ttft_p99: None,
            ttft_stream_sum: 0.0,
            ttft_stream_count: 0,
            ttft_stream_p50: None,
            ttft_stream_p95: None,
            ttft_stream_p99: None,
            ttft_nonstream_sum: 0.0,
            ttft_nonstream_count: 0,
            ttft_nonstream_p50: None,
            ttft_nonstream_p95: None,
            ttft_nonstream_p99: None,
            e2e_sum: 0.0,
            e2e_count: 0,
            e2e_p50: None,
            e2e_p95: None,
            e2e_p99: None,
            tpot_sum: 0.0,
            tpot_count: 0,
            tpot_p50: None,
            tpot_p95: None,
            tpot_p99: None,
            tool_surface: Some(surface.to_string()),
        }
    }

    let backend = DuckDbBackend::open(":memory:").unwrap();
    <DuckDbBackend as h_storage::StorageBackend>::init(&backend)
        .await
        .unwrap();

    let ts: i64 = 1_700_000_000_000_000; // multiple of 10_000_000 us
    <DuckDbBackend as h_storage::StorageBackend>::write_metrics(
        &backend,
        vec![
            surface_row(ts, "function_call", 100),
            surface_row(ts, "mcp", 50),
            surface_row(ts, "cli", 25),
        ],
    )
    .await
    .unwrap();

    let storage: std::sync::Arc<dyn h_storage::StorageBackend> = std::sync::Arc::new(backend);
    let app = router(
        storage,
        test_metrics_context(),
        test_runtime_config_context(),
        test_health_context(),
        std::sync::Arc::new(vec![]),
        h_turn::new_active_trace_registry(),
    );

    let start_s = ts / 1_000_000;
    let end_s = start_s + 10;

    // Filter to mcp + cli — function_call's 100 calls must NOT count.
    let uri = format!(
        "/api/metrics/timeseries?start={start_s}&end={end_s}\
         &granularity=10s&fields=call_count&tool_surface=mcp,cli"
    );
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let series = v["data"]["series"].as_array().expect("series");
    assert_eq!(series.len(), 1, "one field, ungrouped → one series");
    let values = series[0]["values"].as_array().unwrap();
    let total: f64 = values.iter().filter_map(|x| x.as_f64()).sum();
    assert_eq!(
        total, 75.0,
        "expected 50 (mcp) + 25 (cli), excluding function_call"
    );

    // Sanity: no filter sums all three surfaces.
    let uri_all = format!(
        "/api/metrics/timeseries?start={start_s}&end={end_s}\
         &granularity=10s&fields=call_count"
    );
    let resp_all = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&uri_all)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_all.status(), StatusCode::OK);
    let bytes_all = resp_all.into_body().collect().await.unwrap().to_bytes();
    let v_all: Value = serde_json::from_slice(&bytes_all).unwrap();
    let total_all: f64 = v_all["data"]["series"][0]["values"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_f64())
        .sum();
    assert_eq!(total_all, 175.0, "no-filter must sum all three surfaces");

    // Invalid surface → 400 (matches granularity validation pattern).
    let uri_bad = format!(
        "/api/metrics/timeseries?start={start_s}&end={end_s}\
         &granularity=10s&fields=call_count&tool_surface=foo"
    );
    let resp_bad = app
        .oneshot(
            Request::builder()
                .uri(&uri_bad)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_bad.status(), StatusCode::BAD_REQUEST);
}
