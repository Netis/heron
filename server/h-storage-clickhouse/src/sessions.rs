//! Session-scoped queries. Sessions are a view over `agent_turns` grouped by
//! `(source_id, session_id)` — no schema of their own. Ported from the DuckDB
//! backend (`h-storage-duckdb/src/sessions.rs`); aggregates, windowing, and
//! cursor semantics match it column-for-column.
//!
//! `agent_turns` is a `ReplacingMergeTree(_version)`, so every read here uses
//! `FROM agent_turns FINAL` to ensure only the latest version per `turn_id`
//! participates in the aggregates (otherwise stale pre-`update_turn_metadata`
//! rows would double-count tokens / costs and skew MIN/MAX).
//!
//! Divergence from DuckDB — `query_session_turns` user_input / final_answer:
//! the DuckDB backend reconstructs the FULL user_input / final_answer by
//! re-running each agent profile's body extractor over the referenced call
//! bodies (`extract_full_text_batch`, which needs a duckdb `Connection` and is
//! not available here). For shape parity this backend populates
//! `SessionTurnItem.user_input` / `final_answer` best-effort from the turn's
//! stored `user_input_preview` / `final_answer_preview` columns instead. These
//! may be truncated (preview strings end with `…`) where the DuckDB backend
//! would return the full text. See the per-row comment below.

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::Result;
use h_storage::convert::parse_json_string_list;
use h_storage::dialect::{parse_csv, sql_in_list};
use h_storage::query::*;

use crate::client::ch_err;
use crate::sql::{escape_str, time_where};
use crate::ClickHouseBackend;

/// Step-1 row: one `(source_id, session_id)` key with its windowed
/// MAX(end_time) (ms), used for inclusion + cursor ordering.
#[derive(Row, Deserialize)]
struct SessionKeyRow {
    source_id: String,
    session_id: String,
    last_ms: i64,
}

/// Step-2 / detail row: full-lifetime aggregate per session plus the
/// earliest-turn preview. Field order matches the SELECT column order.
#[derive(Row, Deserialize)]
struct SessionAggRow {
    source_id: String,
    session_id: String,
    first_ms: i64,
    last_ms: i64,
    turn_count: u64,
    call_count: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_input_tokens: u64,
    total_cache_creation_input_tokens: u64,
    total_cost_usd: Option<f64>,
    agent_kind: String,
    first_input: Option<String>,
    first_call_id: Option<String>,
}

/// One page row for `query_session_turns`. Mirrors the DuckDB SELECT column
/// list; preview/call-id columns are carried so we can populate
/// `user_input` / `final_answer` best-effort (see module divergence note).
#[derive(Row, Deserialize)]
struct SessionTurnRow {
    turn_id: String,
    source_id: String,
    session_id: String,
    start_ms: i64,
    end_ms: i64,
    duration_ms: u64,
    wire_api: String,
    agent_kind: String,
    models_used: Option<String>,
    call_count: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    status: String,
    final_finish_reason: Option<String>,
    user_input_preview: Option<String>,
    final_answer_preview: Option<String>,
    tool_surfaces_json: Option<String>,
    tool_call_total: u32,
    agent_topology: Option<String>,
    suspicious_skills_json: Option<String>,
}

impl ClickHouseBackend {
    pub(crate) async fn query_sessions(&self, query: &SessionListQuery) -> Result<SessionsPage> {
        let page_size = query.page_size.max(1);

        // Step 1 WHERE: time window + optional source/agent_kind. Both optional
        // fields are session-stable (same session -> same value), so pushing
        // them into WHERE does not truncate the lifetime aggregates computed in
        // Step 2. The time-range predicate filters on `end_time` (turn-in-window
        // inclusion), matching the DuckDB code.
        let mut where_parts =
            vec![time_where("end_time", query.time_range.start_us, query.time_range.end_us)];
        if let Some(sid) = &query.source_id {
            where_parts.push(format!("source_id = '{}'", escape_str(sid)));
        }
        if let Some(ak) = &query.agent_kind {
            // `agent_kind` arrives as a CSV multi-select (e.g. "claude-cli,codex-cli").
            // Parse + IN-match so multiple kinds union, instead of exact-matching the
            // whole CSV string (which selects nothing). Mirrors the DuckDB backend.
            let kinds = parse_csv(ak);
            if !kinds.is_empty() {
                where_parts.push(format!("agent_kind IN ({})", sql_in_list(&kinds)));
            }
        }
        let where_sql = where_parts.join(" AND ");

        // Cursor HAVING clause. Tuple comparison lets us sort by
        // (MAX(end_time), source_id, session_id) DESC uniformly. The windowed
        // MAX(end_time) is compared as a DateTime64 against the cursor's ms
        // timestamp via fromUnixTimestamp64Milli; string parts are escaped and
        // single-quoted.
        let having_sql = if let Some(c) = &query.cursor {
            format!(
                " HAVING (max(end_time), source_id, session_id) < \
                 (fromUnixTimestamp64Milli(toInt64({})), '{}', '{}')",
                c.last_turn_at_ms,
                escape_str(&c.source_id),
                escape_str(&c.session_id),
            )
        } else {
            String::new()
        };

        // Fetch one extra row to detect the next page without a count query.
        let limit = (page_size as u64) + 1;

        // Step 1: inclusion + ordering keys. FINAL so only the latest version of
        // each turn participates in MAX(end_time).
        let step1_sql = format!(
            "SELECT source_id, session_id, \
                    toUnixTimestamp64Milli(max(end_time)) AS last_ms \
             FROM agent_turns FINAL \
             WHERE {where_sql} \
             GROUP BY source_id, session_id{having_sql} \
             ORDER BY max(end_time) DESC, source_id DESC, session_id DESC \
             LIMIT {limit}"
        );

        let mut key_rows = self
            .client
            .query(&step1_sql)
            .fetch_all::<SessionKeyRow>()
            .await
            .map_err(|e| ch_err("query_sessions", e))?;

        let has_more = key_rows.len() > page_size as usize;
        if has_more {
            key_rows.truncate(page_size as usize);
        }
        if key_rows.is_empty() {
            return Ok(SessionsPage {
                items: vec![],
                next_cursor: None,
            });
        }

        // Step 2: full-lifetime aggregate + first-turn preview via ROW_NUMBER().
        // The pair list is inlined (ids are trusted internal strings already
        // vetted by Step 1, escaped defensively). FINAL on the inner read so the
        // window function and aggregates see only the latest version per turn.
        let pairs_sql = key_rows
            .iter()
            .map(|r| format!("('{}', '{}')", escape_str(&r.source_id), escape_str(&r.session_id)))
            .collect::<Vec<_>>()
            .join(", ");

        let step2_sql = format!(
            "SELECT source_id, session_id, \
                    toUnixTimestamp64Milli(min(start_time)) AS first_ms, \
                    toUnixTimestamp64Milli(max(end_time))   AS last_ms, \
                    count() AS turn_count, \
                    sum(call_count) AS call_count, \
                    sum(total_input_tokens) AS total_input_tokens, \
                    sum(total_output_tokens) AS total_output_tokens, \
                    sum(total_cache_read_input_tokens) AS total_cache_read_input_tokens, \
                    sum(total_cache_creation_input_tokens) AS total_cache_creation_input_tokens, \
                    sum(total_cost_usd) AS total_cost_usd, \
                    min(agent_kind) AS agent_kind, \
                    min(if(rn = 1, user_input_preview, NULL)) AS first_input, \
                    min(if(rn = 1, user_call_id,       NULL)) AS first_call_id \
             FROM ( \
                SELECT source_id, session_id, start_time, end_time, call_count, \
                       total_input_tokens, total_output_tokens, \
                       total_cache_read_input_tokens, total_cache_creation_input_tokens, \
                       total_cost_usd, agent_kind, user_input_preview, user_call_id, \
                       row_number() OVER (PARTITION BY source_id, session_id ORDER BY start_time) AS rn \
                FROM agent_turns FINAL \
                WHERE (source_id, session_id) IN ({pairs_sql}) \
             ) t \
             GROUP BY source_id, session_id"
        );

        let agg_rows = self
            .client
            .query(&step2_sql)
            .fetch_all::<SessionAggRow>()
            .await
            .map_err(|e| ch_err("query_sessions", e))?;

        use std::collections::HashMap;
        let mut agg: HashMap<(String, String), SessionListItem> = HashMap::new();
        for r in agg_rows {
            let key = (r.source_id.clone(), r.session_id.clone());
            agg.insert(
                key,
                SessionListItem {
                    source_id: r.source_id,
                    session_id: r.session_id,
                    last_turn_at_in_window: 0,
                    first_turn_at: r.first_ms,
                    last_turn_at: r.last_ms,
                    turn_count: r.turn_count,
                    call_count: r.call_count,
                    total_input_tokens: r.total_input_tokens,
                    total_output_tokens: r.total_output_tokens,
                    total_cache_read_input_tokens: r.total_cache_read_input_tokens,
                    total_cache_creation_input_tokens: r.total_cache_creation_input_tokens,
                    total_cost_usd: r.total_cost_usd,
                    agent_kind: r.agent_kind,
                    first_user_input_preview: r.first_input,
                    first_user_call_id: r.first_call_id,
                },
            );
        }

        // Preserve Step 1's ordering and inject last_turn_at_in_window.
        let mut items: Vec<SessionListItem> = Vec::with_capacity(key_rows.len());
        for kr in &key_rows {
            if let Some(mut it) = agg.remove(&(kr.source_id.clone(), kr.session_id.clone())) {
                it.last_turn_at_in_window = kr.last_ms;
                items.push(it);
            }
        }

        let next_cursor = if has_more {
            items.last().map(|it| {
                encode_session_cursor(&SessionListCursor {
                    last_turn_at_ms: it.last_turn_at_in_window,
                    source_id: it.source_id.clone(),
                    session_id: it.session_id.clone(),
                })
            })
        } else {
            None
        };

        Ok(SessionsPage { items, next_cursor })
    }

    pub(crate) async fn query_session_by_id(
        &self,
        source_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionDetail>> {
        // Same full-lifetime aggregate as Step 2 above, scoped to one session.
        // FINAL so only the latest version per turn participates.
        let sql = format!(
            "SELECT source_id, session_id, \
                    toUnixTimestamp64Milli(min(start_time)) AS first_ms, \
                    toUnixTimestamp64Milli(max(end_time))   AS last_ms, \
                    count() AS turn_count, \
                    sum(call_count) AS call_count, \
                    sum(total_input_tokens) AS total_input_tokens, \
                    sum(total_output_tokens) AS total_output_tokens, \
                    sum(total_cache_read_input_tokens) AS total_cache_read_input_tokens, \
                    sum(total_cache_creation_input_tokens) AS total_cache_creation_input_tokens, \
                    sum(total_cost_usd) AS total_cost_usd, \
                    min(agent_kind) AS agent_kind, \
                    min(if(rn = 1, user_input_preview, NULL)) AS first_input, \
                    min(if(rn = 1, user_call_id,       NULL)) AS first_call_id \
             FROM ( \
                SELECT source_id, session_id, start_time, end_time, call_count, \
                       total_input_tokens, total_output_tokens, \
                       total_cache_read_input_tokens, total_cache_creation_input_tokens, \
                       total_cost_usd, agent_kind, user_input_preview, user_call_id, \
                       row_number() OVER (PARTITION BY source_id, session_id ORDER BY start_time) AS rn \
                FROM agent_turns FINAL \
                WHERE source_id = '{}' AND session_id = '{}' \
             ) t \
             GROUP BY source_id, session_id \
             LIMIT 1",
            escape_str(source_id),
            escape_str(session_id),
        );

        let row = self
            .client
            .query(&sql)
            .fetch_all::<SessionAggRow>()
            .await
            .map_err(|e| ch_err("query_session_by_id", e))?
            .into_iter()
            .next();

        // GROUP BY emits a row whenever the subquery matched at least one turn;
        // an empty session yields no rows -> None.
        Ok(row.map(|r| SessionDetail {
            source_id: r.source_id,
            session_id: r.session_id,
            first_turn_at: r.first_ms,
            last_turn_at: r.last_ms,
            turn_count: r.turn_count,
            call_count: r.call_count,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cache_read_input_tokens: r.total_cache_read_input_tokens,
            total_cache_creation_input_tokens: r.total_cache_creation_input_tokens,
            total_cost_usd: r.total_cost_usd,
            agent_kind: r.agent_kind,
            first_user_input_preview: r.first_input,
            first_user_call_id: r.first_call_id,
        }))
    }

    pub(crate) async fn query_session_turns(
        &self,
        query: &SessionTurnsQuery,
    ) -> Result<SessionTurnsPage> {
        let page_size = query.page_size.max(1);
        let limit = (page_size as u64) + 1;

        // Cursor filter (tuple comparison). ORDER BY start_time DESC, turn_id
        // DESC. start_time is a DateTime64; compare against the cursor's µs
        // timestamp via fromUnixTimestamp64Micro; turn_id escaped + quoted.
        let cursor_sql = if let Some(c) = &query.cursor {
            format!(
                " AND (start_time, turn_id) < \
                 (fromUnixTimestamp64Micro(toInt64({})), '{}')",
                c.start_time_us,
                escape_str(&c.turn_id),
            )
        } else {
            String::new()
        };

        // FINAL so only the latest version per turn is returned (matters after
        // the pair sweeper's update_turn_metadata re-inserts).
        let sql = format!(
            "SELECT turn_id, source_id, session_id, \
                    toUnixTimestamp64Milli(start_time) AS start_ms, \
                    toUnixTimestamp64Milli(end_time)   AS end_ms, \
                    duration_ms, wire_api, agent_kind, \
                    models_used, call_count, \
                    total_input_tokens, total_output_tokens, \
                    status, final_finish_reason, \
                    user_input_preview, final_answer_preview, \
                    tool_surfaces_json, tool_call_total, agent_topology, suspicious_skills_json \
             FROM agent_turns FINAL \
             WHERE source_id = '{}' AND session_id = '{}'{cursor_sql} \
             ORDER BY start_time DESC, turn_id DESC \
             LIMIT {limit}",
            escape_str(&query.source_id),
            escape_str(&query.session_id),
        );

        let mut rows = self
            .client
            .query(&sql)
            .fetch_all::<SessionTurnRow>()
            .await
            .map_err(|e| ch_err("query_session_turns", e))?;

        // Fetch+1 pattern: if we got page_size + 1 rows, there's a next page.
        let has_more = rows.len() as u64 > page_size as u64;
        if has_more {
            rows.truncate(page_size as usize);
        }

        let items: Vec<SessionTurnItem> = rows
            .into_iter()
            .map(|r| {
                let models_used = parse_json_string_list(r.models_used.as_deref());
                let primary_model = models_used.first().cloned();
                let tool_surfaces = parse_json_string_list(r.tool_surfaces_json.as_deref());
                let suspicious_skills: Vec<serde_json::Value> = r
                    .suspicious_skills_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();

                // DIVERGENCE from the DuckDB backend: it reconstructs the full
                // user_input / final_answer from the referenced call bodies via
                // `extract_full_text_batch` (a duckdb-Connection-bound helper not
                // available here). We populate these best-effort from the stored
                // preview columns; values may be truncated where DuckDB would
                // return full text.
                SessionTurnItem {
                    turn_id: r.turn_id,
                    source_id: r.source_id,
                    session_id: r.session_id,
                    start_time: r.start_ms,
                    end_time: r.end_ms,
                    duration_ms: r.duration_ms,
                    wire_api: r.wire_api,
                    agent_kind: r.agent_kind,
                    primary_model,
                    models_used,
                    call_count: r.call_count,
                    total_input_tokens: r.total_input_tokens,
                    total_output_tokens: r.total_output_tokens,
                    status: r.status,
                    final_finish_reason: r.final_finish_reason,
                    user_input: r.user_input_preview,
                    final_answer: r.final_answer_preview,
                    tool_surfaces,
                    tool_call_total: r.tool_call_total,
                    agent_topology: r.agent_topology,
                    suspicious_skills,
                }
            })
            .collect();

        let next_cursor = if has_more {
            items.last().map(|last| {
                encode_session_turns_cursor(&SessionTurnsCursor {
                    start_time_us: last.start_time.saturating_mul(1000),
                    turn_id: last.turn_id.clone(),
                })
            })
        } else {
            None
        };

        Ok(SessionTurnsPage { items, next_cursor })
    }
}
