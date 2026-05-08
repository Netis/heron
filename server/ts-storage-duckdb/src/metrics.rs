//! `llm_metrics` + `llm_finish_metrics` table I/O — sliding-window
//! aggregation writes plus the read-side time-series, summary, model-axis,
//! and finish-reason rollups.

use duckdb::types::{TimeUnit, Value};
use ts_common::error::{AppError, Result};
use ts_metrics::model::{LlmFinishMetric, LlmMetric};
use ts_storage::query::*;

use crate::util::{
    build_dimension_where, build_dimension_where_for_group, sql_in_list, us_to_timestamp,
};
use crate::DuckDbBackend;

struct PreparedMetric {
    timestamp: Value,
    source_id: String,
    granularity: &'static str,
    wire_api: String,
    model: String,
    server_ip: String,
    inner: LlmMetric,
}

fn prepare_metric(m: LlmMetric) -> PreparedMetric {
    PreparedMetric {
        timestamp: Value::Timestamp(TimeUnit::Microsecond, m.timestamp_us),
        source_id: m.source_id.clone(),
        granularity: m.granularity,
        wire_api: m.wire_api.clone(),
        model: m.model.clone(),
        server_ip: m.server_ip.clone(),
        inner: m,
    }
}

/// All valid numeric metric field names accepted by `query_metrics_timeseries`.
/// Virtual `*_avg` fields resolve to `SUM(*_sum) / SUM(*_count)` at query time;
/// the raw `*_sum` / `*_count` fields are also accepted for callers that want
/// to do their own aggregation.
const VALID_METRIC_FIELDS: &[&str] = &[
    "call_count",
    "stream_count",
    "non_stream_count",
    "active_calls_avg",
    "active_calls_sum",
    "active_calls_sample_count",
    "active_calls_max",
    "total_input_tokens",
    "input_token_count",
    "total_output_tokens",
    "output_token_count",
    "input_tokens_avg",
    "output_tokens_avg",
    "total_cache_read_input_tokens",
    "total_cache_creation_input_tokens",
    "error_count",
    "error_4xx_count",
    "error_429_count",
    "error_5xx_count",
    // Phase 5 will read llm_finish_metrics directly via a dedicated query
    // path; finish-reason fields are no longer columns of llm_metrics.
    "ttft_avg",
    "ttft_sum",
    "ttft_count",
    "ttft_p50",
    "ttft_p95",
    "ttft_p99",
    "e2e_avg",
    "e2e_sum",
    "e2e_count",
    "e2e_p50",
    "e2e_p95",
    "e2e_p99",
    "tpot_avg",
    "tpot_sum",
    "tpot_count",
    "tpot_p50",
    "tpot_p95",
    "tpot_p99",
];

/// Build the per-field SQL expressions used by `query_metrics_timeseries`.
///
/// * Additive fields (counts, totals, `*_sum`, `*_count`) → plain `SUM`.
/// * Peak fields (`*_max`) → `MAX` — taking SUM across multiple rows at the
///   same timestamp (different sources, or specific dim rows under a grouped
///   query) inflates a peak by stacking each row's local peak.
/// * Averages (`*_avg`) → exact ratio `SUM(*_sum) / SUM(*_count)`, derived
///   from the additive sum+count pair so multi-row aggregation (slow-response
///   windows, cross-source merging) stays correct.
/// * Per-row percentiles (`*_p50/p95/p99`) → weighted average by the matching
///   `*_count` (number of samples contributing to the row's digest). This is
///   an approximation until serialized t-digest bytes land; weighting by the
///   count field (rather than `call_count`) keeps slow-response rows with
///   `call_count=0` from falsely collapsing the result to zero.
fn build_field_exprs(fields: &[String]) -> Vec<String> {
    fields
        .iter()
        .map(|f| {
            if MAX_FIELDS.contains(&f.as_str()) {
                format!("CAST(MAX({f}) AS DOUBLE)")
            } else if SUM_FIELDS.contains(&f.as_str()) {
                format!("CAST(SUM({f}) AS DOUBLE)")
            } else if let Some((sum_col, count_col)) = avg_pair(f) {
                format!(
                    "CASE WHEN SUM({count_col}) > 0 \
                     THEN SUM({sum_col}) / SUM({count_col}) ELSE NULL END"
                )
            } else if f.ends_with("_p50") || f.ends_with("_p95") || f.ends_with("_p99") {
                let weight = percentile_weight(f);
                format!(
                    "CASE WHEN SUM({weight}) > 0 \
                     THEN SUM({f} * {weight}) / SUM({weight}) ELSE NULL END"
                )
            } else {
                format!("CAST(SUM({f}) AS DOUBLE)")
            }
        })
        .collect()
}

/// Map `*_avg` virtual field → `(sum_column, count_column)` pair in the
/// physical schema. `None` for fields that are not averages.
fn avg_pair(f: &str) -> Option<(&'static str, &'static str)> {
    match f {
        "active_calls_avg" => Some(("active_calls_sum", "active_calls_sample_count")),
        "input_tokens_avg" => Some(("total_input_tokens", "input_token_count")),
        "output_tokens_avg" => Some(("total_output_tokens", "output_token_count")),
        "ttft_avg" => Some(("ttft_sum", "ttft_count")),
        "e2e_avg" => Some(("e2e_sum", "e2e_count")),
        "tpot_avg" => Some(("tpot_sum", "tpot_count")),
        _ => None,
    }
}

/// Weight column for percentile weighted-avg aggregation.
fn percentile_weight(field: &str) -> &'static str {
    if field.starts_with("ttft") {
        "ttft_count"
    } else if field.starts_with("e2e") {
        "e2e_count"
    } else if field.starts_with("tpot") {
        "tpot_count"
    } else {
        "call_count"
    }
}

/// Fields that represent counts or totals (use SUM when aggregating across groups).
const SUM_FIELDS: &[&str] = &[
    "call_count",
    "stream_count",
    "non_stream_count",
    "active_calls_sum",
    "active_calls_sample_count",
    "total_input_tokens",
    "input_token_count",
    "total_output_tokens",
    "output_token_count",
    "total_cache_read_input_tokens",
    "total_cache_creation_input_tokens",
    "error_count",
    "error_4xx_count",
    "error_429_count",
    "error_5xx_count",
    "ttft_sum",
    "ttft_count",
    "e2e_sum",
    "e2e_count",
    "tpot_sum",
    "tpot_count",
];

/// Fields that represent peaks (use MAX, never SUM, when aggregating across
/// rows at the same timestamp — different sources or different specific-dim
/// rows under a grouped query).
const MAX_FIELDS: &[&str] = &["active_calls_max"];


impl DuckDbBackend {
    pub(crate) async fn write_metrics(&self, metrics: Vec<LlmMetric>) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        let conn = self.write_metrics_conn.clone();
        tokio::task::spawn_blocking(move || {
            let prepared: Vec<PreparedMetric> = metrics.into_iter().map(prepare_metric).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("llm_metrics")
                .map_err(|e| AppError::Storage(format!("failed to create appender: {e}")))?;
            for p in &prepared {
                let m = &p.inner;
                appender
                    .append_row(duckdb::params![
                        p.timestamp,
                        p.source_id,
                        p.granularity,
                        p.wire_api,
                        p.model,
                        p.server_ip,
                        m.call_count,
                        m.stream_count,
                        m.non_stream_count,
                        m.active_calls_sum,
                        m.active_calls_sample_count,
                        m.active_calls_max,
                        m.total_input_tokens,
                        m.input_token_count,
                        m.total_output_tokens,
                        m.output_token_count,
                        m.total_cache_read_input_tokens,
                        m.total_cache_creation_input_tokens,
                        m.error_count,
                        m.error_4xx_count,
                        m.error_429_count,
                        m.error_5xx_count,
                        m.ttft_sum,
                        m.ttft_count,
                        m.ttft_p50,
                        m.ttft_p95,
                        m.ttft_p99,
                        m.e2e_sum,
                        m.e2e_count,
                        m.e2e_p50,
                        m.e2e_p95,
                        m.e2e_p99,
                        m.tpot_sum,
                        m.tpot_count,
                        m.tpot_p50,
                        m.tpot_p95,
                        m.tpot_p99,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append metric: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush metrics: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn write_finish_metrics(&self, metrics: Vec<LlmFinishMetric>) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        // Shares the metrics writer Mutex with `write_metrics` so the two
        // long/wide rollups for one bucket flush serialize against each other
        // — they always come in pairs from the bucket finalizer and writing
        // them on the same connection avoids cross-table interleaving.
        let conn = self.write_metrics_conn.clone();
        tokio::task::spawn_blocking(move || {
            // Pre-format the timestamp Value outside the writer lock, same
            // pattern as `prepare_metric`.
            let prepared: Vec<(Value, LlmFinishMetric)> = metrics
                .into_iter()
                .map(|m| (Value::Timestamp(TimeUnit::Microsecond, m.timestamp_us), m))
                .collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn.appender("llm_finish_metrics").map_err(|e| {
                AppError::Storage(format!("failed to create llm_finish_metrics appender: {e}"))
            })?;
            for (ts, m) in &prepared {
                appender
                    .append_row(duckdb::params![
                        ts,
                        m.source_id,
                        m.granularity,
                        m.wire_api,
                        m.model,
                        m.server_ip,
                        m.finish_reason,
                        m.count,
                    ])
                    .map_err(|e| {
                        AppError::Storage(format!("failed to append finish metric: {e}"))
                    })?;
            }
            appender.flush().map_err(|e| {
                AppError::Storage(format!("failed to flush llm_finish_metrics: {e}"))
            })?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_metrics_timeseries(
        &self,
        query: &MetricsTimeseriesQuery,
    ) -> Result<Vec<MetricsTimeseriesRow>> {
        // Validate all requested fields
        for field in &query.fields {
            if !VALID_METRIC_FIELDS.contains(&field.as_str()) {
                return Err(AppError::Storage(format!("invalid metric field: {field}")));
            }
        }

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let field_exprs = build_field_exprs(&query.fields);
            let fields_sql = field_exprs.join(", ");
            let rows = if let Some(ref group_by) = query.group_by {
                // Grouped query: aggregate across the group dimension plus source_id.
                let dim_where = build_dimension_where_for_group(&query.filter, group_by);
                let sql = format!(
                    "SELECT epoch(timestamp) AS ts, {group_by}, {fields_sql} \
                     FROM llm_metrics \
                     WHERE {dim_where} AND granularity = ? AND timestamp >= ? AND timestamp < ? \
                     GROUP BY timestamp, {group_by} \
                     ORDER BY timestamp, {group_by}"
                );

                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    AppError::Storage(format!("failed to prepare timeseries query: {e}"))
                })?;

                let mut rows = Vec::new();
                let mut query_rows = stmt
                    .query(duckdb::params![query.granularity, start_ts, end_ts])
                    .map_err(|e| {
                        AppError::Storage(format!("failed to execute timeseries query: {e}"))
                    })?;
                while let Some(row) = query_rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let ts: i64 = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("ts read error: {e}")))?;
                    let group: String = row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("group read error: {e}")))?;
                    let mut values = Vec::new();
                    for i in 0..query.fields.len() {
                        let v: Option<f64> = row
                            .get(2 + i)
                            .map_err(|e| AppError::Storage(format!("field read error: {e}")))?;
                        values.push(v);
                    }
                    rows.push(MetricsTimeseriesRow {
                        timestamp: ts,
                        group: Some(group),
                        values,
                    });
                }
                rows
            } else {
                // Ungrouped query: still must GROUP BY timestamp because the
                // per-source aggregators emit one row per source per (ts,
                // dim). Without the GROUP BY we'd return N overlapping rows
                // at each timestamp (N = number of capture sources).
                let dim_where = build_dimension_where(&query.filter);
                let sql = format!(
                    "SELECT epoch(timestamp) AS ts, {fields_sql} \
                     FROM llm_metrics \
                     WHERE {dim_where} AND granularity = ? AND timestamp >= ? AND timestamp < ? \
                     GROUP BY timestamp \
                     ORDER BY timestamp"
                );

                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    AppError::Storage(format!("failed to prepare timeseries query: {e}"))
                })?;

                let mut rows = Vec::new();
                let mut query_rows = stmt
                    .query(duckdb::params![query.granularity, start_ts, end_ts])
                    .map_err(|e| {
                        AppError::Storage(format!("failed to execute timeseries query: {e}"))
                    })?;
                while let Some(row) = query_rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let ts: i64 = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("ts read error: {e}")))?;
                    let mut values = Vec::new();
                    for i in 0..query.fields.len() {
                        let v: Option<f64> = row
                            .get(1 + i)
                            .map_err(|e| AppError::Storage(format!("field read error: {e}")))?;
                        values.push(v);
                    }
                    rows.push(MetricsTimeseriesRow {
                        timestamp: ts,
                        group: None,
                        values,
                    });
                }
                rows
            };

            Ok(rows)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_metrics_summary(
        &self,
        query: &MetricsSummaryQuery,
    ) -> Result<MetricsSummaryRow> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let dim_where = build_dimension_where(&query.filter);
            let sql = format!(
                "
                SELECT
                    COALESCE(SUM(call_count), 0),
                    COALESCE(SUM(error_count), 0),
                    COALESCE(SUM(error_4xx_count), 0),
                    COALESCE(SUM(error_429_count), 0),
                    COALESCE(SUM(error_5xx_count), 0),
                    COALESCE(SUM(total_input_tokens), 0),
                    COALESCE(SUM(total_output_tokens), 0),
                    CASE WHEN SUM(ttft_count) > 0
                         THEN SUM(ttft_sum) / SUM(ttft_count) ELSE NULL END,
                    CASE WHEN SUM(e2e_count) > 0
                         THEN SUM(e2e_sum) / SUM(e2e_count) ELSE NULL END,
                    CASE WHEN SUM(tpot_count) > 0
                         THEN SUM(tpot_sum) / SUM(tpot_count) ELSE NULL END
                FROM llm_metrics
                WHERE {dim_where}
                  AND granularity = '10s'
                  AND timestamp >= ? AND timestamp < ?
            "
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare summary query: {e}")))?;

            let row = stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| {
                    Ok(MetricsSummaryRow {
                        call_count: row.get::<_, u64>(0)?,
                        error_count: row.get::<_, u64>(1)?,
                        error_4xx_count: row.get::<_, u64>(2)?,
                        error_429_count: row.get::<_, u64>(3)?,
                        error_5xx_count: row.get::<_, u64>(4)?,
                        total_input_tokens: row.get::<_, u64>(5)?,
                        total_output_tokens: row.get::<_, u64>(6)?,
                        ttft_avg: row.get::<_, Option<f64>>(7)?,
                        e2e_avg: row.get::<_, Option<f64>>(8)?,
                        tpot_avg: row.get::<_, Option<f64>>(9)?,
                    })
                })
                .map_err(|e| AppError::Storage(format!("failed to execute summary query: {e}")))?;

            Ok(row)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_metrics_models(
        &self,
        query: &MetricsModelsQuery,
    ) -> Result<Vec<MetricsModelRow>> {
        const VALID_SORT_FIELDS: &[&str] = &[
            "call_count",
            "error_count",
            "total_input_tokens",
            "total_output_tokens",
            "ttft_avg",
            "ttft_p95",
            "e2e_avg",
            "e2e_p95",
            "tpot_avg",
        ];

        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.to_uppercase() == "ASC" {
            "ASC"
        } else {
            "DESC"
        };

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();
        let sort_order = sort_order.to_string();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let sort_by = &query.sort_by;
            let limit = query.limit;
            // Per-(wire_api, model) breakdown shares the grouped-tier logic:
            // both dimensions are always specific, server_ip follows filter.
            let dim_where = build_dimension_where_for_group(&query.filter, "wire_api");

            let sql = format!(
                "
                SELECT * FROM (
                    SELECT
                        wire_api,
                        model,
                        COALESCE(SUM(call_count), 0) AS call_count,
                        COALESCE(SUM(error_count), 0) AS error_count,
                        COALESCE(SUM(error_4xx_count), 0) AS error_4xx_count,
                        COALESCE(SUM(error_429_count), 0) AS error_429_count,
                        COALESCE(SUM(error_5xx_count), 0) AS error_5xx_count,
                        COALESCE(SUM(total_input_tokens), 0) AS total_input_tokens,
                        COALESCE(SUM(total_output_tokens), 0) AS total_output_tokens,
                        CASE WHEN SUM(ttft_count) > 0
                             THEN SUM(ttft_sum) / SUM(ttft_count)
                             ELSE NULL END AS ttft_avg,
                        CASE WHEN SUM(ttft_count) > 0
                             THEN SUM(ttft_p95 * ttft_count) / SUM(ttft_count)
                             ELSE NULL END AS ttft_p95,
                        CASE WHEN SUM(e2e_count) > 0
                             THEN SUM(e2e_sum) / SUM(e2e_count)
                             ELSE NULL END AS e2e_avg,
                        CASE WHEN SUM(e2e_count) > 0
                             THEN SUM(e2e_p95 * e2e_count) / SUM(e2e_count)
                             ELSE NULL END AS e2e_p95,
                        CASE WHEN SUM(tpot_count) > 0
                             THEN SUM(tpot_sum) / SUM(tpot_count)
                             ELSE NULL END AS tpot_avg
                    FROM llm_metrics
                    WHERE {dim_where}
                      AND granularity = '10s'
                      AND timestamp >= ? AND timestamp < ?
                    GROUP BY wire_api, model
                ) sub
                ORDER BY {sort_by} {sort_order}
                LIMIT {limit}
            "
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare models query: {e}")))?;

            let mut rows = Vec::new();
            let mut query_rows = stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute models query: {e}")))?;

            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                rows.push(MetricsModelRow {
                    wire_api: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    model: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    call_count: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_count: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_4xx_count: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_429_count: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_5xx_count: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_input_tokens: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_output_tokens: row
                        .get(8)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttft_avg: row
                        .get(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttft_p95: row
                        .get(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    e2e_avg: row
                        .get(11)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    e2e_p95: row
                        .get(12)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    tpot_avg: row
                        .get(13)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                });
            }

            Ok(rows)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_finish_reasons(
        &self,
        query: &FinishReasonsQuery,
    ) -> Result<Vec<FinishReasonTimeseries>> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            // Pick the matching pre-aggregated dimension tier:
            //   - wire_apis/models both non-empty → (W, M, *) tier, IN-list filter
            //   - both empty → (*, *, *) tier
            //   - only one non-empty → drop to (W, M, *) tier and SUM over
            //     `<other_dim> != '*'` rows. The writer emits (W,M,*), (W,M,·),
            //     (*,*,·), (*,*,*) tiers (see `dimension_keys`); selecting
            //     `wire_api IN (…) AND model != '*' AND server_ip = '*'` lands
            //     squarely on the (W, M, *) rows for the requested wire_apis,
            //     and SUM gives the cross-model rollup the caller wants.
            //
            // Inlined via format! to match the file's `sql_in_list` convention
            // for IN-list filters. DuckDB has no backslash escaping in string
            // literals, so the doubled-quote escape (`''`) inside `sql_in_list`
            // is complete and safe against injection.
            let has_wire = !query.wire_apis.is_empty();
            let has_model = !query.models.is_empty();
            let has_server = !query.server_ips.is_empty();
            let wire_clause = if has_wire {
                format!("wire_api IN ({})", sql_in_list(&query.wire_apis))
            } else if has_model {
                "wire_api != '*'".to_string()
            } else {
                "wire_api = '*'".to_string()
            };
            let model_clause = if has_model {
                format!("model IN ({})", sql_in_list(&query.models))
            } else if has_wire {
                "model != '*'".to_string()
            } else {
                "model = '*'".to_string()
            };
            // server_ip is independent of wire/model: aggregator emits both
            // (·,·,S) and (·,·,*) tiers in parallel for each (W,M) state.
            let server_clause = if has_server {
                format!("server_ip IN ({})", sql_in_list(&query.server_ips))
            } else {
                "server_ip = '*'".to_string()
            };

            let sql = format!(
                "SELECT epoch_us(timestamp) AS ts_us, finish_reason, SUM(count) AS c \
                 FROM llm_finish_metrics \
                 WHERE granularity = ? \
                   AND timestamp >= ? AND timestamp < ? \
                   AND {wire_clause} AND {model_clause} \
                   AND {server_clause} \
                 GROUP BY ts_us, finish_reason \
                 ORDER BY finish_reason ASC, ts_us ASC"
            );

            let mut stmt = conn.prepare(&sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare finish-reasons query: {e}"))
            })?;
            let mut query_rows = stmt
                .query(duckdb::params![query.granularity, start_ts, end_ts])
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute finish-reasons query: {e}"))
                })?;

            // Bucket rows into series by finish_reason. ORDER BY guarantees
            // each series' points arrive contiguously and timestamp-sorted.
            let mut out: Vec<FinishReasonTimeseries> = Vec::new();
            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let ts_us: i64 = row
                    .get(0)
                    .map_err(|e| AppError::Storage(format!("ts read error: {e}")))?;
                let finish_reason: String = row
                    .get(1)
                    .map_err(|e| AppError::Storage(format!("reason read error: {e}")))?;
                let count: u64 = row
                    .get(2)
                    .map_err(|e| AppError::Storage(format!("count read error: {e}")))?;

                match out.last_mut() {
                    Some(last) if last.finish_reason == finish_reason => {
                        last.points.push((ts_us, count));
                    }
                    _ => out.push(FinishReasonTimeseries {
                        finish_reason,
                        points: vec![(ts_us, count)],
                    }),
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            // Source: llm_finish_metrics. The `wire_api != '*'` filter excludes
            // the cross-wire-api rollup tier; finish_reason is always concrete
            // in this table (no `*` rows for finish_reason itself), but we keep
            // the symmetry for safety against future schema changes.
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT wire_api, finish_reason \
                     FROM llm_finish_metrics \
                     WHERE wire_api != '*' AND finish_reason != '*' \
                     ORDER BY wire_api, finish_reason",
                )
                .map_err(|e| {
                    AppError::Storage(format!(
                        "failed to prepare distinct_finish_reasons query: {e}"
                    ))
                })?;
            let mut rows = stmt.query([]).map_err(|e| {
                AppError::Storage(format!(
                    "failed to execute distinct_finish_reasons query: {e}"
                ))
            })?;
            let mut result = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let wire_api: String = row
                    .get(0)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let finish_reason: String = row
                    .get(1)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                result.push(DistinctFinishReason {
                    wire_api,
                    finish_reason,
                });
            }
            Ok(result)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use ts_llm::wire_apis as wa;
    use ts_metrics::model::{LlmFinishMetric, LlmMetric};
    use ts_storage::query::*;
    use ts_storage::StorageBackend;

    fn in_memory() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
    }

    fn sample_metric() -> LlmMetric {
        LlmMetric {
            timestamp_us: 1_700_000_000_000_000,
            source_id: String::new(),
            granularity: "1m",
            wire_api: wa::OPENAI_CHAT.to_string(),
            model: "gpt-4".to_string(),
            server_ip: "10.0.0.2".to_string(),
            call_count: 42,
            stream_count: 30,
            non_stream_count: 12,
            // active calls avg 3.5 → sum 147 across 42 samples.
            active_calls_sum: 147,
            active_calls_sample_count: 42,
            active_calls_max: 8,
            total_input_tokens: 10000,
            input_token_count: 42,
            total_output_tokens: 5000,
            output_token_count: 42,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            error_count: 2,
            error_4xx_count: 1,
            error_429_count: 0,
            error_5xx_count: 1,
            // ttft_avg 150 × 42 = 6300.
            ttft_sum: 6300.0,
            ttft_count: 42,
            ttft_p50: Some(120.0),
            ttft_p95: Some(350.0),
            ttft_p99: Some(500.0),
            // e2e_avg 1200 × 42 = 50400.
            e2e_sum: 50_400.0,
            e2e_count: 42,
            e2e_p50: Some(1000.0),
            e2e_p95: Some(2500.0),
            e2e_p99: Some(4000.0),
            // tpot_avg 22.2 × 30 streaming = 666.
            tpot_sum: 666.0,
            tpot_count: 30,
            tpot_p50: Some(23.8),
            tpot_p95: Some(12.5),
            tpot_p99: Some(8.3),
        }
    }

    #[tokio::test]
    async fn test_write_metrics_single() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let metric = sample_metric();
        backend.write_metrics(vec![metric]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT granularity, model, call_count, ttft_p50 FROM llm_metrics")
            .unwrap();
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<f64>>(3)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, "1m");
        assert_eq!(row.1, "gpt-4");
        assert_eq!(row.2, 42);
        assert_eq!(row.3, Some(120.0));
    }

    #[tokio::test]
    async fn test_write_metrics_empty_batch() {
        let backend = in_memory();
        backend.init().await.unwrap();
        backend.write_metrics(vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn test_write_metrics_new_columns() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let metric = sample_metric();
        backend.write_metrics(vec![metric]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT total_output_tokens, output_token_count, tpot_sum, tpot_count \
                 FROM llm_metrics",
            )
            .unwrap();
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, 5000);
        assert_eq!(row.1, 42);
        // tpot_sum 666 / tpot_count 30 = 22.2
        assert!((row.2 - 666.0).abs() < 1e-6);
        assert_eq!(row.3, 30);
    }

    #[tokio::test]
    async fn test_write_finish_metrics_round_trip() {
        // Phase 4 long-format finish-reason table. Inserts mixed raw provider
        // values and verifies that the row count, key columns, and per-reason
        // counts round-trip without any normalization.
        let backend = in_memory();
        backend.init().await.unwrap();

        let base = LlmFinishMetric {
            timestamp_us: 1_700_000_000_000_000,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: wa::OPENAI_CHAT.to_string(),
            model: "gpt-4".to_string(),
            server_ip: "10.0.0.2".to_string(),
            finish_reason: String::new(),
            count: 0,
        };
        let rows = vec![
            LlmFinishMetric {
                finish_reason: "stop".into(),
                count: 35,
                ..base.clone()
            },
            LlmFinishMetric {
                finish_reason: "length".into(),
                count: 3,
                ..base.clone()
            },
            LlmFinishMetric {
                finish_reason: "tool_calls".into(),
                count: 2,
                ..base.clone()
            },
            // Unknown / future provider value preserved verbatim.
            LlmFinishMetric {
                finish_reason: "pause_turn".into(),
                count: 1,
                ..base
            },
        ];
        backend.write_finish_metrics(rows).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM llm_finish_metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 4);

        let stop_count: u64 = conn
            .query_row(
                "SELECT count FROM llm_finish_metrics WHERE finish_reason = 'stop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stop_count, 35);

        let pause_count: u64 = conn
            .query_row(
                "SELECT count FROM llm_finish_metrics WHERE finish_reason = 'pause_turn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pause_count, 1);
    }

    #[tokio::test]
    async fn query_finish_reasons_groups_by_raw_value() {
        // Phase 5: long-format read path. Two timestamps × three raw provider
        // finish_reason values, written at the (*, *, *) tier so the default
        // (no wire_api / no model filter) read picks them up.
        let backend = in_memory();
        backend.init().await.unwrap();

        let bucket_a: i64 = 1_700_000_000_000_000;
        let bucket_b: i64 = 1_700_000_060_000_000; // +60s, next 1m bucket
        let mk = |ts_us: i64, reason: &str, count: u64| LlmFinishMetric {
            timestamp_us: ts_us,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: "*".to_string(),
            model: "*".to_string(),
            server_ip: "*".to_string(),
            finish_reason: reason.to_string(),
            count,
        };

        backend
            .write_finish_metrics(vec![
                mk(bucket_a, "end_turn", 12),
                mk(bucket_a, "tool_use", 4),
                mk(bucket_a, "max_tokens", 1),
                mk(bucket_b, "end_turn", 7),
                mk(bucket_b, "pause_turn", 2),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: bucket_a - 1,
                end_us: bucket_b + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: Vec::new(),
            models: Vec::new(),
            server_ips: Vec::new(),
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        // One series per distinct raw value; alphabetical by finish_reason.
        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(
            names,
            vec!["end_turn", "max_tokens", "pause_turn", "tool_use"]
        );

        let end_turn = series
            .iter()
            .find(|s| s.finish_reason == "end_turn")
            .unwrap();
        assert_eq!(end_turn.points, vec![(bucket_a, 12), (bucket_b, 7)]);

        let pause_turn = series
            .iter()
            .find(|s| s.finish_reason == "pause_turn")
            .unwrap();
        assert_eq!(pause_turn.points, vec![(bucket_b, 2)]);

        let max_tokens = series
            .iter()
            .find(|s| s.finish_reason == "max_tokens")
            .unwrap();
        assert_eq!(max_tokens.points, vec![(bucket_a, 1)]);
    }

    #[tokio::test]
    async fn query_finish_reasons_filters_by_wire_api() {
        // With `wire_api = Some("openai_chat")` and no model filter, the read
        // sums per-model rows at the (W, M, *) tier.
        let backend = in_memory();
        backend.init().await.unwrap();

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

        backend
            .write_finish_metrics(vec![
                mk(wa::OPENAI_CHAT, "gpt-4", "stop", 5),
                mk(wa::OPENAI_CHAT, "gpt-4o", "stop", 2),
                mk(wa::OPENAI_CHAT, "gpt-4", "length", 1),
                mk(wa::ANTHROPIC, "claude-3", "end_turn", 9),
                // Fully-rolled-up tier for the same window — must be excluded
                // by the read (server_ip='*' AND wire_api filter).
                mk("*", "*", "stop", 99),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: ts - 1,
                end_us: ts + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: vec![wa::OPENAI_CHAT.to_string()],
            models: Vec::new(),
            server_ips: Vec::new(),
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        // Only openai_chat finish reasons; counts summed across models.
        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(names, vec!["length", "stop"]);
        let stop = series.iter().find(|s| s.finish_reason == "stop").unwrap();
        assert_eq!(stop.points, vec![(ts, 7)]); // 5 + 2
        let length = series.iter().find(|s| s.finish_reason == "length").unwrap();
        assert_eq!(length.points, vec![(ts, 1)]);
    }

    #[tokio::test]
    async fn query_finish_reasons_filters_by_multi_wire_api() {
        // With `wire_apis = ["openai_chat", "anthropic"]` (CSV expansion at the
        // API layer), the read sums per-model rows at the (W, M, *) tier across
        // all listed wire_apis — same finish_reason in different wire_apis
        // collapses into a single series.
        let backend = in_memory();
        backend.init().await.unwrap();

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

        backend
            .write_finish_metrics(vec![
                mk(wa::OPENAI_CHAT, "gpt-4", "stop", 5),
                mk(wa::OPENAI_CHAT, "gpt-4o", "stop", 2),
                mk(wa::ANTHROPIC, "claude-3", "stop", 3),
                mk(wa::ANTHROPIC, "claude-3", "end_turn", 9),
                // A wire_api outside the filter must NOT contribute.
                mk("gemini", "gemini-pro", "stop", 100),
                // Fully-rolled-up tier — must be excluded by server_ip='*' AND
                // the wire_api IN-list filter.
                mk("*", "*", "stop", 99),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: ts - 1,
                end_us: ts + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: vec![wa::OPENAI_CHAT.to_string(), wa::ANTHROPIC.to_string()],
            models: Vec::new(),
            server_ips: Vec::new(),
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(names, vec!["end_turn", "stop"]);
        // stop sums across both wire_apis and their models: 5 + 2 + 3 = 10.
        let stop = series.iter().find(|s| s.finish_reason == "stop").unwrap();
        assert_eq!(stop.points, vec![(ts, 10)]);
        let end_turn = series
            .iter()
            .find(|s| s.finish_reason == "end_turn")
            .unwrap();
        assert_eq!(end_turn.points, vec![(ts, 9)]);
    }

    #[tokio::test]
    async fn query_finish_reasons_filters_by_server_ip() {
        // With `server_ips = ["10.0.0.1"]` and no wire/model filter, the read
        // lands on the (*, *, S) tier and SUMs only the listed servers.
        let backend = in_memory();
        backend.init().await.unwrap();

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

        backend
            .write_finish_metrics(vec![
                mk("10.0.0.1", "end_turn", 5),
                mk("10.0.0.1", "tool_use", 2),
                mk("10.0.0.2", "end_turn", 7),
                // Cross-server rollup tier — must be excluded by the IN-list.
                mk("*", "end_turn", 99),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: ts - 1,
                end_us: ts + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: Vec::new(),
            models: Vec::new(),
            server_ips: vec!["10.0.0.1".to_string()],
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(names, vec!["end_turn", "tool_use"]);
        let end_turn = series
            .iter()
            .find(|s| s.finish_reason == "end_turn")
            .unwrap();
        assert_eq!(end_turn.points, vec![(ts, 5)]);
        let tool_use = series
            .iter()
            .find(|s| s.finish_reason == "tool_use")
            .unwrap();
        assert_eq!(tool_use.points, vec![(ts, 2)]);
    }

    #[tokio::test]
    async fn query_distinct_finish_reasons_returns_pairs() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts: i64 = 1_700_000_000_000_000;
        let mk = |wire: &str, reason: &str| LlmFinishMetric {
            timestamp_us: ts,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: wire.to_string(),
            model: "m".to_string(),
            server_ip: "*".to_string(),
            finish_reason: reason.to_string(),
            count: 1,
        };

        backend
            .write_finish_metrics(vec![
                mk(wa::ANTHROPIC, "end_turn"),
                mk(wa::ANTHROPIC, "tool_use"),
                mk(wa::ANTHROPIC, "end_turn"), // duplicate — DISTINCT collapses
                mk(wa::OPENAI_CHAT, "stop"),
                mk(wa::OPENAI_CHAT, "tool_calls"),
                // Cross-wire-api rollup tier — must be excluded.
                mk("*", "stop"),
            ])
            .await
            .unwrap();

        let pairs = backend.query_distinct_finish_reasons().await.unwrap();
        let as_tuples: Vec<(&str, &str)> = pairs
            .iter()
            .map(|p| (p.wire_api.as_str(), p.finish_reason.as_str()))
            .collect();
        // Sorted by (wire_api, finish_reason) ascending — alphabetical so
        // anthropic comes before openai-chat.
        assert_eq!(
            as_tuples,
            vec![
                (wa::ANTHROPIC, "end_turn"),
                (wa::ANTHROPIC, "tool_use"),
                (wa::OPENAI_CHAT, "stop"),
                (wa::OPENAI_CHAT, "tool_calls"),
            ]
        );
    }

    #[tokio::test]
    async fn test_query_metrics_timeseries_basic() {
        let backend = in_memory();
        backend.init().await.unwrap();

        // Two global wildcard metrics at different timestamps
        let mut m1 = sample_metric();
        m1.timestamp_us = 1_700_000_000_000_000;
        m1.granularity = "1m";
        m1.wire_api = "*".to_string();
        m1.model = "*".to_string();
        m1.server_ip = "*".to_string();
        m1.ttft_p50 = Some(100.0);
        m1.ttft_p95 = Some(200.0);

        let mut m2 = sample_metric();
        m2.timestamp_us = 1_700_000_060_000_000; // +60s
        m2.granularity = "1m";
        m2.wire_api = "*".to_string();
        m2.model = "*".to_string();
        m2.server_ip = "*".to_string();
        m2.ttft_p50 = Some(150.0);
        m2.ttft_p95 = Some(300.0);

        backend.write_metrics(vec![m1, m2]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: 1_700_000_000_000_000,
                end_us: 1_700_000_120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["ttft_p50".to_string(), "ttft_p95".to_string()],
            group_by: None,
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].group.is_none());
        assert_eq!(rows[0].values[0], Some(100.0));
        assert_eq!(rows[0].values[1], Some(200.0));
        assert_eq!(rows[1].values[0], Some(150.0));
        assert_eq!(rows[1].values[1], Some(300.0));
    }

    #[tokio::test]
    async fn test_query_metrics_timeseries_group_by_wire_api() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        // Per-model rows: (wire_api, model, server_ip='*')
        // These are what the aggregator actually produces. group_by=wire_api
        // should SUM across models within each wire_api.
        let mut m = sample_metric();
        m.timestamp_us = ts;
        m.granularity = "1m";
        m.server_ip = "*".to_string();

        m.wire_api = wa::OPENAI_CHAT.to_string();
        m.model = "gpt-4".to_string();
        m.call_count = 200;
        backend.write_metrics(vec![m.clone()]).await.unwrap();

        m.model = "gpt-3.5".to_string();
        m.call_count = 100;
        backend.write_metrics(vec![m.clone()]).await.unwrap();

        m.wire_api = wa::ANTHROPIC.to_string();
        m.model = "claude-3".to_string();
        m.call_count = 50;
        backend.write_metrics(vec![m]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["call_count".to_string()],
            group_by: Some("wire_api".to_string()),
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        // Should have 2 rows: anthropic and openai (aggregated across models)
        assert_eq!(rows.len(), 2);
        let anthropic_row = rows
            .iter()
            .find(|r| r.group.as_deref() == Some(wa::ANTHROPIC))
            .unwrap();
        let openai_row = rows
            .iter()
            .find(|r| r.group.as_deref() == Some(wa::OPENAI_CHAT))
            .unwrap();
        assert_eq!(anthropic_row.values[0], Some(50.0));
        assert_eq!(openai_row.values[0], Some(300.0)); // 200 + 100
    }

    #[tokio::test]
    async fn test_multi_source_ungrouped_timeseries_merges() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut source0 = sample_metric();
        source0.timestamp_us = ts;
        source0.source_id = "s0".into();
        source0.granularity = "1m";
        source0.wire_api = "*".into();
        source0.model = "*".into();
        source0.server_ip = "*".into();
        source0.call_count = 10;
        source0.ttft_count = 10;
        source0.ttft_p50 = Some(100.0);
        source0.error_count = 1;

        let mut source1 = sample_metric();
        source1.timestamp_us = ts;
        source1.source_id = "s1".into();
        source1.granularity = "1m";
        source1.wire_api = "*".into();
        source1.model = "*".into();
        source1.server_ip = "*".into();
        source1.call_count = 30;
        source1.ttft_count = 30;
        source1.ttft_p50 = Some(200.0);
        source1.error_count = 3;

        backend.write_metrics(vec![source0, source1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec![
                "call_count".to_string(),
                "ttft_p50".to_string(),
                "error_count".to_string(),
            ],
            group_by: None,
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "ungrouped query must return 1 row per timestamp across sources, got {}",
            rows.len()
        );
        assert_eq!(rows[0].values[0], Some(40.0), "call_count SUM = 10 + 30");
        // weighted avg by ttft_count: (100*10 + 200*30) / 40 = 175
        let p50 = rows[0].values[1].unwrap();
        assert!((p50 - 175.0).abs() < 0.01, "weighted p50 ≈ 175, got {p50}");
        assert_eq!(rows[0].values[2], Some(4.0), "error_count SUM = 1 + 3");
    }

    #[tokio::test]
    async fn test_active_calls_max_uses_max_across_sources() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut s0 = sample_metric();
        s0.timestamp_us = ts;
        s0.source_id = "s0".into();
        s0.granularity = "1m";
        s0.wire_api = "*".into();
        s0.model = "*".into();
        s0.server_ip = "*".into();
        s0.active_calls_max = 5;

        let mut s1 = sample_metric();
        s1.timestamp_us = ts;
        s1.source_id = "s1".into();
        s1.granularity = "1m";
        s1.wire_api = "*".into();
        s1.model = "*".into();
        s1.server_ip = "*".into();
        s1.active_calls_max = 7;

        backend.write_metrics(vec![s0, s1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["active_calls_max".to_string()],
            group_by: None,
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values[0],
            Some(7.0),
            "active_calls_max must MAX(5, 7) = 7, not SUM = 12"
        );
    }

    #[tokio::test]
    async fn test_multi_source_grouped_timeseries_merges() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut s0 = sample_metric();
        s0.timestamp_us = ts;
        s0.source_id = "s0".into();
        s0.granularity = "1m";
        s0.wire_api = wa::OPENAI_CHAT.into();
        s0.model = "gpt-4".into();
        s0.server_ip = "*".into();
        s0.call_count = 10;

        let mut s1 = sample_metric();
        s1.timestamp_us = ts;
        s1.source_id = "s1".into();
        s1.granularity = "1m";
        s1.wire_api = wa::OPENAI_CHAT.into();
        s1.model = "gpt-4".into();
        s1.server_ip = "*".into();
        s1.call_count = 40;

        backend.write_metrics(vec![s0, s1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["call_count".to_string()],
            group_by: Some("wire_api".to_string()),
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group.as_deref(), Some(wa::OPENAI_CHAT));
        assert_eq!(rows[0].values[0], Some(50.0), "grouped SUM across sources");
    }

    #[tokio::test]
    async fn test_query_metrics_summary() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts1 = 1_700_000_000_000_000i64;
        let ts2 = ts1 + 10_000_000; // +10s

        let mut m1 = sample_metric();
        m1.timestamp_us = ts1;
        m1.granularity = "10s";
        m1.wire_api = "*".to_string();
        m1.model = "*".to_string();
        m1.server_ip = "*".to_string();
        m1.call_count = 100;
        m1.stream_count = 80;
        m1.error_count = 5;
        m1.error_4xx_count = 3;
        m1.error_429_count = 1;
        m1.error_5xx_count = 2;
        m1.total_input_tokens = 10_000;
        m1.total_output_tokens = 5_000;
        // ttft avg 100 over 100 samples → sum 10_000
        m1.ttft_sum = 10_000.0;
        m1.ttft_count = 100;
        m1.e2e_sum = 50_000.0;
        m1.e2e_count = 100;
        // tpot avg 40 over 80 streaming samples → sum 3200
        m1.tpot_sum = 3_200.0;
        m1.tpot_count = 80;

        let mut m2 = sample_metric();
        m2.timestamp_us = ts2;
        m2.granularity = "10s";
        m2.wire_api = "*".to_string();
        m2.model = "*".to_string();
        m2.server_ip = "*".to_string();
        m2.call_count = 200;
        m2.stream_count = 160;
        m2.error_count = 10;
        m2.error_4xx_count = 6;
        m2.error_429_count = 2;
        m2.error_5xx_count = 4;
        m2.total_input_tokens = 20_000;
        m2.total_output_tokens = 10_000;
        // ttft avg 200 over 200 samples → sum 40_000
        m2.ttft_sum = 40_000.0;
        m2.ttft_count = 200;
        m2.e2e_sum = 200_000.0;
        m2.e2e_count = 200;
        // tpot avg 60 over 160 streaming samples → sum 9600
        m2.tpot_sum = 9_600.0;
        m2.tpot_count = 160;

        backend.write_metrics(vec![m1, m2]).await.unwrap();

        let query = MetricsSummaryQuery {
            time_range: TimeRange {
                start_us: ts1,
                end_us: ts2 + 10_000_000,
            },
            filter: DimensionFilter::default(),
        };

        let summary = backend.query_metrics_summary(&query).await.unwrap();
        assert_eq!(summary.call_count, 300);
        assert_eq!(summary.error_count, 15);
        assert_eq!(summary.error_4xx_count, 9);
        assert_eq!(summary.error_429_count, 3);
        assert_eq!(summary.error_5xx_count, 6);
        assert_eq!(summary.total_input_tokens, 30_000);
        assert_eq!(summary.total_output_tokens, 15_000);
        // Exact avg via sum+count: (10000 + 40000) / 300 = 166.666...
        let ttft_avg = summary.ttft_avg.unwrap();
        assert!(
            (ttft_avg - 500.0 / 3.0).abs() < 0.01,
            "expected ~166.67, got {ttft_avg}"
        );
        // tpot exact avg: (3200 + 9600) / 240 = 53.33
        let tpot_avg = summary.tpot_avg.unwrap();
        assert!(
            (tpot_avg - 160.0 / 3.0).abs() < 0.01,
            "expected ~53.33, got {tpot_avg}"
        );
    }

    #[tokio::test]
    async fn test_query_metrics_models() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut m_gpt4 = sample_metric();
        m_gpt4.timestamp_us = ts;
        m_gpt4.granularity = "10s";
        m_gpt4.wire_api = wa::OPENAI_CHAT.to_string();
        m_gpt4.model = "gpt-4".to_string();
        m_gpt4.server_ip = "*".to_string();
        m_gpt4.call_count = 100;
        m_gpt4.stream_count = 80;
        // ttft avg 150 over 100 → sum 15000
        m_gpt4.ttft_sum = 15_000.0;
        m_gpt4.ttft_count = 100;
        m_gpt4.ttft_p95 = Some(400.0);
        m_gpt4.e2e_sum = 100_000.0;
        m_gpt4.e2e_count = 100;
        m_gpt4.e2e_p95 = Some(3000.0);
        // tpot avg 20 over 80 → sum 1600
        m_gpt4.tpot_sum = 1_600.0;
        m_gpt4.tpot_count = 80;

        let mut m_claude = sample_metric();
        m_claude.timestamp_us = ts;
        m_claude.granularity = "10s";
        m_claude.wire_api = wa::ANTHROPIC.to_string();
        m_claude.model = "claude-3".to_string();
        m_claude.server_ip = "*".to_string();
        m_claude.call_count = 200;
        m_claude.stream_count = 150;
        // ttft avg 120 over 200 → sum 24000
        m_claude.ttft_sum = 24_000.0;
        m_claude.ttft_count = 200;
        m_claude.ttft_p95 = Some(300.0);
        m_claude.e2e_sum = 160_000.0;
        m_claude.e2e_count = 200;
        m_claude.e2e_p95 = Some(2000.0);
        // tpot avg 22 over 150 → sum 3300
        m_claude.tpot_sum = 3_300.0;
        m_claude.tpot_count = 150;

        backend.write_metrics(vec![m_gpt4, m_claude]).await.unwrap();

        let query = MetricsModelsQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 10_000_000,
            },
            filter: DimensionFilter::default(),
            sort_by: "call_count".to_string(),
            sort_order: "DESC".to_string(),
            limit: 10,
        };

        let rows = backend.query_metrics_models(&query).await.unwrap();
        assert_eq!(rows.len(), 2);
        // claude-3 should come first (200 > 100)
        assert_eq!(rows[0].wire_api, wa::ANTHROPIC);
        assert_eq!(rows[0].model, "claude-3");
        assert_eq!(rows[0].call_count, 200);
        assert_eq!(rows[1].wire_api, wa::OPENAI_CHAT);
        assert_eq!(rows[1].model, "gpt-4");
        assert_eq!(rows[1].call_count, 100);
    }

    #[tokio::test]
    async fn test_query_metrics_summary_wire_api_filter() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        // (W, M, *) tier rows — two wire_apis.
        let mut openai_row = sample_metric();
        openai_row.timestamp_us = ts;
        openai_row.granularity = "10s";
        openai_row.wire_api = wa::OPENAI_CHAT.into();
        openai_row.model = "gpt-4".into();
        openai_row.server_ip = "*".into();
        openai_row.call_count = 100;

        let mut anthropic_row = sample_metric();
        anthropic_row.timestamp_us = ts;
        anthropic_row.granularity = "10s";
        anthropic_row.wire_api = wa::ANTHROPIC.into();
        anthropic_row.model = "claude-3".into();
        anthropic_row.server_ip = "*".into();
        anthropic_row.call_count = 200;

        // (*, *, *) tier row — must NOT be counted when a wire_api filter is
        // applied (otherwise we'd double-count).
        let mut total_row = sample_metric();
        total_row.timestamp_us = ts;
        total_row.granularity = "10s";
        total_row.wire_api = "*".into();
        total_row.model = "*".into();
        total_row.server_ip = "*".into();
        total_row.call_count = 300;

        backend
            .write_metrics(vec![openai_row, anthropic_row, total_row])
            .await
            .unwrap();

        let query = MetricsSummaryQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 10_000_000,
            },
            filter: DimensionFilter {
                wire_apis: vec![wa::OPENAI_CHAT.into()],
                ..Default::default()
            },
        };
        let summary = backend.query_metrics_summary(&query).await.unwrap();
        assert_eq!(
            summary.call_count, 100,
            "filter should return only the openai row"
        );
    }

    #[tokio::test]
    async fn test_query_metrics_models_wire_api_filter() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut gpt4 = sample_metric();
        gpt4.timestamp_us = ts;
        gpt4.granularity = "10s";
        gpt4.wire_api = wa::OPENAI_CHAT.into();
        gpt4.model = "gpt-4".into();
        gpt4.server_ip = "*".into();
        gpt4.call_count = 100;

        let mut claude = sample_metric();
        claude.timestamp_us = ts;
        claude.granularity = "10s";
        claude.wire_api = wa::ANTHROPIC.into();
        claude.model = "claude-3".into();
        claude.server_ip = "*".into();
        claude.call_count = 200;

        backend.write_metrics(vec![gpt4, claude]).await.unwrap();

        let query = MetricsModelsQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 10_000_000,
            },
            filter: DimensionFilter {
                wire_apis: vec![wa::OPENAI_CHAT.into()],
                ..Default::default()
            },
            sort_by: "call_count".into(),
            sort_order: "DESC".into(),
            limit: 10,
        };
        let rows = backend.query_metrics_models(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].wire_api, wa::OPENAI_CHAT);
        assert_eq!(rows[0].model, "gpt-4");
    }

    #[tokio::test]
    async fn test_query_metrics_timeseries_wire_api_filter_ungrouped() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        // (W, M, *) tier — two wire_apis worth of rows at the same timestamp.
        let mut gpt4 = sample_metric();
        gpt4.timestamp_us = ts;
        gpt4.granularity = "1m";
        gpt4.wire_api = wa::OPENAI_CHAT.into();
        gpt4.model = "gpt-4".into();
        gpt4.server_ip = "*".into();
        gpt4.call_count = 100;

        let mut claude = sample_metric();
        claude.timestamp_us = ts;
        claude.granularity = "1m";
        claude.wire_api = wa::ANTHROPIC.into();
        claude.model = "claude-3".into();
        claude.server_ip = "*".into();
        claude.call_count = 200;

        // (*, *, *) tier row must not be included alongside the filter.
        let mut total_row = sample_metric();
        total_row.timestamp_us = ts;
        total_row.granularity = "1m";
        total_row.wire_api = "*".into();
        total_row.model = "*".into();
        total_row.server_ip = "*".into();
        total_row.call_count = 300;

        backend
            .write_metrics(vec![gpt4, claude, total_row])
            .await
            .unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".into(),
            filter: DimensionFilter {
                wire_apis: vec![wa::OPENAI_CHAT.into()],
                ..Default::default()
            },
            fields: vec!["call_count".into()],
            group_by: None,
        };
        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Some(100.0));
    }

}
