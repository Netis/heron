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
