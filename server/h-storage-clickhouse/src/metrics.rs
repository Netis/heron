//! `llm_metrics` + `llm_finish_metrics` table I/O — aggregation writes plus the
//! read-side time-series / summary / model-axis / finish-reason rollups and the
//! agent-distribution / activity aggregates over `agent_turns`.
//!
//! The dynamic-column time-series SELECT (the field count varies per request)
//! is expressed as a single `[expr, ...] AS vals` array projection of
//! `Nullable(Float64)`, so the row shape is fixed (`{ ts, [grp,] vals }`) and
//! maps cleanly to `MetricsTimeseriesRow.values: Vec<Option<f64>>`.

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::{AppError, Result};
use h_metrics::model::{LlmFinishMetric, LlmMetric};
use h_storage::dialect::{build_dimension_where, build_dimension_where_for_group, escape_clickhouse};
use h_storage::query::*;

use crate::client::{ch_err, insert_all};
use crate::rows::{FinishMetricRow, MetricRow};
use crate::sql::sql_in_list;
use crate::ClickHouseBackend;

/// All valid numeric metric field names accepted by `query_metrics_timeseries`.
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
    "ttft_avg",
    "ttft_sum",
    "ttft_count",
    "ttft_p50",
    "ttft_p95",
    "ttft_p99",
    "ttft_stream_avg",
    "ttft_stream_sum",
    "ttft_stream_count",
    "ttft_stream_p50",
    "ttft_stream_p95",
    "ttft_stream_p99",
    "ttft_nonstream_avg",
    "ttft_nonstream_sum",
    "ttft_nonstream_count",
    "ttft_nonstream_p50",
    "ttft_nonstream_p95",
    "ttft_nonstream_p99",
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
    "ttft_stream_sum",
    "ttft_stream_count",
    "ttft_nonstream_sum",
    "ttft_nonstream_count",
    "e2e_sum",
    "e2e_count",
    "tpot_sum",
    "tpot_count",
];

const MAX_FIELDS: &[&str] = &["active_calls_max"];

fn avg_pair(f: &str) -> Option<(&'static str, &'static str)> {
    match f {
        "active_calls_avg" => Some(("active_calls_sum", "active_calls_sample_count")),
        "input_tokens_avg" => Some(("total_input_tokens", "input_token_count")),
        "output_tokens_avg" => Some(("total_output_tokens", "output_token_count")),
        "ttft_avg" => Some(("ttft_sum", "ttft_count")),
        "ttft_stream_avg" => Some(("ttft_stream_sum", "ttft_stream_count")),
        "ttft_nonstream_avg" => Some(("ttft_nonstream_sum", "ttft_nonstream_count")),
        "e2e_avg" => Some(("e2e_sum", "e2e_count")),
        "tpot_avg" => Some(("tpot_sum", "tpot_count")),
        _ => None,
    }
}

fn percentile_weight(field: &str) -> &'static str {
    if field.starts_with("ttft_stream") {
        "ttft_stream_count"
    } else if field.starts_with("ttft_nonstream") {
        "ttft_nonstream_count"
    } else if field.starts_with("ttft") {
        "ttft_count"
    } else if field.starts_with("e2e") {
        "e2e_count"
    } else if field.starts_with("tpot") {
        "tpot_count"
    } else {
        "call_count"
    }
}

/// One per-field SQL expression, cast to `Nullable(Float64)` so all elements of
/// the `vals` array share one type. Mirrors the DuckDB `build_field_exprs`:
/// SUM for counts/totals, MAX for peaks, exact `SUM/SUM` ratio for `*_avg`, and
/// count-weighted average for per-row percentiles.
fn ch_field_expr(f: &str) -> String {
    let inner = if MAX_FIELDS.contains(&f) {
        format!("max({f})")
    } else if SUM_FIELDS.contains(&f) {
        format!("sum({f})")
    } else if let Some((sum_col, count_col)) = avg_pair(f) {
        format!("if(sum({count_col}) > 0, sum({sum_col}) / sum({count_col}), NULL)")
    } else if f.ends_with("_p50") || f.ends_with("_p95") || f.ends_with("_p99") {
        let weight = percentile_weight(f);
        format!("if(sum({weight}) > 0, sum({f} * {weight}) / sum({weight}), NULL)")
    } else {
        format!("sum({f})")
    };
    format!("CAST({inner} AS Nullable(Float64))")
}

/// Build the `[expr, ...] AS vals` array projection (Array(Nullable(Float64))).
fn build_vals_array(fields: &[String]) -> String {
    if fields.is_empty() {
        return "CAST([] AS Array(Nullable(Float64))) AS vals".to_string();
    }
    let exprs: Vec<String> = fields.iter().map(|f| ch_field_expr(f)).collect();
    format!("[{}] AS vals", exprs.join(", "))
}

/// Half-open timestamp predicate on the `DateTime64(6)` `timestamp` column.
fn ts_where(start_us: i64, end_us: i64) -> String {
    crate::sql::time_where("timestamp", start_us, end_us)
}

#[derive(Row, Deserialize)]
struct TsRow {
    ts: i64,
    vals: Vec<Option<f64>>,
}

#[derive(Row, Deserialize)]
struct TsGroupRow {
    ts: i64,
    grp: String,
    vals: Vec<Option<f64>>,
}

#[derive(Row, Deserialize)]
struct SummaryRow {
    call_count: u64,
    error_count: u64,
    error_4xx_count: u64,
    error_429_count: u64,
    error_5xx_count: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    ttft_avg: Option<f64>,
    e2e_avg: Option<f64>,
    tpot_avg: Option<f64>,
}

#[derive(Row, Deserialize)]
struct ModelRow {
    wire_api: String,
    model: String,
    call_count: u64,
    error_count: u64,
    error_4xx_count: u64,
    error_429_count: u64,
    error_5xx_count: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    ttft_avg: Option<f64>,
    ttft_p95: Option<f64>,
    e2e_avg: Option<f64>,
    e2e_p95: Option<f64>,
    tpot_avg: Option<f64>,
}

#[derive(Row, Deserialize)]
struct FinishRow {
    ts_us: i64,
    finish_reason: String,
    c: u64,
}

#[derive(Row, Deserialize)]
struct AgentSummaryRow {
    agent_kind: String,
    turn_count: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    avg_duration_ms: Option<f64>,
    last_seen_ms: i64,
}

#[derive(Row, Deserialize)]
struct AgentActivityRow {
    ts: i64,
    agent_kind: String,
    turn_count: u64,
}

impl ClickHouseBackend {
    pub(crate) async fn write_metrics(&self, metrics: Vec<LlmMetric>) -> Result<()> {
        let rows: Vec<MetricRow> = metrics.into_iter().map(MetricRow::from).collect();
        insert_all!(self.client, "llm_metrics", MetricRow, rows);
        Ok(())
    }

    pub(crate) async fn write_finish_metrics(
        &self,
        metrics: Vec<LlmFinishMetric>,
    ) -> Result<()> {
        let rows: Vec<FinishMetricRow> = metrics.into_iter().map(FinishMetricRow::from).collect();
        insert_all!(self.client, "llm_finish_metrics", FinishMetricRow, rows);
        Ok(())
    }

    pub(crate) async fn query_metrics_timeseries(
        &self,
        query: &MetricsTimeseriesQuery,
    ) -> Result<Vec<MetricsTimeseriesRow>> {
        for field in &query.fields {
            if !VALID_METRIC_FIELDS.contains(&field.as_str()) {
                return Err(AppError::Storage(format!("invalid metric field: {field}")));
            }
        }
        let vals = build_vals_array(&query.fields);
        let ts_pred = ts_where(query.time_range.start_us, query.time_range.end_us);
        let gran = crate::sql::escape_str(&query.granularity);

        if let Some(group_by) = query.group_by.as_deref() {
            // group_by is a column name interpolated into SQL — whitelist it.
            if !matches!(group_by, "wire_api" | "model" | "server_ip") {
                return Err(AppError::Storage(format!("invalid group_by: {group_by}")));
            }
            let dim_where = build_dimension_where_for_group(&query.filter, group_by, escape_clickhouse);
            let sql = format!(
                "SELECT toInt64(toUnixTimestamp(timestamp)) AS ts, {group_by} AS grp, {vals} \
                 FROM llm_metrics \
                 WHERE {dim_where} AND granularity = '{gran}' AND {ts_pred} \
                 GROUP BY timestamp, {group_by} \
                 ORDER BY timestamp, {group_by}"
            );
            let rows = self
                .client
                .query(&sql)
                .fetch_all::<TsGroupRow>()
                .await
                .map_err(|e| ch_err("query_metrics_timeseries (grouped)", e))?;
            Ok(rows
                .into_iter()
                .map(|r| MetricsTimeseriesRow {
                    timestamp: r.ts,
                    group: Some(r.grp),
                    values: r.vals,
                })
                .collect())
        } else {
            let dim_where = build_dimension_where(&query.filter, escape_clickhouse);
            let sql = format!(
                "SELECT toInt64(toUnixTimestamp(timestamp)) AS ts, {vals} \
                 FROM llm_metrics \
                 WHERE {dim_where} AND granularity = '{gran}' AND {ts_pred} \
                 GROUP BY timestamp \
                 ORDER BY timestamp"
            );
            let rows = self
                .client
                .query(&sql)
                .fetch_all::<TsRow>()
                .await
                .map_err(|e| ch_err("query_metrics_timeseries", e))?;
            Ok(rows
                .into_iter()
                .map(|r| MetricsTimeseriesRow {
                    timestamp: r.ts,
                    group: None,
                    values: r.vals,
                })
                .collect())
        }
    }

    pub(crate) async fn query_metrics_summary(
        &self,
        query: &MetricsSummaryQuery,
    ) -> Result<MetricsSummaryRow> {
        let dim_where = build_dimension_where(&query.filter, escape_clickhouse);
        let ts_pred = ts_where(query.time_range.start_us, query.time_range.end_us);
        let sql = format!(
            "SELECT \
                sum(call_count) AS call_count, \
                sum(error_count) AS error_count, \
                sum(error_4xx_count) AS error_4xx_count, \
                sum(error_429_count) AS error_429_count, \
                sum(error_5xx_count) AS error_5xx_count, \
                sum(total_input_tokens) AS total_input_tokens, \
                sum(total_output_tokens) AS total_output_tokens, \
                if(sum(ttft_count) > 0, sum(ttft_sum) / sum(ttft_count), NULL) AS ttft_avg, \
                if(sum(e2e_count) > 0, sum(e2e_sum) / sum(e2e_count), NULL) AS e2e_avg, \
                if(sum(tpot_count) > 0, sum(tpot_sum) / sum(tpot_count), NULL) AS tpot_avg \
             FROM llm_metrics \
             WHERE {dim_where} AND granularity = '10s' AND {ts_pred}"
        );
        let r = self
            .client
            .query(&sql)
            .fetch_one::<SummaryRow>()
            .await
            .map_err(|e| ch_err("query_metrics_summary", e))?;
        Ok(MetricsSummaryRow {
            call_count: r.call_count,
            error_count: r.error_count,
            error_4xx_count: r.error_4xx_count,
            error_429_count: r.error_429_count,
            error_5xx_count: r.error_5xx_count,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            ttft_avg: r.ttft_avg,
            e2e_avg: r.e2e_avg,
            tpot_avg: r.tpot_avg,
        })
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
        let sort_order = if query.sort_order.eq_ignore_ascii_case("ASC") {
            "ASC"
        } else {
            "DESC"
        };
        let dim_where = build_dimension_where_for_group(&query.filter, "wire_api", escape_clickhouse);
        let ts_pred = ts_where(query.time_range.start_us, query.time_range.end_us);
        let sql = format!(
            "SELECT * FROM ( \
                SELECT wire_api, model, \
                    sum(call_count) AS call_count, \
                    sum(error_count) AS error_count, \
                    sum(error_4xx_count) AS error_4xx_count, \
                    sum(error_429_count) AS error_429_count, \
                    sum(error_5xx_count) AS error_5xx_count, \
                    sum(total_input_tokens) AS total_input_tokens, \
                    sum(total_output_tokens) AS total_output_tokens, \
                    if(sum(ttft_count) > 0, sum(ttft_sum) / sum(ttft_count), NULL) AS ttft_avg, \
                    if(sum(ttft_count) > 0, sum(ttft_p95 * ttft_count) / sum(ttft_count), NULL) AS ttft_p95, \
                    if(sum(e2e_count) > 0, sum(e2e_sum) / sum(e2e_count), NULL) AS e2e_avg, \
                    if(sum(e2e_count) > 0, sum(e2e_p95 * e2e_count) / sum(e2e_count), NULL) AS e2e_p95, \
                    if(sum(tpot_count) > 0, sum(tpot_sum) / sum(tpot_count), NULL) AS tpot_avg \
                FROM llm_metrics \
                WHERE {dim_where} AND granularity = '10s' AND {ts_pred} \
                GROUP BY wire_api, model \
             ) sub \
             ORDER BY {} {sort_order} LIMIT {}",
            query.sort_by, query.limit,
        );
        let rows = self
            .client
            .query(&sql)
            .fetch_all::<ModelRow>()
            .await
            .map_err(|e| ch_err("query_metrics_models", e))?;
        Ok(rows
            .into_iter()
            .map(|r| MetricsModelRow {
                wire_api: r.wire_api,
                model: r.model,
                call_count: r.call_count,
                error_count: r.error_count,
                error_4xx_count: r.error_4xx_count,
                error_429_count: r.error_429_count,
                error_5xx_count: r.error_5xx_count,
                total_input_tokens: r.total_input_tokens,
                total_output_tokens: r.total_output_tokens,
                ttft_avg: r.ttft_avg,
                ttft_p95: r.ttft_p95,
                e2e_avg: r.e2e_avg,
                e2e_p95: r.e2e_p95,
                tpot_avg: r.tpot_avg,
            })
            .collect())
    }

    pub(crate) async fn query_finish_reasons(
        &self,
        query: &FinishReasonsQuery,
    ) -> Result<Vec<FinishReasonTimeseries>> {
        // Pick the matching pre-aggregated dimension tier (same logic as DuckDB).
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
        let server_clause = if has_server {
            format!("server_ip IN ({})", sql_in_list(&query.server_ips))
        } else {
            "server_ip = '*'".to_string()
        };
        let gran = crate::sql::escape_str(&query.granularity);
        let ts_pred = ts_where(query.time_range.start_us, query.time_range.end_us);
        let sql = format!(
            "SELECT toUnixTimestamp64Micro(timestamp) AS ts_us, finish_reason, sum(count) AS c \
             FROM llm_finish_metrics \
             WHERE granularity = '{gran}' AND {ts_pred} \
               AND {wire_clause} AND {model_clause} AND {server_clause} \
             GROUP BY ts_us, finish_reason \
             ORDER BY finish_reason ASC, ts_us ASC"
        );
        let rows = self
            .client
            .query(&sql)
            .fetch_all::<FinishRow>()
            .await
            .map_err(|e| ch_err("query_finish_reasons", e))?;

        let mut out: Vec<FinishReasonTimeseries> = Vec::new();
        for r in rows {
            match out.last_mut() {
                Some(last) if last.finish_reason == r.finish_reason => {
                    last.points.push((r.ts_us, r.c));
                }
                _ => out.push(FinishReasonTimeseries {
                    finish_reason: r.finish_reason,
                    points: vec![(r.ts_us, r.c)],
                }),
            }
        }
        Ok(out)
    }

    pub(crate) async fn query_agent_summary(
        &self,
        query: &AgentSummaryQuery,
    ) -> Result<Vec<AgentKindSummary>> {
        let ts_pred = crate::sql::time_where(
            "start_time",
            query.time_range.start_us,
            query.time_range.end_us,
        );
        let sql = format!(
            "SELECT agent_kind, \
                count() AS turn_count, \
                sum(total_input_tokens) AS total_input_tokens, \
                sum(total_output_tokens) AS total_output_tokens, \
                toNullable(avg(duration_ms)) AS avg_duration_ms, \
                toUnixTimestamp64Milli(max(start_time)) AS last_seen_ms \
             FROM agent_turns FINAL \
             WHERE {ts_pred} \
             GROUP BY agent_kind \
             ORDER BY turn_count DESC"
        );
        let rows = self
            .client
            .query(&sql)
            .fetch_all::<AgentSummaryRow>()
            .await
            .map_err(|e| ch_err("query_agent_summary", e))?;
        Ok(rows
            .into_iter()
            .map(|r| AgentKindSummary {
                agent_kind: r.agent_kind,
                turn_count: r.turn_count,
                total_input_tokens: r.total_input_tokens,
                total_output_tokens: r.total_output_tokens,
                avg_duration_ms: r.avg_duration_ms,
                last_seen_ms: r.last_seen_ms,
            })
            .collect())
    }

    pub(crate) async fn query_agent_activity(
        &self,
        query: &AgentActivityQuery,
    ) -> Result<Vec<AgentActivityPoint>> {
        let window_secs =
            ((query.time_range.end_us - query.time_range.start_us) / 1_000_000).max(60);
        let bucket = query.bucket_seconds.unwrap_or_else(|| {
            let target = (window_secs / 120).max(60) as u32;
            for &nice in &[60u32, 300, 600, 1800, 3600, 7200, 14400, 86400] {
                if target <= nice {
                    return nice;
                }
            }
            86400
        });
        let ts_pred = crate::sql::time_where(
            "start_time",
            query.time_range.start_us,
            query.time_range.end_us,
        );
        let sql = format!(
            // toStartOfInterval returns DateTime (not DateTime64), so derive ms
            // via seconds*1000 (bucket is second-aligned). toInt64 avoids
            // UInt32 overflow on the multiply.
            "SELECT \
                toInt64(toUnixTimestamp(toStartOfInterval(start_time, INTERVAL {bucket} SECOND))) * 1000 AS ts, \
                agent_kind, \
                count() AS turn_count \
             FROM agent_turns FINAL \
             WHERE {ts_pred} \
             GROUP BY ts, agent_kind \
             ORDER BY ts ASC, agent_kind ASC"
        );
        let rows = self
            .client
            .query(&sql)
            .fetch_all::<AgentActivityRow>()
            .await
            .map_err(|e| ch_err("query_agent_activity", e))?;
        Ok(rows
            .into_iter()
            .map(|r| AgentActivityPoint {
                timestamp_ms: r.ts,
                agent_kind: r.agent_kind,
                turn_count: r.turn_count,
            })
            .collect())
    }
}
