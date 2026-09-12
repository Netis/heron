//! `llm_metrics` / `llm_finish_metrics` — write path and the aggregate reads.
//!
//! Metrics are the one entity where a duplicate row is not merely untidy: the
//! read path SUMs across rows, so a resend that lands twice inflates a chart
//! instead of showing a repeat. Three things guard that:
//!
//! 1. Metrics go out in their own small POST, well away from the multi-megabyte
//!    body batches, so they hit the failure paths that cause resends far less
//!    often in the first place.
//! 2. Each row carries a `row_id` minted **here**, before any retry, so the
//!    same logical row keeps the same id no matter how many times it is sent.
//!    That makes a duplicate detectable rather than invisible.
//! 3. `storage.aglake.metrics_dedup` turns on read-side deduplication for
//!    deployments that actually observe duplicates. It is off by default
//!    because `dedup` requires a full sort, and that is not a price to pay
//!    continuously against an event that may never happen.
//!
//! Granularity is part of the index name, not a field: aglake's retention is
//! per-index and Heron's metrics retention is per-granularity, so the two line
//! up exactly, and the most common filter becomes index-level pruning.
//!
//! # Derived values are computed in Rust
//!
//! The SQL backends express averages and weighted percentiles as SQL
//! (`if(sum(c) > 0, sum(s) / sum(c), NULL)`). Here the query pulls back only
//! the raw `sum`/`max` columns and the arithmetic happens in
//! [`FieldCalc::eval`]. That keeps the SPL to one `stats` with no conditional
//! expressions, and makes the zero-denominator case — which has to be `None`,
//! not `0.0`, because "no samples" and "zero latency" are different answers —
//! a plain Rust branch instead of a nested SQL conditional.

use std::collections::BTreeSet;

use h_common::error::{AppError, Result};
use h_metrics::model::{LlmFinishMetric, LlmMetric};
use h_storage::query::*;

use crate::client::Row;
use crate::dims;
use crate::rows::{finish_event, metric_event, Envelope, ST_FINISH, ST_METRIC};
use crate::spl::Search;
use crate::AglakeBackend;

impl AglakeBackend {
    /// Drop duplicate metric rows before they are summed, when configured to.
    ///
    /// This is the read half of the `row_id` scheme: writes are at-least-once,
    /// so a resend after a lost response can land the same logical row twice,
    /// and every read here is a `sum`. One extra row is not a cosmetic repeat
    /// in that arithmetic — it inflates the value. `row_id` is minted once per
    /// row before the first send and reused across every retry, which is what
    /// makes the second copy recognizable at all.
    ///
    /// Off by default because it is not free: `dedup` forces a sort and takes
    /// the query off the columnar fast path entirely (see [`crate::spl::Search::dedup`]).
    /// Paying that continuously to guard an event a deployment may never see
    /// is the wrong default; turning it on after observing duplicates is not.
    fn dedup_metrics(&self, s: &mut crate::spl::Search) {
        if self.metrics_dedup {
            s.dedup("row_id");
        }
    }

    pub(crate) async fn write_metrics(&self, metrics: Vec<LlmMetric>) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        let mut events = Vec::with_capacity(metrics.len());
        let mut unknown_granularity = 0usize;
        for m in &metrics {
            let Some(index) = self.ix.metrics_for(m.granularity) else {
                unknown_granularity += 1;
                continue;
            };
            let e = metric_event(m, uuid::Uuid::now_v7().to_string());
            match Envelope::new(m.timestamp_us, &m.source_id, ST_METRIC, index, e).encode() {
                Ok(s) => events.push(s),
                Err(err) => tracing::error!(
                    target: "aglake::write", error = %err,
                    "aglake: failed to encode metric event; skipping it"
                ),
            }
        }
        warn_unknown_granularity(unknown_granularity, "llm_metrics");
        self.hec.send(events).await
    }

    pub(crate) async fn write_finish_metrics(&self, metrics: Vec<LlmFinishMetric>) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        let mut events = Vec::with_capacity(metrics.len());
        let mut unknown_granularity = 0usize;
        for m in &metrics {
            let Some(index) = self.ix.finish_for(&m.granularity) else {
                unknown_granularity += 1;
                continue;
            };
            let e = finish_event(m, uuid::Uuid::now_v7().to_string());
            match Envelope::new(m.timestamp_us, &m.source_id, ST_FINISH, index, e).encode() {
                Ok(s) => events.push(s),
                Err(err) => tracing::error!(
                    target: "aglake::write", error = %err,
                    "aglake: failed to encode finish-metric event; skipping it"
                ),
            }
        }
        warn_unknown_granularity(unknown_granularity, "llm_finish_metrics");
        self.hec.send(events).await
    }

    pub(crate) async fn query_metrics_timeseries(
        &self,
        query: &MetricsTimeseriesQuery,
    ) -> Result<Vec<MetricsTimeseriesRow>> {
        let plan = FieldPlan::new(&query.fields)?;
        let group_by = match query.group_by.as_deref() {
            None => None,
            // Interpolated into the query as a field name — whitelist it.
            Some(g @ ("wire_api" | "model" | "server_ip")) => Some(g),
            Some(other) => return Err(AppError::Storage(format!("invalid group_by: {other}"))),
        };

        let Some(index) = self.ix.metrics_for(&query.granularity) else {
            return Err(AppError::Storage(format!(
                "unknown granularity: {}",
                query.granularity
            )));
        };
        let mut s = Search::new(index, ST_METRIC);
        dims::apply_for_group(&mut s, &query.filter, group_by);
        self.dedup_metrics(&mut s);
        plan.add_evals(&mut s);

        let by = match group_by {
            Some(g) => format!("ts_us, {g}"),
            None => "ts_us".to_string(),
        };
        let mut columns = vec!["ts_us".to_string()];
        if let Some(g) = group_by {
            columns.push(g.to_string());
        }
        columns.extend(plan.agg_names.iter().cloned());
        let spl = format!(
            "{} | stats {} by {by} | sort 0 +num(ts_us) | table {}",
            s.build(),
            plan.aggs.join(", "),
            columns.join(", ")
        );

        let (earliest, latest) = (
            crate::spl::epoch_secs(query.time_range.start_us),
            crate::spl::epoch_secs(query.time_range.end_us),
        );
        let rows = self.search.search(&spl, &earliest, &latest).await?.rows();

        Ok(rows
            .into_iter()
            .map(|r| MetricsTimeseriesRow {
                // The API's time grid is in seconds, matching the SQL
                // backends' `toUnixTimestamp` / `epoch` on this read.
                timestamp: num(&r, "ts_us").unwrap_or(0.0) as i64 / 1_000_000,
                group: group_by.and_then(|g| r.get(g).and_then(as_str)),
                values: plan.eval_all(&r),
            })
            .collect())
    }

    pub(crate) async fn query_metrics_summary(
        &self,
        query: &MetricsSummaryQuery,
    ) -> Result<MetricsSummaryRow> {
        // Fixed 10s granularity, matching both SQL backends: the summary is an
        // exact total, so it reads the finest tier rather than a rollup.
        let index = self
            .ix
            .metrics_for("10s")
            .ok_or_else(|| AppError::Storage("no 10s metrics index".into()))?;
        let mut s = Search::new(index, ST_METRIC);
        dims::apply(&mut s, &query.filter);
        self.dedup_metrics(&mut s);

        const COLS: &[&str] = &[
            "call_count",
            "error_count",
            "error_4xx_count",
            "error_429_count",
            "error_5xx_count",
            "total_input_tokens",
            "total_output_tokens",
            "ttft_sum",
            "ttft_count",
            "e2e_sum",
            "e2e_count",
            "tpot_sum",
            "tpot_count",
        ];
        let aggs: Vec<String> = COLS.iter().map(|c| format!("sum({c}) as {c}")).collect();
        let spl = format!(
            "{} | stats {} | table {}",
            s.build(),
            aggs.join(", "),
            COLS.join(", ")
        );
        let (earliest, latest) = (
            crate::spl::epoch_secs(query.time_range.start_us),
            crate::spl::epoch_secs(query.time_range.end_us),
        );
        let rows = self.search.search(&spl, &earliest, &latest).await?.rows();
        // No matching rows at all: `stats` with no `by` still emits one row of
        // zeros, but an empty index can yield none. Both mean "nothing here".
        let r = rows.into_iter().next().unwrap_or_default();

        Ok(MetricsSummaryRow {
            call_count: int(&r, "call_count"),
            error_count: int(&r, "error_count"),
            error_4xx_count: int(&r, "error_4xx_count"),
            error_429_count: int(&r, "error_429_count"),
            error_5xx_count: int(&r, "error_5xx_count"),
            total_input_tokens: int(&r, "total_input_tokens"),
            total_output_tokens: int(&r, "total_output_tokens"),
            ttft_avg: ratio(&r, "ttft_sum", "ttft_count"),
            e2e_avg: ratio(&r, "e2e_sum", "e2e_count"),
            tpot_avg: ratio(&r, "tpot_sum", "tpot_count"),
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

        let index = self
            .ix
            .metrics_for("10s")
            .ok_or_else(|| AppError::Storage("no 10s metrics index".into()))?;
        let mut s = Search::new(index, ST_METRIC);
        // Grouping by (wire_api, model) forces the detail tier for both, even
        // when neither is filtered.
        dims::apply_for_group(&mut s, &query.filter, Some("model"));
        self.dedup_metrics(&mut s);
        s.eval("w_ttft_p95", "ttft_p95 * ttft_count");
        s.eval("w_e2e_p95", "e2e_p95 * e2e_count");

        const COLS: &[&str] = &[
            "call_count",
            "error_count",
            "error_4xx_count",
            "error_429_count",
            "error_5xx_count",
            "total_input_tokens",
            "total_output_tokens",
            "ttft_sum",
            "ttft_count",
            "w_ttft_p95",
            "e2e_sum",
            "e2e_count",
            "w_e2e_p95",
            "tpot_sum",
            "tpot_count",
        ];
        let aggs: Vec<String> = COLS.iter().map(|c| format!("sum({c}) as {c}")).collect();
        let spl = format!(
            "{} | stats {} by wire_api, model | table wire_api, model, {}",
            s.build(),
            aggs.join(", "),
            COLS.join(", ")
        );
        let (earliest, latest) = (
            crate::spl::epoch_secs(query.time_range.start_us),
            crate::spl::epoch_secs(query.time_range.end_us),
        );
        let rows = self.search.search(&spl, &earliest, &latest).await?.rows();

        let mut items: Vec<MetricsModelRow> = rows
            .into_iter()
            .map(|r| MetricsModelRow {
                wire_api: r.get("wire_api").and_then(as_str).unwrap_or_default(),
                model: r.get("model").and_then(as_str).unwrap_or_default(),
                call_count: int(&r, "call_count"),
                error_count: int(&r, "error_count"),
                error_4xx_count: int(&r, "error_4xx_count"),
                error_429_count: int(&r, "error_429_count"),
                error_5xx_count: int(&r, "error_5xx_count"),
                total_input_tokens: int(&r, "total_input_tokens"),
                total_output_tokens: int(&r, "total_output_tokens"),
                ttft_avg: ratio(&r, "ttft_sum", "ttft_count"),
                ttft_p95: ratio(&r, "w_ttft_p95", "ttft_count"),
                e2e_avg: ratio(&r, "e2e_sum", "e2e_count"),
                e2e_p95: ratio(&r, "w_e2e_p95", "e2e_count"),
                tpot_avg: ratio(&r, "tpot_sum", "tpot_count"),
            })
            .collect();

        // Sorting happens here rather than in SPL: half the sort keys are
        // derived ratios that only exist after the Rust-side arithmetic, and
        // the row count is bounded by the model cardinality.
        let key = |r: &MetricsModelRow| -> f64 {
            match query.sort_by.as_str() {
                "call_count" => r.call_count as f64,
                "error_count" => r.error_count as f64,
                "total_input_tokens" => r.total_input_tokens as f64,
                "total_output_tokens" => r.total_output_tokens as f64,
                "ttft_avg" => r.ttft_avg.unwrap_or(f64::NEG_INFINITY),
                "ttft_p95" => r.ttft_p95.unwrap_or(f64::NEG_INFINITY),
                "e2e_avg" => r.e2e_avg.unwrap_or(f64::NEG_INFINITY),
                "e2e_p95" => r.e2e_p95.unwrap_or(f64::NEG_INFINITY),
                _ => r.tpot_avg.unwrap_or(f64::NEG_INFINITY),
            }
        };
        let asc = query.sort_order.eq_ignore_ascii_case("ASC");
        items.sort_by(|a, b| {
            let ord = key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal);
            let ord = if asc { ord } else { ord.reverse() };
            // Deterministic tie-break, which the SQL backends lack.
            ord.then_with(|| (&a.wire_api, &a.model).cmp(&(&b.wire_api, &b.model)))
        });
        items.truncate(query.limit as usize);
        Ok(items)
    }

    pub(crate) async fn query_finish_reasons(
        &self,
        query: &FinishReasonsQuery,
    ) -> Result<Vec<FinishReasonTimeseries>> {
        let Some(index) = self.ix.finish_for(&query.granularity) else {
            return Err(AppError::Storage(format!(
                "unknown granularity: {}",
                query.granularity
            )));
        };
        let mut s = Search::new(index, ST_FINISH);
        dims::apply_parts(
            &mut s,
            &query.wire_apis,
            &query.models,
            &query.server_ips,
            &[],
        );
        self.dedup_metrics(&mut s);
        let spl = format!(
            "{} | stats sum(count) as c by ts_us, finish_reason \
             | sort 0 +num(ts_us) | table ts_us, finish_reason, c",
            s.build()
        );
        let (earliest, latest) = (
            crate::spl::epoch_secs(query.time_range.start_us),
            crate::spl::epoch_secs(query.time_range.end_us),
        );
        let rows = self.search.search(&spl, &earliest, &latest).await?.rows();

        // Pivot `(ts, reason, count)` into one series per reason, preserving
        // the ascending timestamp order the query produced.
        let mut series: Vec<FinishReasonTimeseries> = Vec::new();
        for r in rows {
            let Some(reason) = r.get("finish_reason").and_then(as_str) else {
                continue;
            };
            let ts = num(&r, "ts_us").unwrap_or(0.0) as i64;
            let c = int(&r, "c");
            match series.iter_mut().find(|s| s.finish_reason == reason) {
                Some(s) => s.points.push((ts, c)),
                None => series.push(FinishReasonTimeseries {
                    finish_reason: reason,
                    points: vec![(ts, c)],
                }),
            }
        }
        Ok(series)
    }
}

// ---------------------------------------------------------------------------
// Field planning
// ---------------------------------------------------------------------------

/// Raw columns that are summed across rows.
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

/// Peaks, which must not be summed.
const MAX_FIELDS: &[&str] = &["active_calls_max"];

/// `(field, sum_column, count_column)` for the exact averages.
const AVG_PAIRS: &[(&str, &str, &str)] = &[
    (
        "active_calls_avg",
        "active_calls_sum",
        "active_calls_sample_count",
    ),
    (
        "input_tokens_avg",
        "total_input_tokens",
        "input_token_count",
    ),
    (
        "output_tokens_avg",
        "total_output_tokens",
        "output_token_count",
    ),
    ("ttft_avg", "ttft_sum", "ttft_count"),
    ("ttft_stream_avg", "ttft_stream_sum", "ttft_stream_count"),
    (
        "ttft_nonstream_avg",
        "ttft_nonstream_sum",
        "ttft_nonstream_count",
    ),
    ("e2e_avg", "e2e_sum", "e2e_count"),
    ("tpot_avg", "tpot_sum", "tpot_count"),
];

/// The count a per-row percentile is weighted by when averaged across rows.
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

/// How one requested field is produced from the aggregated columns.
#[derive(Debug, PartialEq)]
enum FieldCalc {
    Plain(String),
    /// `sum(numerator) / sum(denominator)`, or `None` when the denominator is
    /// zero — no samples is not the same answer as zero.
    Ratio(String, String),
}

impl FieldCalc {
    /// Resolve a requested field name. `None` means "not a metric field",
    /// which is what makes this the field whitelist — there is no separate
    /// list to drift out of sync with the arithmetic.
    fn for_field(f: &str) -> Option<Self> {
        if MAX_FIELDS.contains(&f) || SUM_FIELDS.contains(&f) {
            return Some(Self::Plain(f.to_string()));
        }
        if let Some((_, s, c)) = AVG_PAIRS.iter().find(|(name, _, _)| *name == f) {
            return Some(Self::Ratio(s.to_string(), c.to_string()));
        }
        if f.ends_with("_p50") || f.ends_with("_p95") || f.ends_with("_p99") {
            let w = percentile_weight(f);
            // Only the known distributions have percentiles; anything else
            // ending in _p95 is a typo, not a field.
            if percentile_prefix_known(f) {
                return Some(Self::Ratio(format!("w_{f}"), w.to_string()));
            }
        }
        None
    }

    fn eval(&self, r: &Row) -> Option<f64> {
        match self {
            Self::Plain(c) => num(r, c),
            Self::Ratio(n, d) => ratio(r, n, d),
        }
    }
}

fn percentile_prefix_known(f: &str) -> bool {
    ["ttft_stream", "ttft_nonstream", "ttft", "e2e", "tpot"]
        .iter()
        .any(|p| f.starts_with(p))
}

/// The aggregations a set of requested fields needs, plus how to turn the
/// aggregated row back into their values.
struct FieldPlan {
    calcs: Vec<FieldCalc>,
    /// `("w_ttft_p95", "ttft_p95 * ttft_count")` — products that must be
    /// formed per row, before the aggregation folds them.
    evals: Vec<(String, String)>,
    aggs: Vec<String>,
    agg_names: Vec<String>,
}

impl FieldPlan {
    fn new(fields: &[String]) -> Result<Self> {
        let mut calcs = Vec::with_capacity(fields.len());
        let mut needed: BTreeSet<String> = BTreeSet::new();
        let mut evals: Vec<(String, String)> = Vec::new();

        for f in fields {
            let calc = FieldCalc::for_field(f)
                .ok_or_else(|| AppError::Storage(format!("invalid metric field: {f}")))?;
            match &calc {
                FieldCalc::Plain(c) => {
                    needed.insert(c.clone());
                }
                FieldCalc::Ratio(n, d) => {
                    needed.insert(n.clone());
                    needed.insert(d.clone());
                    if let Some(base) = n.strip_prefix("w_") {
                        let expr = format!("{base} * {d}");
                        if !evals.iter().any(|(name, _)| name == n) {
                            evals.push((n.clone(), expr));
                        }
                    }
                }
            }
            calcs.push(calc);
        }

        let agg_names: Vec<String> = needed.iter().cloned().collect();
        let aggs = agg_names
            .iter()
            .map(|c| {
                if MAX_FIELDS.contains(&c.as_str()) {
                    format!("max({c}) as {c}")
                } else {
                    format!("sum({c}) as {c}")
                }
            })
            .collect();
        Ok(Self {
            calcs,
            evals,
            aggs,
            agg_names,
        })
    }

    fn add_evals(&self, s: &mut Search) {
        for (name, expr) in &self.evals {
            s.eval(name, expr);
        }
    }

    fn eval_all(&self, r: &Row) -> Vec<Option<f64>> {
        self.calcs.iter().map(|c| c.eval(r)).collect()
    }
}

// ---------------------------------------------------------------------------
// Row accessors
// ---------------------------------------------------------------------------

/// Read a numeric cell. `stats` output may arrive as a JSON number or, past
/// 2^53 or via a field extraction, as a string.
fn num(r: &Row, key: &str) -> Option<f64> {
    let v = r.get(key)?;
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

fn int(r: &Row, key: &str) -> u64 {
    num(r, key)
        .filter(|n| *n >= 0.0)
        .map(|n| n as u64)
        .unwrap_or(0)
}

/// `numerator / denominator`, or `None` when there were no samples.
fn ratio(r: &Row, numerator: &str, denominator: &str) -> Option<f64> {
    let d = num(r, denominator)?;
    if d > 0.0 {
        Some(num(r, numerator)? / d)
    } else {
        None
    }
}

fn as_str(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// A granularity with no index is a configuration mismatch, not a data
/// problem: the aggregator emitted a cadence this backend was never told
/// about. Dropping it silently would leave a hole in a chart with nothing to
/// explain it.
fn warn_unknown_granularity(count: usize, entity: &'static str) {
    if count > 0 {
        tracing::error!(
            target: "aglake::write",
            entity,
            dropped = count,
            "aglake: dropped metric row(s) whose granularity has no index. \
             The aggregator is emitting a cadence this backend does not know."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::fixtures;
    use crate::schema::Indexes;

    fn row(pairs: &[(&str, serde_json::Value)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// Every granularity the aggregator can emit must resolve to a distinct
    /// index; an unknown one must resolve to nothing rather than to a
    /// neighbour's index, which would blend two cadences into one series.
    #[test]
    fn granularity_routes_to_its_own_index() {
        let ix = Indexes::new("heron");
        assert_eq!(ix.metrics_for("10s"), Some("heron_metrics_10s"));
        assert_eq!(ix.metrics_for("1h"), Some("heron_metrics_1h"));
        assert_eq!(ix.finish_for("10s"), Some("heron_finish_10s"));
        assert_eq!(ix.metrics_for("nope"), None);
    }

    /// `row_id` is what makes a duplicated write detectable, so it has to be
    /// present and unique per row.
    #[test]
    fn each_metric_row_carries_a_distinct_row_id() {
        let m = fixtures::sample_metric();
        let a = metric_event(&m, uuid::Uuid::now_v7().to_string());
        let b = metric_event(&m, uuid::Uuid::now_v7().to_string());
        assert!(!a.row_id.is_empty());
        assert_ne!(a.row_id, b.row_id);
    }

    #[test]
    fn plan_sums_counts_maxes_peaks_and_dedups_shared_columns() {
        let p = FieldPlan::new(&[
            "call_count".into(),
            "active_calls_max".into(),
            "ttft_avg".into(),
            // Shares ttft_count with ttft_avg — must be requested once.
            "ttft_p95".into(),
        ])
        .unwrap();
        assert!(p
            .aggs
            .contains(&"sum(call_count) as call_count".to_string()));
        assert!(
            p.aggs
                .contains(&"max(active_calls_max) as active_calls_max".to_string()),
            "peaks must not be summed: {:?}",
            p.aggs
        );
        assert_eq!(
            p.agg_names.iter().filter(|c| *c == "ttft_count").count(),
            1,
            "shared column requested twice: {:?}",
            p.agg_names
        );
        // The weighted percentile needs its product formed before the fold.
        assert_eq!(
            p.evals,
            vec![(
                "w_ttft_p95".to_string(),
                "ttft_p95 * ttft_count".to_string()
            )]
        );
    }

    /// The field list *is* the whitelist — anything the arithmetic cannot
    /// produce has to be refused, since field names are spliced into the query.
    #[test]
    fn unknown_fields_are_rejected() {
        for bad in [
            "nope",
            "call_count; drop",
            "",
            "bogus_p95",
            "ttft",
            "sum(call_count)",
        ] {
            assert!(
                FieldPlan::new(&[bad.to_string()]).is_err(),
                "accepted invalid field {bad:?}"
            );
        }
        for good in ["call_count", "ttft_avg", "e2e_p99", "active_calls_max"] {
            assert!(
                FieldPlan::new(&[good.to_string()]).is_ok(),
                "rejected valid field {good:?}"
            );
        }
    }

    /// A ratio with no samples must be `None`. Returning 0.0 would draw a
    /// latency of zero on the chart instead of a gap.
    #[test]
    fn ratios_with_a_zero_denominator_are_none_not_zero() {
        let r = row(&[
            ("ttft_sum", serde_json::json!(0.0)),
            ("ttft_count", serde_json::json!(0)),
            ("e2e_sum", serde_json::json!(500.0)),
            ("e2e_count", serde_json::json!(4)),
        ]);
        assert_eq!(ratio(&r, "ttft_sum", "ttft_count"), None);
        assert_eq!(ratio(&r, "e2e_sum", "e2e_count"), Some(125.0));
        // A missing column is also "no samples", not zero.
        assert_eq!(ratio(&r, "tpot_sum", "tpot_count"), None);
    }

    /// Values may come back as JSON numbers or as strings; both have to read
    /// as the same number.
    #[test]
    fn numeric_cells_parse_from_either_json_form() {
        let r = row(&[
            ("a", serde_json::json!(42)),
            ("b", serde_json::json!("42")),
            ("c", serde_json::json!("9007199254740999")),
            ("d", serde_json::json!("not a number")),
        ]);
        assert_eq!(num(&r, "a"), Some(42.0));
        assert_eq!(num(&r, "b"), Some(42.0));
        assert_eq!(num(&r, "c"), Some(9007199254740999.0));
        assert_eq!(num(&r, "d"), None);
        assert_eq!(int(&r, "a"), 42);
        assert_eq!(int(&r, "missing"), 0);
    }

    /// The requested field order is the order of `values`, since the API zips
    /// them positionally against its `fields` list.
    #[test]
    fn values_come_back_in_the_requested_field_order() {
        let p =
            FieldPlan::new(&["e2e_avg".into(), "call_count".into(), "ttft_avg".into()]).unwrap();
        let r = row(&[
            ("call_count", serde_json::json!(7)),
            ("e2e_sum", serde_json::json!(300.0)),
            ("e2e_count", serde_json::json!(3)),
            ("ttft_sum", serde_json::json!(50.0)),
            ("ttft_count", serde_json::json!(5)),
        ]);
        assert_eq!(
            p.eval_all(&r),
            vec![Some(100.0), Some(7.0), Some(10.0)],
            "values must follow the requested order, not the aggregation order"
        );
    }

    /// Percentiles are averaged weighted by their sample count, so a row with
    /// many samples dominates one with few — the same thing the SQL backends
    /// express as `sum(p95 * count) / sum(count)`.
    #[test]
    fn percentiles_are_count_weighted() {
        let p = FieldPlan::new(&["ttft_p95".into()]).unwrap();
        // Two folded rows: p95=100 over 1 sample, p95=200 over 9 samples.
        // w_ttft_p95 = 100*1 + 200*9 = 1900; ttft_count = 10 → 190.
        let r = row(&[
            ("w_ttft_p95", serde_json::json!(1900.0)),
            ("ttft_count", serde_json::json!(10)),
        ]);
        assert_eq!(p.eval_all(&r), vec![Some(190.0)]);
    }
}
