//! Retention sweeper — deletes rows older than per-table cutoffs.
//!
//! Ports `h-storage-duckdb`'s `apply_retention` to ClickHouse. ClickHouse 26.x
//! supports lightweight `DELETE FROM tbl WHERE <predicate>` (not legacy
//! `ALTER TABLE ... DELETE` mutations), so each per-table sweep is a single
//! `DELETE` issued via `self.exec`.
//!
//! Differences from the DuckDB port:
//!   * No `spawn_blocking` / writer-mutex set — the `clickhouse` crate's
//!     `Client` is an async HTTP connection pool, so deletes are plain `await`.
//!   * ClickHouse `DELETE` returns no affected-row count, so each table's
//!     report count is obtained from a `SELECT count()` run with the *same*
//!     predicate immediately before the delete. This is best-effort: a
//!     concurrent insert/delete between the count and the delete can make the
//!     reported number drift slightly from the rows actually removed.
//!   * DuckDB's post-sweep `CHECKPOINT` (which shrinks the MVCC file) has no
//!     ClickHouse analogue; when `optimize_on_sweep` is set we instead run
//!     `OPTIMIZE TABLE <tbl> FINAL` per swept table to merge away the
//!     deleted-row tombstones. Default is off, matching config.
//!
//! Cutoff math mirrors DuckDB's `timestamp_value`: each `SystemTime` cutoff is
//! converted to epoch microseconds (i64) and compared via
//! `<col> < fromUnixTimestamp64Micro(<us>)`, keeping the MergeTree primary-key
//! index on the timestamp column usable (same convention as `sql::time_where`).

use std::time::SystemTime;

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::{AppError, Result};
use h_storage::retention::{RetentionPolicy, RetentionReport};

use crate::client::ch_err;
use crate::ClickHouseBackend;

/// `SELECT count()` result shape for the best-effort pre-delete row count.
#[derive(Row, Deserialize)]
struct CountRow {
    n: u64,
}

/// Convert a `SystemTime` cutoff into epoch microseconds (i64), mirroring the
/// DuckDB backend's `timestamp_value` math but emitting the raw micros for
/// interpolation into `fromUnixTimestamp64Micro(...)`.
fn cutoff_micros(t: SystemTime) -> Result<i64> {
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| AppError::Storage(format!("retention cutoff before UNIX epoch: {e}")))?;
    i64::try_from(dur.as_micros())
        .map_err(|_| AppError::Storage("retention cutoff out of i64 range".to_string()))
}

impl ClickHouseBackend {
    /// Count the rows that match `predicate` on `table` (best-effort — see the
    /// module docs). Used to populate the `RetentionReport` since ClickHouse
    /// `DELETE` reports no affected-row count.
    async fn count_where(&self, table: &str, predicate: &str) -> Result<u64> {
        let sql = format!("SELECT count() AS n FROM {table} WHERE {predicate}");
        let n = self
            .client
            .query(&sql)
            .fetch_one::<CountRow>()
            .await
            .map_err(|e| ch_err(&format!("retention count {table}"), e))?
            .n;
        Ok(n)
    }

    pub(crate) async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        let mut report = RetentionReport::default();
        // Tables touched this sweep — used to drive the optional OPTIMIZE FINAL.
        let mut swept: Vec<&'static str> = Vec::new();

        // spans — keyed on request_time.
        if let Some(cutoff) = policy.spans_before {
            let us = cutoff_micros(cutoff)?;
            let predicate = format!("request_time < fromUnixTimestamp64Micro({us})");
            report.spans_deleted = self.count_where("spans", &predicate).await?;
            self.exec(&format!("DELETE FROM spans WHERE {predicate}"))
                .await?;
            swept.push("spans");
        }

        // http_exchanges — keyed on request_time.
        if let Some(cutoff) = policy.http_exchanges_before {
            let us = cutoff_micros(cutoff)?;
            let predicate = format!("request_time < fromUnixTimestamp64Micro({us})");
            report.http_exchanges_deleted = self.count_where("http_exchanges", &predicate).await?;
            self.exec(&format!("DELETE FROM http_exchanges WHERE {predicate}"))
                .await?;
            swept.push("http_exchanges");
        }

        // traces — keyed on end_time.
        if let Some(cutoff) = policy.traces_before {
            let us = cutoff_micros(cutoff)?;
            let predicate = format!("end_time < fromUnixTimestamp64Micro({us})");
            report.traces_deleted = self.count_where("traces", &predicate).await?;
            self.exec(&format!("DELETE FROM traces WHERE {predicate}"))
                .await?;
            swept.push("traces");
        }

        // Per-granularity metrics sweep. For each (label, cutoff) pair, delete
        // from llm_metrics and — in lock-step, same (granularity, timestamp)
        // cutoff — from the long-format llm_finish_metrics table, mirroring the
        // DuckDB backend. Only the llm_metrics count is recorded in the report
        // (the DuckDB report likewise tracks only llm_metrics per granularity).
        let mut metrics_swept = false;
        for (label, cutoff) in &policy.metrics_before {
            let us = cutoff_micros(*cutoff)?;
            let label_lit = label.replace('\'', "''");
            let predicate =
                format!("granularity = '{label_lit}' AND timestamp < fromUnixTimestamp64Micro({us})");

            let n = self.count_where("llm_metrics", &predicate).await?;
            self.exec(&format!("DELETE FROM llm_metrics WHERE {predicate}"))
                .await?;
            self.exec(&format!("DELETE FROM llm_finish_metrics WHERE {predicate}"))
                .await?;
            report.metrics_deleted.insert(label.clone(), n);
            metrics_swept = true;
        }
        if metrics_swept {
            swept.push("llm_metrics");
            swept.push("llm_finish_metrics");
        }

        // ClickHouse has no CHECKPOINT; when configured, merge away the
        // delete tombstones for each swept table. Default off → skip.
        if self.optimize_on_sweep {
            for tbl in swept {
                self.exec(&format!("OPTIMIZE TABLE {tbl} FINAL")).await?;
            }
        }

        Ok(report)
    }
}
