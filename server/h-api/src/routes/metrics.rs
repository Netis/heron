use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use h_storage::query::{
    FinishReasonTimeseries, FinishReasonsQuery, MetricsModelsQuery, MetricsSummaryQuery,
    MetricsTimeseriesQuery,
};
use h_storage::StorageBackend;
use serde::{Deserialize, Serialize};

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

/// Ceiling on `(window / granularity)` for the timeseries endpoints.
///
/// The console derives granularity from the window it is showing
/// (`use-metrics.ts`), which bounds it to at most 8,640 points — a day at 5m,
/// its densest pairing. Its widest preset, 7d, lands on 1h and 168 points; only
/// a hand-entered custom range past about 2.3 years reaches this limit. What
/// this stops is the raw API, which until now would accept `7d` at `10s`
/// (60,480 buckets) or `30d` at `1m` (43,200) and simply take the consequences.
///
/// Those consequences are real on both sides of the call. This handler
/// materializes the full aligned grid and then one `Vec<Option<f64>>` of that
/// length per (field, group) series, so the response alone is `buckets ×
/// fields × groups` — 30 days at 1m across 200 models and five fields is 43
/// million values before anything is serialized. On the storage side the same
/// request is a `buckets × cardinality` aggregation, measured at 283 s for 1.75
/// million groups on aglake. Neither number is a bug; the request is simply
/// asking for a chart that cannot be drawn.
///
/// Refusing with a message that names a granularity that would fit is better
/// than either answer available otherwise: minutes of work, or an
/// out-of-memory. Same reasoning as `max_page_offset` in the aglake backend.
const MAX_TIMESERIES_BUCKETS: i64 = 20_000;

/// Reject a window/granularity pair that would produce an unusable number of
/// points, naming the coarsest granularity that would have fit.
fn check_bucket_count(start: i64, end: i64, granularity: &str) -> Result<(), ApiError> {
    let gran = granularity_secs(granularity);
    if end <= start || gran <= 0 {
        return Ok(());
    }
    let buckets = (end - start) / gran;
    if buckets <= MAX_TIMESERIES_BUCKETS {
        return Ok(());
    }
    let suggestion = VALID_GRANULARITIES
        .iter()
        .find(|g| (end - start) / granularity_secs(g) <= MAX_TIMESERIES_BUCKETS)
        .map(|g| format!("use granularity={g}"))
        .unwrap_or_else(|| "narrow the time range".to_string());
    Err(ApiError::InvalidParam(format!(
        "granularity={granularity} over this time range would produce {buckets} \
         points, above the {MAX_TIMESERIES_BUCKETS} limit — {suggestion}, or \
         request a shorter window"
    )))
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
    check_bucket_count(params.start, params.end, &params.granularity)?;
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

    check_bucket_count(params.start, params.end, &params.granularity)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;

    /// Every window/granularity pair the console can produce must pass. The
    /// console derives granularity from the window (`use-metrics.ts`), so
    /// these are the pairs it actually asks for — if the guard rejected one,
    /// it would break the UI rather than protect it.
    #[test]
    fn every_pairing_the_console_produces_is_allowed() {
        for (window, gran) in [
            (15 * 60, "10s"),  // <=15min -> 10s
            (2 * HOUR, "1m"),  // <=2h    -> 1m
            (24 * HOUR, "5m"), // <=24h   -> 5m
            (7 * DAY, "1h"),       // >24h -> 1h; the widest preset, 168 points
            (30 * DAY, "1h"),      // a custom range
            (365 * DAY, "1h"),     // 8,760 points
            (2 * 365 * DAY, "1h"), // still under the limit, at 17,520
        ] {
            assert!(
                check_bucket_count(0, window, gran).is_ok(),
                "console pairing ({window}s @ {gran}) must be allowed"
            );
        }
    }

    /// The raw API can name any pair. These are the ones that would produce a
    /// chart nobody can read and an aggregation nobody wants to wait for.
    #[test]
    fn absurd_pairings_are_refused_with_a_usable_suggestion() {
        let msg = match check_bucket_count(0, 7 * DAY, "10s") {
            Err(ApiError::InvalidParam(m)) => m,
            other => panic!("expected an InvalidParam rejection, got {other:?}"),
        };
        assert!(msg.contains("60480"), "must say how many points: {msg}");
        // 7d/5m = 2,016, so 5m is the coarsest that fits and 1m (10,080) does
        // not — the suggestion has to be the first one that actually works.
        assert!(
            msg.contains("granularity=1m"),
            "must name a granularity that fits: {msg}"
        );

        assert!(check_bucket_count(0, 30 * DAY, "1m").is_err());
        assert!(check_bucket_count(0, 365 * DAY, "5m").is_err());
    }

    /// The boundary itself is allowed; one bucket past it is not.
    #[test]
    fn the_limit_is_inclusive() {
        let at = MAX_TIMESERIES_BUCKETS * 10;
        assert!(check_bucket_count(0, at, "10s").is_ok());
        assert!(check_bucket_count(0, at + 10, "10s").is_err());
    }

    /// A degenerate range is not this function's business — `to_time_range`
    /// already rejects it, and answering "0 buckets, fine" keeps the two
    /// validations from disagreeing about which error the caller sees.
    #[test]
    fn an_empty_or_inverted_range_is_left_to_the_range_validator() {
        assert!(check_bucket_count(100, 100, "10s").is_ok());
        assert!(check_bucket_count(200, 100, "10s").is_ok());
    }
}
