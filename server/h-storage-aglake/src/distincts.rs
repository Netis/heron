//! Filter-dropdown distincts and the agent rollups.
//!
//! The four `query_distinct_*` methods carry no time range in their trait
//! signature, so they search unbounded — the one place this backend cannot
//! prune buckets. They stay cheap because `stats … by <field>` on an indexed
//! field reads postings rather than events, but their cost does grow with
//! retention, which is worth knowing when a deployment's dropdowns get slow.

use h_common::error::Result;
use h_storage::query::*;

use crate::client::Row;
use crate::rows::{ST_SPAN, ST_TRACE};
use crate::spl::{self, Search};
use crate::AglakeBackend;

/// Ceiling on a dropdown's option count. A filter list past this is unusable
/// in the UI anyway, and the cap keeps a runaway-cardinality field (a client
/// inventing model names) from turning a dropdown into a full scan.
const MAX_DISTINCT: usize = 10_000;

impl AglakeBackend {
    /// `stats count by <field>` over one index, ascending, capped.
    async fn distinct_values(
        &self,
        what: &'static str,
        index: &str,
        sourcetype: &str,
        field: &str,
    ) -> Result<Vec<String>> {
        let s = Search::new(index, sourcetype);
        let spl_q = format!(
            "{} | stats count by {field} | sort {MAX_DISTINCT} +str({field}) | table {field}",
            s.build()
        );
        let rows = self.search.search_all_time(&spl_q).await?.rows();
        if rows.len() >= MAX_DISTINCT {
            tracing::warn!(
                target: "aglake::read",
                query = what,
                cap = MAX_DISTINCT,
                "aglake: distinct value list hit its cap and is truncated"
            );
        }
        Ok(rows
            .into_iter()
            .filter_map(|r| r.get(field).and_then(|v| v.as_str()).map(str::to_string))
            .filter(|v| !v.is_empty())
            .collect())
    }

    pub(crate) async fn query_distinct_wire_apis(&self) -> Result<Vec<String>> {
        self.distinct_values(
            "query_distinct_wire_apis",
            &self.ix.spans,
            ST_SPAN,
            "wire_api",
        )
        .await
    }

    pub(crate) async fn query_distinct_models(&self) -> Result<Vec<String>> {
        self.distinct_values("query_distinct_models", &self.ix.spans, ST_SPAN, "model")
            .await
    }

    pub(crate) async fn query_distinct_server_ips(&self) -> Result<Vec<String>> {
        self.distinct_values(
            "query_distinct_server_ips",
            &self.ix.spans,
            ST_SPAN,
            "server_ip",
        )
        .await
    }

    pub(crate) async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
        let s = Search::new(&self.ix.spans, ST_SPAN);
        let spl_q = format!(
            "{} | stats count by wire_api, finish_reason \
             | sort {MAX_DISTINCT} +str(wire_api), +str(finish_reason) \
             | table wire_api, finish_reason",
            s.build()
        );
        let rows = self.search.search_all_time(&spl_q).await?.rows();
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let wire_api = str_of(&r, "wire_api")?;
                let finish_reason = str_of(&r, "finish_reason")?;
                // Calls still in flight have no finish reason; they are not a
                // filter option.
                (!finish_reason.is_empty()).then_some(DistinctFinishReason {
                    wire_api,
                    finish_reason,
                })
            })
            .collect())
    }

    pub(crate) async fn query_distinct_agent_kinds(
        &self,
        query: &DistinctAgentKindsQuery,
    ) -> Result<Vec<String>> {
        let mut s = Search::new(&self.ix.traces, ST_TRACE);
        s.any_of("wire_api", &query.filter.wire_apis);
        s.any_of("models_used", &query.filter.models);
        s.any_of("server_ip", &query.filter.server_ips);
        if !query.include_proxy_hops {
            s.eq_num("proxy_hidden", 0);
        }
        let spl_q = format!(
            "{} | stats count by agent_kind | sort {MAX_DISTINCT} +str(agent_kind) \
             | table agent_kind",
            s.build()
        );
        let rows = self
            .search
            .search(
                &spl_q,
                &spl::epoch_secs(query.time_range.start_us),
                &spl::epoch_secs(query.time_range.end_us),
            )
            .await?
            .rows();
        Ok(rows
            .into_iter()
            .filter_map(|r| str_of(&r, "agent_kind"))
            .filter(|v| !v.is_empty())
            .collect())
    }

    pub(crate) async fn query_agent_summary(
        &self,
        query: &AgentSummaryQuery,
    ) -> Result<Vec<AgentKindSummary>> {
        let s = Search::new(&self.ix.traces, ST_TRACE);
        // `last_seen_ms` is the latest turn *start*, not the latest end — both
        // SQL backends read `max(start_time)`, and this is a cross-backend
        // contract rather than a judgement call. Ordering is likewise theirs:
        // busiest agent kind first.
        let spl_q = format!(
            "{} | stats count as turn_count, sum(total_input_tokens) as total_input_tokens, \
               sum(total_output_tokens) as total_output_tokens, \
               avg(duration_ms) as avg_duration_ms, max(ts_us) as last_us \
             by agent_kind \
             | sort 0 -num(turn_count), +str(agent_kind) \
             | table agent_kind, turn_count, total_input_tokens, total_output_tokens, \
               avg_duration_ms, last_us",
            s.build()
        );
        let rows = self
            .search
            .search(
                &spl_q,
                &spl::epoch_secs(query.time_range.start_us),
                &spl::epoch_secs(query.time_range.end_us),
            )
            .await?
            .rows();
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let agent_kind = str_of(&r, "agent_kind")?;
                Some(AgentKindSummary {
                    agent_kind,
                    turn_count: int(&r, "turn_count"),
                    total_input_tokens: int(&r, "total_input_tokens"),
                    total_output_tokens: int(&r, "total_output_tokens"),
                    avg_duration_ms: num(&r, "avg_duration_ms"),
                    last_seen_ms: num(&r, "last_us").unwrap_or(0.0) as i64 / 1000,
                })
            })
            .collect())
    }

    pub(crate) async fn query_agent_activity(
        &self,
        query: &AgentActivityQuery,
    ) -> Result<Vec<AgentActivityPoint>> {
        // With no explicit hint, pick a bucket that lands the chart at roughly
        // 60-180 points and snaps to a width whose tick labels read cleanly.
        // Copied from the SQL backends rather than reinvented: a different
        // default here would put the same request on a different time grid
        // depending on which backend answered it.
        let window_secs =
            ((query.time_range.end_us - query.time_range.start_us) / 1_000_000).max(60);
        let bucket = query
            .bucket_seconds
            .unwrap_or_else(|| {
                let target = (window_secs / 120).max(60) as u32;
                for &nice in &[60u32, 300, 600, 1800, 3600, 7200, 14400, 86400] {
                    if target <= nice {
                        return nice;
                    }
                }
                86400
            })
            .max(1);
        let s = Search::new(&self.ix.traces, ST_TRACE);
        // Bucket on `_time` rather than `ts_us`: `bin` works on the event
        // timestamp, and bucket starts are whole seconds, so the f64 precision
        // that makes `_time` unusable for ordering is irrelevant here.
        let spl_q = format!(
            "{} | bin _time span={bucket}s | stats count as turn_count by _time, agent_kind \
             | sort 0 +num(_time), +str(agent_kind) | table _time, agent_kind, turn_count",
            s.build()
        );
        let rows = self
            .search
            .search(
                &spl_q,
                &spl::epoch_secs(query.time_range.start_us),
                &spl::epoch_secs(query.time_range.end_us),
            )
            .await?
            .rows();
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let agent_kind = str_of(&r, "agent_kind")?;
                Some(AgentActivityPoint {
                    timestamp_ms: (num(&r, "_time").unwrap_or(0.0) * 1000.0).round() as i64,
                    agent_kind,
                    turn_count: int(&r, "turn_count"),
                })
            })
            .collect())
    }
}

fn str_of(r: &Row, key: &str) -> Option<String> {
    r.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

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
