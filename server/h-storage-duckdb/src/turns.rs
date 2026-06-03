//! `agent_turns` table I/O — write, paginated query, by-id detail.

use duckdb::types::{TimeUnit, Value};
use h_common::error::{AppError, Result};
use h_storage::query::*;
use h_turn::AgentTurn;

use crate::util::{extract_full_text, parse_json_string_list, us_to_timestamp, ExtractKind};
use crate::DuckDbBackend;
use h_turn::PairCandidate;

/// Read `metadata.proxy.{role, peer_turn_id, peer_turn_ids}` out of a
/// row's stored JSON. Returns all-`None` for direct turns (no
/// metadata, malformed metadata, or proxy block absent). Centralized
/// so the same parsing rule serves the list and detail handlers.
///
/// `peer_turn_ids` is the full sibling list (empty Vec when present
/// but actually empty). For groups of size 2 this contains one
/// element; for the haproxy 3-leg case, two.
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

struct PreparedTurn {
    turn_id: String,
    source_id: String,
    session_id: String,
    wire_api: String,
    agent_kind: String,
    client_ip: String,
    server_ip: String,
    start_time: Value,
    end_time: Value,
    duration_ms: u64,
    call_count: u32,
    models_used: String,
    subagents_used: String,
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
    metadata: String,
    tool_surfaces_json: String,
    tool_call_total: u32,
    agent_topology: Option<String>,
    suspicious_skills_json: String,
}

fn prepare_turn(t: AgentTurn) -> PreparedTurn {
    // Serialize tool_surfaces as a JSON array of snake_case strings.
    let tool_surfaces_json = {
        let strings: Vec<String> = t.tool_surfaces.iter().map(|s| s.to_string()).collect();
        serde_json::to_string(&strings).unwrap_or_else(|_| "[]".to_string())
    };
    let suspicious_skills_json =
        serde_json::to_string(&t.suspicious_skills).unwrap_or_else(|_| "[]".to_string());
    PreparedTurn {
        turn_id: t.turn_id,
        source_id: t.source_id,
        session_id: t.session_id,
        wire_api: t.wire_api,
        agent_kind: t.agent_kind,
        client_ip: t.client_ip.to_string(),
        server_ip: t.server_ip.to_string(),
        start_time: Value::Timestamp(TimeUnit::Microsecond, t.start_time_us),
        end_time: Value::Timestamp(TimeUnit::Microsecond, t.end_time_us),
        duration_ms: t.duration_ms,
        call_count: t.call_count,
        models_used: serde_json::to_string(&t.models_used).unwrap_or_default(),
        subagents_used: serde_json::to_string(&t.subagents_used).unwrap_or_default(),
        total_input_tokens: t.total_input_tokens,
        total_output_tokens: t.total_output_tokens,
        total_cache_read_input_tokens: t.total_cache_read_input_tokens,
        total_cache_creation_input_tokens: t.total_cache_creation_input_tokens,
        total_cost_usd: t.total_cost_usd,
        status: t.status.to_string(),
        final_finish_reason: t.final_finish_reason,
        user_input_preview: t.user_input_preview,
        user_call_id: t.user_call_id,
        final_answer_preview: t.final_answer_preview,
        final_call_id: t.final_call_id,
        call_ids: serde_json::to_string(&t.call_ids).unwrap_or_default(),
        metadata: t.metadata.to_string(),
        tool_surfaces_json,
        tool_call_total: t.tool_call_total,
        agent_topology: t.agent_topology.map(|top| top.to_string()),
        suspicious_skills_json,
    }
}

impl DuckDbBackend {
    pub(crate) async fn write_turns(&self, turns: Vec<AgentTurn>) -> Result<()> {
        if turns.is_empty() {
            return Ok(());
        }
        #[cfg(feature = "fault-injection")]
        {
            use crate::fault_injection::FaultPoint;
            if self.fault_set.should_fire(FaultPoint::DuckDbInvalidate) {
                return Err(crate::fault_injection::fatal_invalidate_error());
            }
            if self.fault_set.should_fire(FaultPoint::DiskFull) {
                return Err(crate::fault_injection::disk_full_error());
            }
        }
        let conn = self.write_turns_conn.clone();
        tokio::task::spawn_blocking(move || {
            let prepared: Vec<PreparedTurn> = turns.into_iter().map(prepare_turn).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("agent_turns")
                .map_err(|e| AppError::Storage(format!("failed to create turns appender: {e}")))?;
            for p in &prepared {
                appender
                    .append_row(duckdb::params![
                        p.turn_id,
                        p.source_id,
                        p.session_id,
                        p.wire_api,
                        p.agent_kind,
                        p.client_ip,
                        p.server_ip,
                        p.start_time,
                        p.end_time,
                        p.duration_ms,
                        p.call_count,
                        p.models_used,
                        p.subagents_used,
                        p.total_input_tokens,
                        p.total_output_tokens,
                        p.total_cache_read_input_tokens,
                        p.total_cache_creation_input_tokens,
                        p.total_cost_usd,
                        p.status,
                        p.final_finish_reason,
                        p.user_input_preview,
                        p.user_call_id,
                        p.final_answer_preview,
                        p.final_call_id,
                        p.call_ids,
                        p.metadata,
                        p.tool_surfaces_json,
                        p.tool_call_total,
                        p.agent_topology,
                        p.suspicious_skills_json,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append turn: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush turns: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
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

            let mut where_parts = vec!["start_time >= ?".to_string(), "start_time < ?".to_string()];

            if !query.filter.wire_apis.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .wire_apis
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("wire_api IN ({})", list.join(", ")));
            }
            if !query.filter.models.is_empty() {
                // models_used is stored as a JSON-encoded VARCHAR of Vec<String>.
                // Match if any requested model appears in the stored list.
                let list: Vec<String> = query
                    .filter
                    .models
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!(
                    "list_has_any(CAST(CAST(models_used AS JSON) AS VARCHAR[]), [{}])",
                    list.join(", ")
                ));
            }
            if !query.statuses.is_empty() {
                let list: Vec<String> = query
                    .statuses
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("status IN ({})", list.join(", ")));
            }
            if !query.agent_kinds.is_empty() {
                let list: Vec<String> = query
                    .agent_kinds
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("agent_kind IN ({})", list.join(", ")));
            }
            if !query.client_ips.is_empty() {
                let list: Vec<String> = query
                    .client_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("client_ip IN ({})", list.join(", ")));
            }
            if !query.server_ports.is_empty() {
                // agent_turns has no server_port column, so we resolve it
                // via the turn's first call_id against llm_calls — same
                // shortcut the topology query uses. EXISTS is cheaper
                // than a JOIN here because we never select from the
                // joined row.
                let list: Vec<String> = query.server_ports.iter().map(|p| p.to_string()).collect();
                where_parts.push(format!(
                    "EXISTS (SELECT 1 FROM llm_calls c \
                       WHERE c.id = json_extract_string(agent_turns.call_ids, '$[0]') \
                         AND c.server_port IN ({}))",
                    list.join(", ")
                ));
            }
            if !query.filter.server_ips.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .server_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("server_ip IN ({})", list.join(", ")));
            }
            if !query.include_proxy_hops {
                // Default list view: hide the hop the sweeper marked as
                // hidden (proxy_out + mirror_secondary). proxy_in /
                // mirror_primary stay visible. Direct turns (no
                // metadata.proxy.role) also stay visible because
                // json_extract_string returns NULL and NULL NOT IN (...)
                // is NULL → the IS NULL branch keeps them.
                where_parts.push(
                    "(json_extract_string(metadata, '$.proxy.role') IS NULL \
                       OR json_extract_string(metadata, '$.proxy.role') \
                          NOT IN ('proxy_out', 'mirror_secondary'))"
                        .to_string(),
                );
            }

            let where_sql = where_parts.join(" AND ");
            let sort_by = &query.sort_by;

            let count_sql = format!("SELECT COUNT(*) FROM agent_turns WHERE {where_sql}");
            let mut count_stmt = conn
                .prepare(&count_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare count query: {e}")))?;
            let total: u64 = count_stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| row.get(0))
                .map_err(|e| AppError::Storage(format!("failed to execute count query: {e}")))?;

            let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
            let limit = query.page_size;
            let items_sql = format!(
                "SELECT turn_id, source_id, session_id, \
                 epoch_ms(start_time), epoch_ms(end_time), duration_ms, \
                 wire_api, agent_kind, models_used, call_count, \
                 total_input_tokens, total_output_tokens, status, \
                 final_finish_reason, user_input_preview, final_answer_preview, \
                 client_ip, server_ip, metadata, \
                 tool_surfaces_json, tool_call_total, agent_topology, suspicious_skills_json \
                 FROM agent_turns WHERE {where_sql} \
                 ORDER BY {sort_by} {sort_order} \
                 LIMIT {limit} OFFSET {offset}"
            );

            let mut items_stmt = conn
                .prepare(&items_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare items query: {e}")))?;

            let mut items = Vec::new();
            let mut query_rows = items_stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute items query: {e}")))?;

            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let models_used_raw: Option<String> = row
                    .get(8)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let models_used = parse_json_string_list(models_used_raw.as_deref());
                let primary_model = models_used.first().cloned();
                let metadata_raw: Option<String> = row
                    .get(18)
                    .map_err(|e| AppError::Storage(format!("read metadata: {e}")))?;
                let (proxy_role, proxy_peer_turn_id, proxy_peer_turn_ids) =
                    extract_proxy_fields(metadata_raw);
                let tool_surfaces_json: Option<String> = row
                    .get(19)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let tool_surfaces = parse_json_string_list(tool_surfaces_json.as_deref());
                let suspicious_skills_json: Option<String> = row
                    .get(22)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let suspicious_skills: Vec<serde_json::Value> = suspicious_skills_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                items.push(TurnListItem {
                    turn_id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    source_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    session_id: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    start_time: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    end_time: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    duration_ms: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    wire_api: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    agent_kind: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    client_ip: row
                        .get(16)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_ip: row
                        .get(17)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    primary_model,
                    models_used,
                    call_count: row
                        .get(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_input_tokens: row
                        .get(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_output_tokens: row
                        .get(11)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    status: row
                        .get(12)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    final_finish_reason: row
                        .get::<_, Option<String>>(13)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    user_input_preview: row
                        .get::<_, Option<String>>(14)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    final_answer_preview: row
                        .get::<_, Option<String>>(15)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    proxy_role,
                    proxy_peer_turn_id,
                    proxy_peer_turn_ids,
                    tool_surfaces,
                    tool_call_total: row
                        .get::<_, Option<u32>>(20)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?
                        .unwrap_or(0),
                    agent_topology: row
                        .get::<_, Option<String>>(21)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    suspicious_skills,
                });
            }

            Ok(TurnsPage { total, items })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_turn_by_id(&self, turn_id: &str) -> Result<Option<TurnDetail>> {
        let conn = self.read_pool.acquire().await?;
        let turn_id = turn_id.to_string();

        tokio::task::spawn_blocking(move || {
            let sql = "
                SELECT
                    turn_id, source_id, session_id, wire_api, agent_kind,
                    epoch_ms(start_time), epoch_ms(end_time), duration_ms, call_count,
                    models_used, subagents_used,
                    total_input_tokens, total_output_tokens,
                    total_cache_read_input_tokens, total_cache_creation_input_tokens,
                    total_cost_usd, status, final_finish_reason,
                    user_input_preview, user_call_id,
                    final_answer_preview, final_call_id,
                    call_ids, metadata,
                    client_ip, server_ip,
                    tool_surfaces_json, tool_call_total, agent_topology, suspicious_skills_json
                FROM agent_turns
                WHERE turn_id = ?
            ";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare turn_by_id query: {e}"))
            })?;

            #[allow(clippy::type_complexity)]
            let result = stmt.query_row(duckdb::params![turn_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,          // turn_id
                    row.get::<_, String>(1)?,          // source_id
                    row.get::<_, String>(2)?,          // session_id
                    row.get::<_, String>(3)?,          // wire_api
                    row.get::<_, String>(4)?,          // agent_kind
                    row.get::<_, i64>(5)?,             // start_time
                    row.get::<_, i64>(6)?,             // end_time
                    row.get::<_, u64>(7)?,             // duration_ms
                    row.get::<_, u32>(8)?,             // call_count
                    row.get::<_, Option<String>>(9)?,  // models_used
                    row.get::<_, Option<String>>(10)?, // subagents_used
                    row.get::<_, u64>(11)?,            // total_input_tokens
                    row.get::<_, u64>(12)?,            // total_output_tokens
                    row.get::<_, u64>(13)?,            // total_cache_read_input_tokens
                    row.get::<_, u64>(14)?,            // total_cache_creation_input_tokens
                    row.get::<_, Option<f64>>(15)?,    // total_cost_usd
                    row.get::<_, String>(16)?,         // status
                    row.get::<_, Option<String>>(17)?, // final_finish_reason
                    row.get::<_, Option<String>>(18)?, // user_input_preview
                    row.get::<_, Option<String>>(19)?, // user_call_id
                    row.get::<_, Option<String>>(20)?, // final_answer_preview
                    row.get::<_, Option<String>>(21)?, // final_call_id
                    row.get::<_, Option<String>>(22)?, // call_ids (JSON)
                    row.get::<_, Option<String>>(23)?, // metadata
                    row.get::<_, String>(24)?,         // client_ip
                    row.get::<_, String>(25)?,         // server_ip
                    row.get::<_, Option<String>>(26)?, // tool_surfaces_json
                    row.get::<_, Option<u32>>(27)?,    // tool_call_total
                    row.get::<_, Option<String>>(28)?, // agent_topology
                    row.get::<_, Option<String>>(29)?, // suspicious_skills_json
                ))
            });

            let tuple = match result {
                Ok(t) => t,
                Err(duckdb::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => {
                    return Err(AppError::Storage(format!(
                        "failed to query turn by id: {e}"
                    )));
                }
            };

            let (
                turn_id,
                source_id,
                session_id,
                wire_api,
                agent_kind,
                start_time,
                end_time,
                duration_ms,
                call_count,
                models_used_raw,
                subagents_used_raw,
                total_input_tokens,
                total_output_tokens,
                total_cache_read_input_tokens,
                total_cache_creation_input_tokens,
                total_cost_usd,
                status,
                final_finish_reason,
                user_input_preview,
                user_call_id,
                final_answer_preview,
                final_call_id,
                call_ids_raw,
                metadata_raw,
                client_ip,
                server_ip,
                tool_surfaces_json,
                tool_call_total_raw,
                agent_topology,
                suspicious_skills_json,
            ) = tuple;

            let models_used = parse_json_string_list(models_used_raw.as_deref());
            let subagents_used = parse_json_string_list(subagents_used_raw.as_deref());
            let call_ids = parse_json_string_list(call_ids_raw.as_deref());
            let metadata = metadata_raw
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
            let tool_surfaces = parse_json_string_list(tool_surfaces_json.as_deref());
            let tool_call_total = tool_call_total_raw.unwrap_or(0);
            let suspicious_skills: Vec<serde_json::Value> = suspicious_skills_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            // `truncate_preview` in h-turn appends `…` only when it truncates,
            // so a preview that does not end in `…` is already the full text —
            // skip the llm_calls lookup + profile re-extraction in that case.
            let user_input = match user_input_preview.as_deref() {
                Some(p) if !p.ends_with('…') => user_input_preview.clone(),
                _ => extract_full_text(
                    &conn,
                    &agent_kind,
                    user_call_id.as_deref(),
                    ExtractKind::User,
                )
                .or_else(|| user_input_preview.clone()),
            };
            let final_answer = match final_answer_preview.as_deref() {
                Some(p) if !p.ends_with('…') => final_answer_preview.clone(),
                _ => extract_full_text(
                    &conn,
                    &agent_kind,
                    final_call_id.as_deref(),
                    ExtractKind::Assistant,
                )
                .or_else(|| final_answer_preview.clone()),
            };

            Ok(Some(TurnDetail {
                turn_id,
                source_id,
                session_id,
                wire_api,
                agent_kind,
                client_ip,
                server_ip,
                start_time,
                end_time,
                duration_ms,
                call_count,
                models_used,
                subagents_used,
                total_input_tokens,
                total_output_tokens,
                total_cache_read_input_tokens,
                total_cache_creation_input_tokens,
                total_cost_usd,
                status,
                final_finish_reason,
                user_call_id,
                user_input,
                final_call_id,
                final_answer,
                call_ids,
                metadata,
                tool_surfaces,
                tool_call_total,
                agent_topology,
                suspicious_skills,
            }))
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    /// Light projection of `agent_turns` rows for pair-detection. Skips
    /// rows whose `metadata` already encodes a `proxy.role`.
    pub(crate) async fn query_pair_candidates(
        &self,
        start_us: i64,
        end_us: i64,
    ) -> Result<Vec<PairCandidate>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(start_us);
            let end_ts = us_to_timestamp(end_us);
            // metadata is stored as VARCHAR holding a JSON document.
            // `json_extract_string(metadata, '$.proxy.role')` returns the
            // string at that path, or NULL if absent. Filtering out rows
            // that already carry a role keeps repeat-sweeps idempotent.
            let sql = "
                SELECT turn_id, session_id, agent_kind, wire_api,
                       epoch_ms(start_time) * 1000 AS start_us,
                       epoch_ms(end_time) * 1000 AS end_us,
                       call_count,
                       total_input_tokens, total_output_tokens,
                       final_finish_reason,
                       models_used,
                       client_ip, server_ip
                  FROM agent_turns
                 WHERE start_time >= ?
                   AND start_time <  ?
                   AND (metadata IS NULL
                        OR json_extract_string(metadata, '$.proxy.role') IS NULL)
                 ORDER BY start_time ASC
            ";
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| AppError::Storage(format!("prepare pair candidates: {e}")))?;
            let mut rows = stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("query pair candidates: {e}")))?;
            let mut out = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let models_raw: Option<String> = row
                    .get(10)
                    .map_err(|e| AppError::Storage(format!("read models: {e}")))?;
                let models = parse_json_string_list(models_raw.as_deref());
                let primary_model = models.first().cloned();
                let client_ip: String = row
                    .get(11)
                    .map_err(|e| AppError::Storage(format!("read client_ip: {e}")))?;
                let server_ip: String = row
                    .get(12)
                    .map_err(|e| AppError::Storage(format!("read server_ip: {e}")))?;
                out.push(PairCandidate {
                    turn_id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read turn_id: {e}")))?,
                    session_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read session_id: {e}")))?,
                    agent_kind: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read agent_kind: {e}")))?,
                    wire_api: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read wire_api: {e}")))?,
                    start_time_us: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read start_us: {e}")))?,
                    end_time_us: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read end_us: {e}")))?,
                    call_count: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read call_count: {e}")))?,
                    total_input_tokens: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read in_tok: {e}")))?,
                    total_output_tokens: row
                        .get(8)
                        .map_err(|e| AppError::Storage(format!("read out_tok: {e}")))?,
                    final_finish_reason: row
                        .get::<_, Option<String>>(9)
                        .map_err(|e| AppError::Storage(format!("read finish: {e}")))?,
                    primary_model,
                    network_view: format!("{}->{}", client_ip, server_ip),
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    /// Read-modify-write `agent_turns.metadata` on a single row: merge
    /// `patch` (shallow, top-level key replacement) into the existing
    /// JSON object. No-op if `turn_id` doesn't exist — the sweeper may
    /// race finalization and a target turn can be momentarily absent.
    pub(crate) async fn update_turn_metadata(
        &self,
        turn_id: &str,
        patch: serde_json::Value,
    ) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        {
            use crate::fault_injection::FaultPoint;
            if self.fault_set.should_fire(FaultPoint::DuckDbInvalidate) {
                return Err(crate::fault_injection::fatal_invalidate_error());
            }
            if self.fault_set.should_fire(FaultPoint::DiskFull) {
                return Err(crate::fault_injection::disk_full_error());
            }
        }
        let conn = self.write_turns_conn.clone();
        let turn_id = turn_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let existing: Option<Option<String>> = conn
                .query_row(
                    "SELECT metadata FROM agent_turns WHERE turn_id = ?",
                    duckdb::params![turn_id],
                    |r| r.get(0),
                )
                .ok();
            let merged = match existing {
                Some(Some(text)) => {
                    let mut base = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .unwrap_or_else(|| serde_json::json!({}));
                    if !base.is_object() {
                        base = serde_json::json!({});
                    }
                    if let (Some(obj), Some(patch_obj)) = (base.as_object_mut(), patch.as_object())
                    {
                        for (k, v) in patch_obj {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                    base
                }
                Some(None) => patch,
                None => return Ok(()), // turn not present yet — drop silently
            };
            let merged_str = merged.to_string();
            conn.execute(
                "UPDATE agent_turns SET metadata = ? WHERE turn_id = ?",
                duckdb::params![merged_str, turn_id],
            )
            .map_err(|e| AppError::Storage(format!("update turn metadata: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use std::net::IpAddr;
    use h_llm::model::{ApiType, LlmCall};
    use h_llm::wire_apis as wa;
    use h_storage::query::*;
    use h_storage::StorageBackend;
    use h_turn::{AgentTurn, TurnStatus};

    fn sample_turn(
        turn_id: &str,
        session_id: &str,
        wire_api: &str,
        models_used: Vec<&str>,
        start_us: i64,
        duration_ms: u64,
        call_count: u32,
        call_ids: Vec<&str>,
        status: TurnStatus,
    ) -> AgentTurn {
        AgentTurn {
            source_id: String::new(),
            turn_id: turn_id.into(),
            session_id: session_id.into(),
            wire_api: wire_api.into(),
            agent_kind: "claude-cli".into(),
            client_ip: "127.0.0.1".parse().unwrap(),
            server_ip: "127.0.0.1".parse().unwrap(),
            start_time_us: start_us,
            end_time_us: start_us + (duration_ms as i64) * 1000,
            duration_ms,
            call_count,
            models_used: models_used.into_iter().map(String::from).collect(),
            subagents_used: vec![],
            total_input_tokens: 100,
            total_output_tokens: 50,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_cost_usd: None,
            status,
            final_finish_reason: Some("complete".into()),
            user_input_preview: Some("hello".into()),
            user_call_id: None,
            final_answer_preview: Some("world".into()),
            final_call_id: None,
            call_ids: call_ids.into_iter().map(String::from).collect(),
            metadata: serde_json::json!({}),
            tool_surfaces: vec![],
            tool_call_total: 0,
            agent_topology: None,
            suspicious_skills: vec![],
        }
    }

    fn mk_call_with_time(id: &str, request_time_us: i64) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: id.into(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".into(),
            api_type: ApiType::Chat,
            request_time: request_time_us,
            response_time: Some(request_time_us + 100_000),
            complete_time: Some(request_time_us + 500_000),
            request_path: "/v1/chat/completions".into(),
            is_stream: false,
            request_body: None,
            status_code: Some(200),
            finish_reason: Some("stop".to_string()),
            response_body: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: Some(100.0),
            e2e_latency_ms: Some(500.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 1000,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 8080,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
            body_bytes_dropped: 0,
        }
    }

    #[tokio::test]
    async fn round_trip_one_turn() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        let turn = sample_turn(
            "t1",
            "s1",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            1_700_000_000_000_000,
            1500,
            3,
            vec!["call-42"],
            TurnStatus::Complete,
        );
        backend.write_turns(vec![turn]).await.unwrap();
    }

    fn base_turns_query() -> TurnsQuery {
        TurnsQuery {
            time_range: TimeRange {
                start_us: 1_700_000_000_000_000 - 1,
                end_us: 1_800_000_000_000_000,
            },
            filter: DimensionFilter::default(),
            client_ips: vec![],
            server_ports: vec![],
            statuses: vec![],
            agent_kinds: vec![],
            sort_by: "start_time".into(),
            sort_order: "desc".into(),
            page: 1,
            page_size: 50,
            include_proxy_hops: false,
        }
    }

    #[tokio::test]
    async fn query_turns_filters_and_paginates() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let base = 1_700_000_000_000_000_i64;
        let turns = vec![
            sample_turn(
                "t1",
                "s1",
                wa::OPENAI_CHAT,
                vec!["gpt-4"],
                base + 1_000_000,
                100,
                1,
                vec!["c1"],
                TurnStatus::Complete,
            ),
            sample_turn(
                "t2",
                "s1",
                wa::ANTHROPIC,
                vec!["claude-sonnet"],
                base + 2_000_000,
                200,
                2,
                vec!["c2", "c3"],
                TurnStatus::Complete,
            ),
            sample_turn(
                "t3",
                "s2",
                wa::OPENAI_CHAT,
                vec!["gpt-4o"],
                base + 3_000_000,
                300,
                3,
                vec!["c4"],
                TurnStatus::Incomplete,
            ),
            sample_turn(
                "t4",
                "s3",
                wa::OPENAI_CHAT,
                vec!["gpt-4", "gpt-4o"],
                base + 4_000_000,
                400,
                4,
                vec!["c5"],
                TurnStatus::Complete,
            ),
        ];
        backend.write_turns(turns).await.unwrap();

        // No filter: all 4 turns, default sort_by=start_time DESC
        let page = backend.query_turns(&base_turns_query()).await.unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(page.items.len(), 4);
        assert_eq!(page.items[0].turn_id, "t4");
        assert_eq!(page.items[3].turn_id, "t1");
        assert_eq!(page.items[0].primary_model.as_deref(), Some("gpt-4"));
        assert_eq!(page.items[0].models_used, vec!["gpt-4", "gpt-4o"]);

        // wire_api filter
        let mut q = base_turns_query();
        q.filter.wire_apis = vec![wa::ANTHROPIC.into()];
        let page = backend.query_turns(&q).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].turn_id, "t2");

        // Model filter via list_has_any — should include t1 and t4 (both list gpt-4)
        let mut q = base_turns_query();
        q.filter.models = vec!["gpt-4".into()];
        let page = backend.query_turns(&q).await.unwrap();
        assert_eq!(page.total, 2);
        let ids: Vec<_> = page.items.iter().map(|t| t.turn_id.clone()).collect();
        assert!(ids.contains(&"t1".to_string()));
        assert!(ids.contains(&"t4".to_string()));

        // Status filter (TurnStatus Display: incomplete)
        let mut q = base_turns_query();
        q.statuses = vec!["incomplete".into()];
        let page = backend.query_turns(&q).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].turn_id, "t3");

        // Sort by duration_ms ASC
        let mut q = base_turns_query();
        q.sort_by = "duration_ms".into();
        q.sort_order = "asc".into();
        let page = backend.query_turns(&q).await.unwrap();
        let durations: Vec<_> = page.items.iter().map(|t| t.duration_ms).collect();
        assert_eq!(durations, vec![100, 200, 300, 400]);

        // Pagination
        let mut q = base_turns_query();
        q.page_size = 2;
        q.page = 1;
        let page1 = backend.query_turns(&q).await.unwrap();
        assert_eq!(page1.total, 4);
        assert_eq!(page1.items.len(), 2);
        q.page = 2;
        let page2 = backend.query_turns(&q).await.unwrap();
        assert_eq!(page2.items.len(), 2);
        assert_ne!(page1.items[0].turn_id, page2.items[0].turn_id);

        // Invalid sort field is rejected
        let mut q = base_turns_query();
        q.sort_by = "bogus".into();
        assert!(backend.query_turns(&q).await.is_err());
    }

    #[tokio::test]
    async fn query_turn_by_id_hit_and_miss() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let turn = sample_turn(
            "t-detail",
            "s1",
            wa::ANTHROPIC,
            vec!["claude-sonnet", "claude-haiku"],
            1_700_000_000_000_000,
            1500,
            2,
            vec!["call-a", "call-b"],
            TurnStatus::Complete,
        );
        backend.write_turns(vec![turn]).await.unwrap();

        let hit = backend.query_turn_by_id("t-detail").await.unwrap();
        let d = hit.expect("turn exists");
        assert_eq!(d.turn_id, "t-detail");
        assert_eq!(d.models_used, vec!["claude-sonnet", "claude-haiku"]);
        assert_eq!(d.call_ids, vec!["call-a", "call-b"]);
        // With no user_call_id/final_call_id, full text falls back to previews.
        assert_eq!(d.user_input.as_deref(), Some("hello"));
        assert_eq!(d.final_answer.as_deref(), Some("world"));

        let miss = backend.query_turn_by_id("does-not-exist").await.unwrap();
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn query_turn_by_id_skips_calls_lookup_when_preview_complete() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // A matching llm_calls row exists with full-body text that differs from
        // the preview. If the optimization works, we return the preview
        // (short, no trailing `…`) and never touch the body.
        let base = 1_700_000_000_000_000_i64;
        let mut user_call = mk_call_with_time("c-user", base + 1_000);
        user_call.wire_api = wa::ANTHROPIC;
        user_call.request_body =
            Some(r#"{"messages":[{"role":"user","content":"DB-USER-FULL"}]}"#.into());
        let mut asst_call = mk_call_with_time("c-asst", base + 2_000);
        asst_call.wire_api = wa::ANTHROPIC;
        asst_call.response_body =
            Some(r#"{"content":[{"type":"text","text":"DB-ASSISTANT-FULL"}]}"#.into());
        backend
            .write_calls(vec![user_call, asst_call])
            .await
            .unwrap();

        let mut turn = sample_turn(
            "t-short",
            "s-short",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            base,
            1500,
            2,
            vec!["c-user", "c-asst"],
            TurnStatus::Complete,
        );
        turn.user_input_preview = Some("hi".into());
        turn.user_call_id = Some("c-user".into());
        turn.final_answer_preview = Some("bye".into());
        turn.final_call_id = Some("c-asst".into());
        backend.write_turns(vec![turn]).await.unwrap();

        let d = backend
            .query_turn_by_id("t-short")
            .await
            .unwrap()
            .expect("turn exists");
        // Preview is returned as-is; no llm_calls lookup happened.
        assert_eq!(d.user_input.as_deref(), Some("hi"));
        assert_eq!(d.final_answer.as_deref(), Some("bye"));
    }

    #[tokio::test]
    async fn query_turn_by_id_reads_full_text_when_preview_truncated() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Truncated previews (ending in `…`) must fall through to the llm_calls
        // lookup and return the full body text.
        let base = 1_700_000_000_000_000_i64;
        let full_user: String = "u".repeat(600);
        let full_asst: String = "a".repeat(600);
        let mut user_call = mk_call_with_time("c-user", base + 1_000);
        user_call.wire_api = wa::ANTHROPIC;
        user_call.request_body = Some(
            serde_json::json!({
                "messages": [{ "role": "user", "content": &full_user }]
            })
            .to_string(),
        );
        let mut asst_call = mk_call_with_time("c-asst", base + 2_000);
        asst_call.wire_api = wa::ANTHROPIC;
        asst_call.response_body = Some(
            serde_json::json!({
                "content": [{ "type": "text", "text": &full_asst }]
            })
            .to_string(),
        );
        backend
            .write_calls(vec![user_call, asst_call])
            .await
            .unwrap();

        let truncated_user: String = "u".repeat(500) + "…";
        let truncated_asst: String = "a".repeat(500) + "…";
        let mut turn = sample_turn(
            "t-long",
            "s-long",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            base,
            1500,
            2,
            vec!["c-user", "c-asst"],
            TurnStatus::Complete,
        );
        turn.user_input_preview = Some(truncated_user);
        turn.user_call_id = Some("c-user".into());
        turn.final_answer_preview = Some(truncated_asst);
        turn.final_call_id = Some("c-asst".into());
        backend.write_turns(vec![turn]).await.unwrap();

        let d = backend
            .query_turn_by_id("t-long")
            .await
            .unwrap()
            .expect("turn exists");
        assert_eq!(d.user_input.as_deref(), Some(full_user.as_str()));
        assert_eq!(d.final_answer.as_deref(), Some(full_asst.as_str()));
    }

    #[tokio::test]
    async fn query_turn_calls_orders_and_sequences() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let base = 1_700_000_000_000_000_i64;
        // Insert calls out of chronological order to confirm ORDER BY works.
        let calls = vec![
            mk_call_with_time("call-b", base + 2_000_000),
            mk_call_with_time("call-a", base + 1_000_000),
            mk_call_with_time("call-c", base + 3_000_000),
            // Extra call not in the turn's call_ids — must be excluded.
            mk_call_with_time("call-other", base + 500_000),
        ];
        backend.write_calls(calls).await.unwrap();

        let turn = sample_turn(
            "t-calls",
            "s1",
            wa::OPENAI_CHAT,
            vec!["gpt-4"],
            base,
            3000,
            3,
            vec!["call-a", "call-b", "call-c"],
            TurnStatus::Complete,
        );
        backend.write_turns(vec![turn]).await.unwrap();

        let items = backend.query_turn_calls("t-calls", true).await.unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "call-a");
        assert_eq!(items[0].sequence, 1);
        assert_eq!(items[1].id, "call-b");
        assert_eq!(items[1].sequence, 2);
        assert_eq!(items[2].id, "call-c");
        assert_eq!(items[2].sequence, 3);
        assert!(items[0].request_time < items[1].request_time);

        // Lite mode strips bodies/headers but keeps every other field
        // identical. Use this same fixture to verify the contract: ids,
        // sequence, timing, etc. all match; only the 4 heavy fields
        // come back as None.
        let lite = backend.query_turn_calls("t-calls", false).await.unwrap();
        assert_eq!(lite.len(), 3);
        for (full, lite) in items.iter().zip(lite.iter()) {
            assert_eq!(full.id, lite.id);
            assert_eq!(full.sequence, lite.sequence);
            assert_eq!(full.input_tokens, lite.input_tokens);
            assert_eq!(full.output_tokens, lite.output_tokens);
            assert!(lite.request_body.is_none());
            assert!(lite.response_body.is_none());
            assert!(lite.request_headers.is_none());
            assert!(lite.response_headers.is_none());
        }

        // Unknown turn → empty vec (not error).
        let empty = backend
            .query_turn_calls("no-such-turn", true)
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    fn sample_turn_for_session(
        turn_id: &str,
        session_id: &str,
        start_us: i64,
        user_input: Option<&str>,
    ) -> AgentTurn {
        let mut t = sample_turn(
            turn_id,
            session_id,
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            start_us,
            500,
            1,
            vec![turn_id],
            TurnStatus::Complete,
        );
        t.user_input_preview = user_input.map(String::from);
        t.user_call_id = user_input.map(|_| format!("call-{turn_id}"));
        t
    }

    #[tokio::test]
    async fn query_sessions_window_filters_and_aggregates_full_lifetime() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Three sessions, each with multiple turns spread over time.
        //   S1: turns at t=10, t=50 (lifetime 10..50, middle turn in window 40..60)
        //   S2: turns at t=30, t=45 (both in window)
        //   S3: turns at t=100, t=200 (out of window)
        let base = 1_700_000_000_000_000_i64;
        let us = |secs: i64| base + secs * 1_000_000;
        backend
            .write_turns(vec![
                sample_turn_for_session("t1a", "S1", us(10), Some("first S1")),
                sample_turn_for_session("t1b", "S1", us(50), None),
                sample_turn_for_session("t2a", "S2", us(30), Some("first S2")),
                sample_turn_for_session("t2b", "S2", us(45), None),
                sample_turn_for_session("t3a", "S3", us(100), Some("first S3")),
                sample_turn_for_session("t3b", "S3", us(200), None),
            ])
            .await
            .unwrap();

        // Window [40, 60). S3 entirely out, so excluded. S1 has t=50 in window.
        // S2 has t=45 in window. Both S1 and S2 should return full-lifetime aggregates.
        let page = backend
            .query_sessions(&SessionListQuery {
                time_range: TimeRange {
                    start_us: us(40),
                    end_us: us(60),
                },
                source_id: None,
                agent_kinds: vec![],
                cursor: None,
                page_size: 10,
            })
            .await
            .unwrap();

        assert_eq!(page.items.len(), 2);
        // Sort key is MAX(end_time_in_window) DESC. S1's in-window turn ends
        // latest (t=50 + 500ms), so S1 should be first.
        let s1 = &page.items[0];
        assert_eq!(s1.session_id, "S1");
        assert_eq!(s1.turn_count, 2); // full lifetime: both turns counted
        assert_eq!(s1.first_user_input_preview.as_deref(), Some("first S1"));
        // first_turn_at should be the lifetime's MIN(start_time), not the
        // in-window one. S1's earliest turn is at t=10.
        assert_eq!(s1.first_turn_at, (us(10)) / 1000);

        let s2 = &page.items[1];
        assert_eq!(s2.session_id, "S2");
        assert_eq!(s2.turn_count, 2);
        assert_eq!(s2.first_user_input_preview.as_deref(), Some("first S2"));

        assert!(page.next_cursor.is_none());

        // Page size 1 + cursor roundtrip.
        let p1 = backend
            .query_sessions(&SessionListQuery {
                time_range: TimeRange {
                    start_us: us(40),
                    end_us: us(60),
                },
                source_id: None,
                agent_kinds: vec![],
                cursor: None,
                page_size: 1,
            })
            .await
            .unwrap();
        assert_eq!(p1.items.len(), 1);
        assert_eq!(p1.items[0].session_id, "S1");
        let cursor = p1.next_cursor.expect("has next page");
        let decoded = decode_session_cursor(&cursor).expect("cursor decodes");

        let p2 = backend
            .query_sessions(&SessionListQuery {
                time_range: TimeRange {
                    start_us: us(40),
                    end_us: us(60),
                },
                source_id: None,
                agent_kinds: vec![],
                cursor: Some(decoded),
                page_size: 1,
            })
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 1);
        assert_eq!(p2.items[0].session_id, "S2");
        assert!(p2.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_session_by_id_and_turns_roundtrip() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let base = 1_700_000_000_000_000_i64;
        let us = |secs: i64| base + secs * 1_000_000;
        backend
            .write_turns(vec![
                sample_turn_for_session("ta", "SX", us(10), Some("opener")),
                sample_turn_for_session("tb", "SX", us(20), None),
                sample_turn_for_session("tc", "SX", us(30), None),
            ])
            .await
            .unwrap();

        let d = backend
            .query_session_by_id("", "SX")
            .await
            .unwrap()
            .expect("session exists");
        assert_eq!(d.session_id, "SX");
        assert_eq!(d.turn_count, 3);
        assert_eq!(d.first_user_input_preview.as_deref(), Some("opener"));

        let miss = backend.query_session_by_id("", "ZZZ").await.unwrap();
        assert!(miss.is_none());

        // Turns list: ordered by start_time DESC.
        let turns = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "SX".into(),
                cursor: None,
                page_size: 10,
            })
            .await
            .unwrap();
        assert_eq!(turns.items.len(), 3);
        assert_eq!(turns.items[0].turn_id, "tc");
        assert_eq!(turns.items[2].turn_id, "ta");
        // Fewer rows than page_size → no next page.
        assert!(turns.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_session_turns_cursor_pagination() {
        use h_storage::query::decode_session_turns_cursor;

        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Seed 5 turns in session "S-CURSOR" with strictly increasing start_time.
        // Short previews (no `…`) keep this test purely about cursor mechanics —
        // no full-text extraction round-trip is triggered.
        let base = 1_700_000_000_000_000_i64;
        let us = |secs: i64| base + secs * 1_000_000;
        let turns: Vec<AgentTurn> = (0..5)
            .map(|i| {
                sample_turn_for_session(
                    &format!("turn-{i}"),
                    "S-CURSOR",
                    us(i as i64 * 10),
                    Some("hi"),
                )
            })
            .collect();
        backend.write_turns(turns).await.unwrap();

        // Page 1: newest 2 (turn-4, turn-3).
        let p1 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-CURSOR".into(),
                cursor: None,
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p1.items.len(), 2);
        assert_eq!(p1.items[0].turn_id, "turn-4");
        assert_eq!(p1.items[1].turn_id, "turn-3");
        let cursor1 = p1.next_cursor.expect("more pages");

        // Page 2: turn-2, turn-1.
        let p2 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-CURSOR".into(),
                cursor: decode_session_turns_cursor(&cursor1),
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 2);
        assert_eq!(p2.items[0].turn_id, "turn-2");
        assert_eq!(p2.items[1].turn_id, "turn-1");
        let cursor2 = p2.next_cursor.expect("more pages");

        // Page 3: turn-0, no next cursor.
        let p3 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-CURSOR".into(),
                cursor: decode_session_turns_cursor(&cursor2),
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p3.items.len(), 1);
        assert_eq!(p3.items[0].turn_id, "turn-0");
        assert!(p3.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_session_turns_extracts_full_text_when_preview_truncated() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Build llm_calls carrying real bodies that the Anthropic profile
        // extractor can parse. Bodies must be long enough that the preview is
        // `…`-terminated (i.e. > 500 chars so the stored preview is truncated).
        let base = 1_700_000_000_000_000_i64;
        let full_user: String = "u".repeat(600);
        let full_asst: String = "a".repeat(600);

        let mut user_call = mk_call_with_time("sc-user", base + 1_000);
        user_call.wire_api = wa::ANTHROPIC;
        user_call.request_body = Some(
            serde_json::json!({
                "messages": [{ "role": "user", "content": &full_user }]
            })
            .to_string(),
        );

        let mut asst_call = mk_call_with_time("sc-asst", base + 2_000);
        asst_call.wire_api = wa::ANTHROPIC;
        asst_call.response_body = Some(
            serde_json::json!({
                "content": [{ "type": "text", "text": &full_asst }]
            })
            .to_string(),
        );
        backend
            .write_calls(vec![user_call, asst_call])
            .await
            .unwrap();

        // Turn with `…`-terminated previews pointing at the call ids above.
        let truncated_user: String = "u".repeat(500) + "…";
        let truncated_asst: String = "a".repeat(500) + "…";
        let mut turn = sample_turn(
            "st-long",
            "S-EXTRACT",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            base,
            1500,
            2,
            vec!["sc-user", "sc-asst"],
            TurnStatus::Complete,
        );
        turn.user_input_preview = Some(truncated_user);
        turn.user_call_id = Some("sc-user".into());
        turn.final_answer_preview = Some(truncated_asst);
        turn.final_call_id = Some("sc-asst".into());
        backend.write_turns(vec![turn]).await.unwrap();

        let page = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-EXTRACT".into(),
                cursor: None,
                page_size: 10,
            })
            .await
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].user_input.as_deref(),
            Some(full_user.as_str()),
            "user_input should be full text, not truncated preview"
        );
        assert_eq!(
            page.items[0].final_answer.as_deref(),
            Some(full_asst.as_str()),
            "final_answer should be full text, not truncated preview"
        );
    }

    #[tokio::test]
    async fn query_pair_candidates_returns_only_unpaired() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        let base = 1_700_000_000_000_000_i64;
        let t1 = sample_turn(
            "t1",
            "s1",
            wa::ANTHROPIC,
            vec!["claude"],
            base,
            1000,
            1,
            vec!["c1"],
            TurnStatus::Complete,
        );
        let mut t2 = sample_turn(
            "t2",
            "s1",
            wa::ANTHROPIC,
            vec!["claude"],
            base + 1000,
            1000,
            1,
            vec!["c2"],
            TurnStatus::Complete,
        );
        // t2 already paired — sweeper should skip it.
        t2.metadata = serde_json::json!({
            "proxy": {
                "role": "proxy_in",
                "pair_id": "p-existing",
                "peer_turn_id": "tX",
            }
        });
        backend.write_turns(vec![t1, t2]).await.unwrap();

        let cands = backend
            .query_pair_candidates(base - 1, base + 2_000_000_000)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].turn_id, "t1");
        assert_eq!(cands[0].session_id, "s1");
        assert_eq!(cands[0].network_view, "127.0.0.1->127.0.0.1");
        assert_eq!(cands[0].total_input_tokens, 100);
    }

    #[tokio::test]
    async fn update_turn_metadata_merges_into_existing_object() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        let mut turn = sample_turn(
            "tA",
            "sA",
            wa::ANTHROPIC,
            vec!["claude"],
            1_700_000_000_000_000,
            1500,
            1,
            vec!["c1"],
            TurnStatus::Complete,
        );
        // Pre-existing metadata key — must survive the patch.
        turn.metadata = serde_json::json!({"unrelated": "preserve_me"});
        backend.write_turns(vec![turn]).await.unwrap();

        let patch = serde_json::json!({
            "proxy": {
                "role": "proxy_in",
                "pair_id": "p-1",
                "peer_turn_id": "tB",
            }
        });
        backend.update_turn_metadata("tA", patch).await.unwrap();

        let detail = backend.query_turn_by_id("tA").await.unwrap().unwrap();
        let meta = detail.metadata.expect("metadata json");
        assert_eq!(
            meta.get("unrelated"),
            Some(&serde_json::Value::String("preserve_me".into()))
        );
        assert_eq!(meta["proxy"]["role"], "proxy_in");
        assert_eq!(meta["proxy"]["peer_turn_id"], "tB");
    }

    #[tokio::test]
    async fn query_turns_hides_proxy_hops_by_default_and_surfaces_them_with_flag() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        let base = 1_700_000_000_000_000_i64;
        // proxy_in (visible by default) + proxy_out (hidden by default)
        // + a direct turn (always visible).
        let mut t_in = sample_turn(
            "t_in",
            "s",
            wa::ANTHROPIC,
            vec!["claude"],
            base,
            1500,
            1,
            vec!["c_in"],
            TurnStatus::Complete,
        );
        t_in.metadata = serde_json::json!({
            "proxy": {"role": "proxy_in", "pair_id": "p1", "peer_turn_id": "t_out"}
        });
        let mut t_out = sample_turn(
            "t_out",
            "s",
            wa::ANTHROPIC,
            vec!["claude"],
            base + 2_000,
            1500,
            1,
            vec!["c_out"],
            TurnStatus::Complete,
        );
        t_out.metadata = serde_json::json!({
            "proxy": {"role": "proxy_out", "pair_id": "p1", "peer_turn_id": "t_in"}
        });
        let t_direct = sample_turn(
            "t_direct",
            "s2",
            wa::ANTHROPIC,
            vec!["claude"],
            base + 10_000_000,
            1500,
            1,
            vec!["c_d"],
            TurnStatus::Complete,
        );
        backend
            .write_turns(vec![t_in.clone(), t_out.clone(), t_direct.clone()])
            .await
            .unwrap();

        // Default — proxy_out must be hidden.
        let mut q = base_turns_query();
        q.time_range.start_us = base - 1;
        q.time_range.end_us = base + 1_000_000_000;
        q.include_proxy_hops = false;
        let page = backend.query_turns(&q).await.unwrap();
        let ids: Vec<String> = page.items.iter().map(|i| i.turn_id.clone()).collect();
        assert!(ids.contains(&"t_in".to_string()));
        assert!(ids.contains(&"t_direct".to_string()));
        assert!(
            !ids.contains(&"t_out".to_string()),
            "proxy_out must be hidden by default"
        );
        assert_eq!(page.total, 2);
        // proxy_in row carries the role + peer_turn_id fields.
        let in_item = page.items.iter().find(|i| i.turn_id == "t_in").unwrap();
        assert_eq!(in_item.proxy_role.as_deref(), Some("proxy_in"));
        assert_eq!(in_item.proxy_peer_turn_id.as_deref(), Some("t_out"));
        // Direct row has no proxy fields.
        let d_item = page.items.iter().find(|i| i.turn_id == "t_direct").unwrap();
        assert_eq!(d_item.proxy_role, None);
        assert_eq!(d_item.proxy_peer_turn_id, None);

        // Flag flipped — every row is returned including proxy_out.
        q.include_proxy_hops = true;
        let page = backend.query_turns(&q).await.unwrap();
        let ids: Vec<String> = page.items.iter().map(|i| i.turn_id.clone()).collect();
        assert!(ids.contains(&"t_out".to_string()));
        assert_eq!(page.total, 3);
    }

    #[tokio::test]
    async fn update_turn_metadata_is_noop_when_turn_absent() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        // No turn written; update must succeed silently.
        let patch = serde_json::json!({"proxy": {"role": "proxy_in"}});
        backend
            .update_turn_metadata("never-existed", patch)
            .await
            .expect("noop on missing row");
    }
}
