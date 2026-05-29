use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use h_storage::query::{
    FinishReasonTimeseries, FinishReasonsQuery, MetricsModelsQuery, MetricsSummaryQuery,
    MetricsTimeseriesQuery,
};
use h_storage::StorageBackend;

use crate::extractors::Query;
use crate::params::*;
use crate::response::{ApiError, ApiResponse};

const VALID_GRANULARITIES: &[&str] = &["10s", "1m", "5m", "1h"];

/// Accepted values for the `tool_surface=` filter on `/api/metrics/*`. Mirrors
/// `h_common::agent::ToolSurface`'s serde representation (`snake_case`). The
/// validator rejects any unknown token with a 400 so a typo doesn't silently
/// degrade to an empty result set — same pattern as `granularity` validation.
const VALID_TOOL_SURFACES: &[&str] = &["function_call", "mcp", "cli", "mixed", "unknown"];

fn validate_tool_surfaces(values: &[String]) -> Result<(), ApiError> {
    for v in values {
        if !VALID_TOOL_SURFACES.contains(&v.as_str()) {
            return Err(ApiError::InvalidParam(format!(
                "tool_surface={v}: must be one of: {}",
                VALID_TOOL_SURFACES.join(", ")
            )));
        }
    }
    Ok(())
}

/// Map a granularity label to its window length in seconds. Mirrors
/// `h_metrics::aggregator::GRANULARITIES`. Caller has already validated the
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
    let tool_surfaces = parse_csv(&params.tool_surface);
    validate_tool_surfaces(&tool_surfaces)?;

    let query = MetricsTimeseriesQuery {
        time_range: to_time_range(params.start, params.end)?,
        granularity: params.granularity,
        filter: to_dimension_filter(
            &params.wire_api,
            &params.model,
            &params.server_ip,
            &params.tool_surface,
        ),
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
    let tool_surfaces = parse_csv(&params.tool_surface);
    validate_tool_surfaces(&tool_surfaces)?;
    let query = MetricsSummaryQuery {
        time_range: to_time_range(params.start, params.end)?,
        filter: to_dimension_filter(
            &params.wire_api,
            &params.model,
            &params.server_ip,
            &params.tool_surface,
        ),
    };
    let row = storage.query_metrics_summary(&query).await?;
    Ok(ApiResponse::ok(row))
}

#[derive(Serialize)]
struct ModelsData {
    models: Vec<h_storage::query::MetricsModelRow>,
}

pub async fn models(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<ModelsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let tool_surfaces = parse_csv(&params.tool_surface);
    validate_tool_surfaces(&tool_surfaces)?;
    let query = MetricsModelsQuery {
        time_range: to_time_range(params.start, params.end)?,
        filter: to_dimension_filter(
            &params.wire_api,
            &params.model,
            &params.server_ip,
            &params.tool_surface,
        ),
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
        time_range: to_time_range(params.start, params.end)?,
        granularity: params.granularity,
        wire_apis: parse_csv(&params.wire_api),
        models: parse_csv(&params.model),
        server_ips: parse_csv(&params.server_ip),
    };
    let series = storage.query_finish_reasons(&query).await?;
    Ok(ApiResponse::ok(FinishReasonsData { series }))
}
