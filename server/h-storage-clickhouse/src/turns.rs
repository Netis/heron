//! `agent_turns` table I/O — write, paginated query, by-id detail, pair-sweeper
//! support (`query_pair_candidates` / `update_turn_metadata`).
//!
//! `agent_turns` is `ReplacingMergeTree(_version)`: it is the only mutated
//! table. Writes insert with `_version = end_time` (micros); reads use `FINAL`;
//! `update_turn_metadata` reads the full row (FINAL), merges the JSON patch, and
//! re-inserts the whole row with a wall-clock-micros `_version` so the latest
//! metadata wins on the next FINAL read.

use std::time::{SystemTime, UNIX_EPOCH};

use clickhouse::Row;
use serde::Deserialize;

use h_common::error::{AppError, Result};
use h_storage::convert::parse_json_string_list;
use h_storage::dialect::sql_in_list;
use h_storage::query::*;
use h_turn::{AgentTurn, PairCandidate};

use crate::client::{ch_err, insert_all};
use crate::rows::TurnRow;
use crate::sql::{escape_str, time_where};
use crate::ClickHouseBackend;

/// Full `agent_turns` column list in `TurnRow` field order, with the two
/// `DateTime64(6)` columns surfaced as `i64` micros so they deserialize into
/// `TurnRow`'s `i64` fields and round-trip on re-insert. Used by
/// `update_turn_metadata`'s read-modify-write.
const TURN_ROW_SELECT: &str = "turn_id, source_id, session_id, wire_api, agent_kind, \
     client_ip, server_ip, \
     toUnixTimestamp64Micro(start_time) AS start_time, \
     toUnixTimestamp64Micro(end_time) AS end_time, \
     duration_ms, call_count, models_used, subagents_used, \
     total_input_tokens, total_output_tokens, \
     total_cache_read_input_tokens, total_cache_creation_input_tokens, \
     total_cost_usd, status, final_finish_reason, \
     user_input_preview, user_call_id, final_answer_preview, final_call_id, \
     call_ids, metadata, tool_surfaces_json, tool_call_total, agent_topology, \
     suspicious_skills_json, _version";

/// Read `metadata.proxy.{role, peer_turn_id, peer_turn_ids}` out of a row's
/// stored JSON. All-`None` for direct turns. Ported verbatim from the DuckDB
/// backend so list + detail share one parsing rule.
fn extract_proxy_fields(
    metadata_raw: Option<String>,
) -> (Option<String>, Option<String>, Option<Vec<String>>) {
    let Some(text) = metadata_raw else {
        return (None, None, None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (None, None, None);
    };
    let proxy = v.get("proxy");
    let role = proxy
        .and_then(|p| p.get("role"))
        .and_then(|r| r.as_str())
        .map(String::from);
    let peer_id = proxy
        .and_then(|p| p.get("peer_turn_id"))
        .and_then(|r| r.as_str())
        .map(String::from);
    let peer_ids = proxy.and_then(|p| p.get("peer_turn_ids")).and_then(|a| {
        a.as_array().map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
    });
    (role, peer_id, peer_ids)
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[derive(Row, Deserialize)]
struct TurnListRow {
    turn_id: String,
    source_id: String,
    session_id: String,
    start_time_ms: i64,
    end_time_ms: i64,
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
    client_ip: String,
    server_ip: String,
    metadata: Option<String>,
    tool_surfaces_json: Option<String>,
    tool_call_total: u32,
    agent_topology: Option<String>,
    suspicious_skills_json: Option<String>,
}

#[derive(Row, Deserialize)]
struct TurnDetailRow {
    turn_id: String,
    source_id: String,
    session_id: String,
    wire_api: String,
    agent_kind: String,
    start_time_ms: i64,
    end_time_ms: i64,
    duration_ms: u64,
    call_count: u32,
    models_used: Option<String>,
    subagents_used: Option<String>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_input_tokens: u64,
    total_cache_creation_input_tokens: u64,
    total_cost_usd: Option<f64>,
    status: String,
    final_finish_reason: Option<String>,
    user_input_preview: Option<String>,
    user_call_id: Option<String>,
    final_answer_preview: Option<String>,
    final_call_id: Option<String>,
    call_ids: String,
    metadata: Option<String>,
    client_ip: String,
    server_ip: String,
    tool_surfaces_json: Option<String>,
    tool_call_total: u32,
    agent_topology: Option<String>,
    suspicious_skills_json: Option<String>,
}

#[derive(Row, Deserialize)]
struct PairCandidateRow {
    turn_id: String,
    session_id: String,
    agent_kind: String,
    wire_api: String,
    start_time_us: i64,
    end_time_us: i64,
    call_count: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    final_finish_reason: Option<String>,
    models_used: Option<String>,
    client_ip: String,
    server_ip: String,
}

#[derive(Row, Deserialize)]
struct CountRow {
    n: u64,
}

impl ClickHouseBackend {
    pub(crate) async fn write_turns(&self, turns: Vec<AgentTurn>) -> Result<()> {
        let rows: Vec<TurnRow> = turns.into_iter().map(TurnRow::from).collect();
        insert_all!(self.client, "agent_turns", TurnRow, rows);
        Ok(())
    }

    pub(crate) async fn query_turns(&self, query: &TurnsQuery) -> Result<TurnsPage> {
        const VALID_SORT_FIELDS: &[&str] = &[
            "start_time",
            "end_time",
            "duration_ms",
            "call_count",
            "total_input_tokens",
            "total_output_tokens",
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

        let mut where_parts = vec![time_where(
            "start_time",
            query.time_range.start_us,
            query.time_range.end_us,
        )];
        if !query.filter.wire_apis.is_empty() {
            where_parts.push(format!("wire_api IN ({})", sql_in_list(&query.filter.wire_apis)));
        }
        if !query.filter.models.is_empty() {
            // models_used is a JSON-array String; match if any requested model
            // is present (DuckDB list_has_any → ClickHouse hasAny).
            where_parts.push(format!(
                "hasAny(JSONExtract(coalesce(models_used, '[]'), 'Array(String)'), [{}])",
                sql_in_list(&query.filter.models)
            ));
        }
        if !query.statuses.is_empty() {
            where_parts.push(format!("status IN ({})", sql_in_list(&query.statuses)));
        }
        if !query.agent_kinds.is_empty() {
            where_parts.push(format!("agent_kind IN ({})", sql_in_list(&query.agent_kinds)));
        }
        if !query.client_ips.is_empty() {
            where_parts.push(format!("client_ip IN ({})", sql_in_list(&query.client_ips)));
        }
        if !query.server_ports.is_empty() {
            // agent_turns has no server_port; resolve via the turn's first
            // call_id against llm_calls. ClickHouse can't do the DuckDB
            // correlated EXISTS, so use an uncorrelated IN-subquery (still
            // not a JOIN): the turn's first call_id ∈ { calls on those ports }.
            let ports: Vec<String> = query.server_ports.iter().map(|p| p.to_string()).collect();
            where_parts.push(format!(
                "arrayElement(JSONExtract(call_ids, 'Array(String)'), 1) IN \
                 (SELECT id FROM llm_calls WHERE server_port IN ({}))",
                ports.join(", ")
            ));
        }
        if !query.filter.server_ips.is_empty() {
            where_parts.push(format!("server_ip IN ({})", sql_in_list(&query.filter.server_ips)));
        }
        if !query.include_proxy_hops {
            // Hide the sweeper-folded hops. JSONExtractString returns '' when
            // absent, and '' NOT IN (...) is true, so direct turns +
            // proxy_in/mirror_primary stay visible.
            where_parts.push(
                "JSONExtractString(coalesce(metadata, ''), 'proxy', 'role') \
                 NOT IN ('proxy_out', 'mirror_secondary')"
                    .to_string(),
            );
        }
        let where_sql = where_parts.join(" AND ");

        let total = self
            .client
            .query(&format!(
                "SELECT count() AS n FROM agent_turns FINAL WHERE {where_sql}"
            ))
            .fetch_one::<CountRow>()
            .await
            .map_err(|e| ch_err("query_turns count", e))?
            .n;

        let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
        let items_sql = format!(
            "SELECT turn_id, source_id, session_id, \
             toUnixTimestamp64Milli(start_time) AS start_time_ms, \
             toUnixTimestamp64Milli(end_time) AS end_time_ms, \
             duration_ms, wire_api, agent_kind, models_used, call_count, \
             total_input_tokens, total_output_tokens, status, final_finish_reason, \
             user_input_preview, final_answer_preview, client_ip, server_ip, metadata, \
             tool_surfaces_json, tool_call_total, agent_topology, suspicious_skills_json \
             FROM agent_turns FINAL WHERE {where_sql} \
             ORDER BY {} {sort_order} LIMIT {} OFFSET {offset}",
            query.sort_by, query.page_size,
        );
        let rows = self
            .client
            .query(&items_sql)
            .fetch_all::<TurnListRow>()
            .await
            .map_err(|e| ch_err("query_turns items", e))?;

        let items = rows
            .into_iter()
            .map(|r| {
                let models_used = parse_json_string_list(r.models_used.as_deref());
                let primary_model = models_used.first().cloned();
                let (proxy_role, proxy_peer_turn_id, proxy_peer_turn_ids) =
                    extract_proxy_fields(r.metadata);
                let tool_surfaces = parse_json_string_list(r.tool_surfaces_json.as_deref());
                let suspicious_skills: Vec<serde_json::Value> = r
                    .suspicious_skills_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                TurnListItem {
                    turn_id: r.turn_id,
                    source_id: r.source_id,
                    session_id: r.session_id,
                    start_time: r.start_time_ms,
                    end_time: r.end_time_ms,
                    duration_ms: r.duration_ms,
                    wire_api: r.wire_api,
                    agent_kind: r.agent_kind,
                    client_ip: r.client_ip,
                    server_ip: r.server_ip,
                    primary_model,
                    models_used,
                    call_count: r.call_count,
                    total_input_tokens: r.total_input_tokens,
                    total_output_tokens: r.total_output_tokens,
                    status: r.status,
                    final_finish_reason: r.final_finish_reason,
                    user_input_preview: r.user_input_preview,
                    final_answer_preview: r.final_answer_preview,
                    proxy_role,
                    proxy_peer_turn_id,
                    proxy_peer_turn_ids,
                    tool_surfaces,
                    tool_call_total: r.tool_call_total,
                    agent_topology: r.agent_topology,
                    suspicious_skills,
                }
            })
            .collect();
        Ok(TurnsPage { total, items })
    }

    pub(crate) async fn query_turn_by_id(&self, turn_id: &str) -> Result<Option<TurnDetail>> {
        let sql = format!(
            "SELECT turn_id, source_id, session_id, wire_api, agent_kind, \
             toUnixTimestamp64Milli(start_time) AS start_time_ms, \
             toUnixTimestamp64Milli(end_time) AS end_time_ms, \
             duration_ms, call_count, models_used, subagents_used, \
             total_input_tokens, total_output_tokens, \
             total_cache_read_input_tokens, total_cache_creation_input_tokens, \
             total_cost_usd, status, final_finish_reason, \
             user_input_preview, user_call_id, final_answer_preview, final_call_id, \
             call_ids, metadata, client_ip, server_ip, \
             tool_surfaces_json, tool_call_total, agent_topology, suspicious_skills_json \
             FROM agent_turns FINAL WHERE turn_id = '{}' LIMIT 1",
            escape_str(turn_id)
        );
        let row = self
            .client
            .query(&sql)
            .fetch_all::<TurnDetailRow>()
            .await
            .map_err(|e| ch_err("query_turn_by_id", e))?
            .into_iter()
            .next();
        let Some(r) = row else { return Ok(None) };

        let models_used = parse_json_string_list(r.models_used.as_deref());
        let subagents_used = parse_json_string_list(r.subagents_used.as_deref());
        let call_ids = parse_json_string_list(Some(&r.call_ids));
        let metadata = r
            .metadata
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        let tool_surfaces = parse_json_string_list(r.tool_surfaces_json.as_deref());
        let suspicious_skills: Vec<serde_json::Value> = r
            .suspicious_skills_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        // Divergence from DuckDB: full user_input / final_answer would require
        // re-running the agent profile extractor over the referenced call
        // bodies (the DuckDB path's `extract_full_text`). We surface the stored
        // previews best-effort; truncated previews (ending `…`) stay truncated.
        Ok(Some(TurnDetail {
            turn_id: r.turn_id,
            source_id: r.source_id,
            session_id: r.session_id,
            wire_api: r.wire_api,
            agent_kind: r.agent_kind,
            client_ip: r.client_ip,
            server_ip: r.server_ip,
            start_time: r.start_time_ms,
            end_time: r.end_time_ms,
            duration_ms: r.duration_ms,
            call_count: r.call_count,
            models_used,
            subagents_used,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cache_read_input_tokens: r.total_cache_read_input_tokens,
            total_cache_creation_input_tokens: r.total_cache_creation_input_tokens,
            total_cost_usd: r.total_cost_usd,
            status: r.status,
            final_finish_reason: r.final_finish_reason,
            user_call_id: r.user_call_id,
            user_input: r.user_input_preview,
            final_call_id: r.final_call_id,
            final_answer: r.final_answer_preview,
            call_ids,
            metadata,
            tool_surfaces,
            tool_call_total: r.tool_call_total,
            agent_topology: r.agent_topology,
            suspicious_skills,
        }))
    }

    pub(crate) async fn query_pair_candidates(
        &self,
        start_us: i64,
        end_us: i64,
    ) -> Result<Vec<PairCandidate>> {
        let ts_pred = time_where("start_time", start_us, end_us);
        let sql = format!(
            "SELECT turn_id, session_id, agent_kind, wire_api, \
             toUnixTimestamp64Micro(start_time) AS start_time_us, \
             toUnixTimestamp64Micro(end_time) AS end_time_us, \
             call_count, total_input_tokens, total_output_tokens, \
             final_finish_reason, models_used, client_ip, server_ip \
             FROM agent_turns FINAL \
             WHERE {ts_pred} \
               AND JSONExtractString(coalesce(metadata, ''), 'proxy', 'role') = '' \
             ORDER BY start_time ASC"
        );
        let rows = self
            .client
            .query(&sql)
            .fetch_all::<PairCandidateRow>()
            .await
            .map_err(|e| ch_err("query_pair_candidates", e))?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let models = parse_json_string_list(r.models_used.as_deref());
                let primary_model = models.first().cloned();
                PairCandidate {
                    turn_id: r.turn_id,
                    session_id: r.session_id,
                    agent_kind: r.agent_kind,
                    wire_api: r.wire_api,
                    start_time_us: r.start_time_us,
                    end_time_us: r.end_time_us,
                    call_count: r.call_count,
                    total_input_tokens: r.total_input_tokens,
                    total_output_tokens: r.total_output_tokens,
                    final_finish_reason: r.final_finish_reason,
                    primary_model,
                    network_view: format!("{}->{}", r.client_ip, r.server_ip),
                }
            })
            .collect())
    }

    pub(crate) async fn update_turn_metadata(
        &self,
        turn_id: &str,
        patch: serde_json::Value,
    ) -> Result<()> {
        // Read-modify-write on ReplacingMergeTree: fetch the current full row
        // (FINAL = latest version), shallow-merge the patch into metadata, and
        // re-insert with a strictly-greater `_version` (wall-clock micros).
        let sql = format!(
            "SELECT {TURN_ROW_SELECT} FROM agent_turns FINAL WHERE turn_id = '{}' LIMIT 1",
            escape_str(turn_id)
        );
        let existing = self
            .client
            .query(&sql)
            .fetch_all::<TurnRow>()
            .await
            .map_err(|e| ch_err("update_turn_metadata read", e))?
            .into_iter()
            .next();
        let Some(mut row) = existing else {
            // Turn not present yet — the sweeper races finalization; drop.
            return Ok(());
        };

        let mut base = row
            .metadata
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if !base.is_object() {
            base = serde_json::json!({});
        }
        if let (Some(obj), Some(patch_obj)) = (base.as_object_mut(), patch.as_object()) {
            for (k, v) in patch_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        row.metadata = Some(base.to_string());
        row._version = now_micros();

        insert_all!(self.client, "agent_turns", TurnRow, vec![row]);
        Ok(())
    }
}
