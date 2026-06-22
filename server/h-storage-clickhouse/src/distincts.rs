//! Distinct-value queries used to populate filter dropdowns. ClickHouse port of
//! `h-storage-duckdb/src/distincts.rs` (+ the `query_distinct_finish_reasons`
//! that lives in DuckDB's `metrics.rs`).
//!
//! The first three read the pre-aggregated `llm_metrics` table and exclude the
//! `'*'` rollup tier; `query_distinct_finish_reasons` does the same against
//! `llm_finish_metrics`. `query_distinct_agent_kinds` reads the mutable
//! `traces` table (so it uses `FINAL`) and optionally drops proxy-hop rows
//! by inspecting the `metadata` JSON.

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::Result;
use h_storage::query::{DistinctAgentKindsQuery, DistinctFinishReason};

use crate::client::ch_err;
use crate::sql::{sql_in_list, time_where};
use crate::ClickHouseBackend;

/// Single-column `SELECT DISTINCT x AS v` row. `v` must match the SELECT alias;
/// the source columns are non-null `String` so this stays a plain `String`.
#[derive(Row, Deserialize)]
struct StringRow {
    v: String,
}

/// Row for the `(wire_api, finish_reason)` pair query. Field names match the
/// SELECT column names; both columns are non-null `String` in `llm_finish_metrics`.
#[derive(Row, Deserialize)]
struct FinishReasonRow {
    wire_api: String,
    finish_reason: String,
}

impl ClickHouseBackend {
    pub(crate) async fn query_distinct_wire_apis(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT DISTINCT wire_api AS v FROM llm_metrics \
                 WHERE wire_api != '*' ORDER BY wire_api",
            )
            .fetch_all::<StringRow>()
            .await
            .map_err(|e| ch_err("query_distinct_wire_apis", e))?;
        Ok(rows.into_iter().map(|r| r.v).collect())
    }

    pub(crate) async fn query_distinct_models(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT DISTINCT model AS v FROM llm_metrics \
                 WHERE model != '*' ORDER BY model",
            )
            .fetch_all::<StringRow>()
            .await
            .map_err(|e| ch_err("query_distinct_models", e))?;
        Ok(rows.into_iter().map(|r| r.v).collect())
    }

    pub(crate) async fn query_distinct_server_ips(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT DISTINCT server_ip AS v FROM llm_metrics \
                 WHERE server_ip != '*' ORDER BY server_ip",
            )
            .fetch_all::<StringRow>()
            .await
            .map_err(|e| ch_err("query_distinct_server_ips", e))?;
        Ok(rows.into_iter().map(|r| r.v).collect())
    }

    pub(crate) async fn query_distinct_agent_kinds(
        &self,
        query: &DistinctAgentKindsQuery,
    ) -> Result<Vec<String>> {
        // `traces` is a ReplacingMergeTree, so reads must use FINAL to see
        // the latest version per turn_id.
        let mut where_parts = vec![time_where(
            "start_time",
            query.time_range.start_us,
            query.time_range.end_us,
        )];

        if !query.filter.wire_apis.is_empty() {
            where_parts.push(format!(
                "wire_api IN ({})",
                sql_in_list(&query.filter.wire_apis)
            ));
        }
        if !query.filter.models.is_empty() {
            // `models_used` is stored as a JSON string array. DuckDB uses
            // `list_has_any`; the ClickHouse equivalent parses the JSON to
            // `Array(String)` and tests overlap with the filter list.
            where_parts.push(format!(
                "hasAny(JSONExtract(coalesce(models_used, '[]'), 'Array(String)'), [{}])",
                sql_in_list(&query.filter.models)
            ));
        }
        if !query.filter.server_ips.is_empty() {
            where_parts.push(format!(
                "server_ip IN ({})",
                sql_in_list(&query.filter.server_ips)
            ));
        }
        if !query.include_proxy_hops {
            // `metadata` is Nullable(String) holding JSON. JSONExtractString
            // returns '' when the path is absent, so DuckDB's `IS NULL` maps to
            // `= ''`; the role exclusion stays an explicit NOT IN list.
            where_parts.push(
                "(JSONExtractString(coalesce(metadata, ''), 'proxy', 'role') = '' \
                   OR JSONExtractString(coalesce(metadata, ''), 'proxy', 'role') \
                      NOT IN ('proxy_out', 'mirror_secondary'))"
                    .to_string(),
            );
        }

        let sql = format!(
            "SELECT DISTINCT agent_kind AS v FROM traces FINAL \
             WHERE {} ORDER BY agent_kind",
            where_parts.join(" AND ")
        );
        let rows = self
            .client
            .query(&sql)
            .fetch_all::<StringRow>()
            .await
            .map_err(|e| ch_err("query_distinct_agent_kinds", e))?;
        Ok(rows.into_iter().map(|r| r.v).collect())
    }

    pub(crate) async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
        // Source: llm_finish_metrics. The `wire_api != '*'` filter excludes the
        // cross-wire-api rollup tier; finish_reason is always concrete in this
        // table, but we keep the `!= '*'` symmetry for safety against future
        // schema changes (matches the DuckDB backend).
        let rows = self
            .client
            .query(
                "SELECT DISTINCT wire_api, finish_reason \
                 FROM llm_finish_metrics \
                 WHERE wire_api != '*' AND finish_reason != '*' \
                 ORDER BY wire_api, finish_reason",
            )
            .fetch_all::<FinishReasonRow>()
            .await
            .map_err(|e| ch_err("query_distinct_finish_reasons", e))?;
        Ok(rows
            .into_iter()
            .map(|r| DistinctFinishReason {
                wire_api: r.wire_api,
                finish_reason: r.finish_reason,
            })
            .collect())
    }
}
