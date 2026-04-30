use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use ts_storage::query::{
    FinishReasonTimeseries, FinishReasonsQuery, MetricsModelsQuery, MetricsSummaryQuery,
    MetricsTimeseriesQuery,
};
use ts_storage::StorageBackend;

use crate::extractors::Query;
use crate::params::*;
use crate::response::{ApiError, ApiResponse};

const VALID_GRANULARITIES: &[&str] = &["10s", "1m", "5m", "1h"];

/// Map a granularity label to its window length in seconds. Mirrors
/// `ts_metrics::aggregator::GRANULARITIES`. Caller has already validated the
/// label against `VALID_GRANULARITIES`, so this is infallible at the call site.
fn granularity_secs(label: &str) -> i64 {
    match label {
        "10s" => 10,
        "1m" => 60,
        "5m" => 300,
        "1h" => 3600,
        _ => 60,
    }
}

#[derive(Serialize)]
struct TimeseriesSeries {
    name: String,
    group: Option<String>,
    values: Vec<Option<f64>>,
}

#[derive(Serialize)]
struct TimeseriesData {
    timestamps: Vec<i64>,
    series: Vec<TimeseriesSeries>,
}

pub async fn timeseries(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<TimeseriesParams>,
) -> Result<impl IntoResponse, ApiError> {
    if !VALID_GRANULARITIES.contains(&params.granularity.as_str()) {
        return Err(ApiError::InvalidParam(format!(
            "granularity must be one of: {}",
            VALID_GRANULARITIES.join(", ")
        )));
    }
    let fields = parse_csv(&Some(params.fields.clone()));
    if fields.is_empty() {
        return Err(ApiError::InvalidParam("fields is required".to_string()));
    }
    if let Some(ref gb) = params.group_by {
        if gb != "wire_api" && gb != "model" {
            return Err(ApiError::InvalidParam(
                "group_by must be 'wire_api' or 'model'".to_string(),
            ));
        }
    }

    let query = MetricsTimeseriesQuery {
        time_range: to_time_range(params.start, params.end),
        granularity: params.granularity,
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        fields: fields.clone(),
        group_by: params.group_by,
    };

    let rows = storage.query_metrics_timeseries(&query).await?;

    // Anchor the X-axis on the full aligned time grid `[ceil(start/gran)*gran,
    // ..., < end)` so every chart sharing the same `[start, end)` window sees
    // the same set of timestamps. The aggregator only writes rows for buckets
    // that observed events; without backfill, recharts collapses the X-axis
    // to whichever sub-range happened to have data, and different fields
    // (e.g. `call_count` vs `ttft_avg` while calls are still in flight) end
    // up on different time grids.
    let gran_sec = granularity_secs(&query.granularity);
    let timestamps: Vec<i64> = if params.end > params.start && gran_sec > 0 {
        let first_ts = (params.start + gran_sec - 1).div_euclid(gran_sec) * gran_sec;
        let mut out = Vec::new();
        let mut t = first_ts;
        while t < params.end {
            out.push(t);
            t += gran_sec;
        }
        out
    } else {
        Vec::new()
    };
    let ts_index: HashMap<i64, usize> = timestamps
        .iter()
        .enumerate()
        .map(|(i, &t)| (t, i))
        .collect();

    // Pivot: rows (each with timestamp + group + values) -> series[]. Rows
    // whose timestamp doesn't land on the grid (out-of-window or unaligned —
    // shouldn't happen for production data, defense-in-depth) are dropped.
    let mut series_map: BTreeMap<(String, Option<String>), Vec<Option<f64>>> = BTreeMap::new();
    for row in &rows {
        let Some(&ts_idx) = ts_index.get(&row.timestamp) else {
            continue;
        };
        for (i, field) in fields.iter().enumerate() {
            let key = (field.clone(), row.group.clone());
            let values = series_map
                .entry(key)
                .or_insert_with(|| vec![None; timestamps.len()]);
            values[ts_idx] = row.values.get(i).copied().flatten();
        }
    }

    let series = series_map
        .into_iter()
        .map(|((name, group), values)| TimeseriesSeries {
            name,
            group,
            values,
        })
        .collect();

    Ok(ApiResponse::ok(TimeseriesData { timestamps, series }))
}

pub async fn summary(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<SummaryParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = MetricsSummaryQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
    };
    let row = storage.query_metrics_summary(&query).await?;
    Ok(ApiResponse::ok(row))
}

#[derive(Serialize)]
struct ModelsData {
    models: Vec<ts_storage::query::MetricsModelRow>,
}

pub async fn models(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<ModelsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = MetricsModelsQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        limit: params.limit,
    };
    let rows = storage.query_metrics_models(&query).await?;
    Ok(ApiResponse::ok(ModelsData { models: rows }))
}

/// Query parameters for `GET /api/metrics/finish-reasons`.
///
/// Reads the long-format `llm_finish_metrics` table introduced in Phase 4.
/// Returns one timeseries per distinct raw `finish_reason` observed in the
/// requested window — values are passed through verbatim (no normalization).
///
/// `wire_api`, `model`, and `server_ip` accept comma-separated lists
/// ("anthropic,openai-chat") and behave like sibling `/api/metrics/*` endpoints
/// (see `to_dimension_filter`).
#[derive(Debug, Deserialize)]
pub struct FinishReasonsParams {
    /// Inclusive start in seconds since epoch (matches `/api/metrics/timeseries`).
    pub start: i64,
    /// Exclusive end in seconds since epoch.
    pub end: i64,
    pub granularity: String,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
}

#[derive(Serialize)]
struct FinishReasonsData {
    series: Vec<FinishReasonTimeseries>,
}

pub async fn finish_reasons(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<FinishReasonsParams>,
) -> Result<impl IntoResponse, ApiError> {
    if !VALID_GRANULARITIES.contains(&params.granularity.as_str()) {
        return Err(ApiError::InvalidParam(format!(
            "granularity must be one of: {}",
            VALID_GRANULARITIES.join(", ")
        )));
    }

    let query = FinishReasonsQuery {
        time_range: to_time_range(params.start, params.end),
        granularity: params.granularity,
        wire_apis: parse_csv(&params.wire_api),
        models: parse_csv(&params.model),
        server_ips: parse_csv(&params.server_ip),
    };
    let series = storage.query_finish_reasons(&query).await?;
    Ok(ApiResponse::ok(FinishReasonsData { series }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;
    use ts_metrics::model::LlmFinishMetric;
    use ts_storage::duckdb::DuckDbBackend;

    use crate::{router, ApiMetricsContext};

    fn test_metrics_context() -> ApiMetricsContext {
        let sys = ts_common::internal_metrics::MetricsSystem::new();
        ApiMetricsContext {
            pipelines: vec![],
            global: sys.start(),
        }
    }

    #[tokio::test]
    async fn finish_reasons_endpoint_returns_one_series_per_raw_value() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
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
        <DuckDbBackend as ts_storage::StorageBackend>::write_finish_metrics(
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

        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(storage, test_metrics_context());

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
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
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
        <DuckDbBackend as ts_storage::StorageBackend>::write_finish_metrics(
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

        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(storage, test_metrics_context());

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
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
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
        <DuckDbBackend as ts_storage::StorageBackend>::write_finish_metrics(
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

        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(storage, test_metrics_context());

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
        use ts_metrics::model::LlmMetric;

        let backend = DuckDbBackend::open(":memory:").unwrap();
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
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
        };
        <DuckDbBackend as ts_storage::StorageBackend>::write_metrics(&backend, vec![row])
            .await
            .unwrap();

        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(storage, test_metrics_context());

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
            vec![start_s, start_s + 60, start_s + 120, start_s + 180, start_s + 240],
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
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
            .await
            .unwrap();
        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(storage, test_metrics_context());

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
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
            .await
            .unwrap();
        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(storage, test_metrics_context());
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
}
