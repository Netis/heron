use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use ts_storage::query::{TurnListItem, TurnsQuery};
use ts_turn::AgentTurn;

use crate::extractors::{Path, Query};
use crate::params::*;
use crate::response::{ApiError, ApiResponse};
use crate::ApiAgentTurnsContext;

#[derive(Debug, Deserialize)]
pub struct TurnsParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub client_ip: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub agent_kind: Option<String>,
    #[serde(default = "default_turns_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_turns_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_turns_sort_by() -> String {
    "start_time".to_string()
}
fn default_turns_sort_order() -> String {
    "desc".to_string()
}
fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    50
}

pub async fn list(
    State(ctx): State<ApiAgentTurnsContext>,
    Query(params): Query<TurnsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let page_size = params.page_size.min(200);

    let query = TurnsQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        client_ips: parse_csv(&params.client_ip),
        statuses: parse_csv(&params.status),
        agent_kinds: parse_csv(&params.agent_kind),
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        page: params.page,
        page_size,
    };

    let mut page = ctx.storage.query_turns(&query).await?;

    // Snapshot the in-memory active-turn registry, filter by the same
    // params the SQL query used, and prepend matching rows to page 1 so
    // the console shows in-progress turns alongside finalized ones in a
    // single list. The DB stays terminal-only — no write amplification.
    let in_progress = collect_in_progress(&ctx, &query);
    let in_progress_count = in_progress.len() as u64;

    if params.page == 1 && !in_progress.is_empty() {
        // Always-newest-first: in-progress turns are by definition
        // currently in flight, so they sort to the head of `start_time desc`
        // (the default). Even with a custom sort, listing them first is the
        // "live tip" semantic we want.
        let mut merged = in_progress;
        merged.extend(std::mem::take(&mut page.items));
        page.items = merged;
    }
    // total counts the union: DB rows + in-progress rows (the registry's
    // entries are not in any DB page yet). Pagination math overestimates
    // by at most `in_progress_count` rows (~ # active sessions, typically
    // <= 100), which the UI renders as "maybe one extra empty page" — an
    // acceptable trade for not having to rewrite the page boundary logic.
    page.total = page.total.saturating_add(in_progress_count);
    Ok(ApiResponse::ok(page))
}

/// Read the active-turn registry under its read lock, filter by the
/// query's time range, dimension filter, status, agent_kind, and
/// client_ip lists, and convert the survivors to `TurnListItem`s.
fn collect_in_progress(ctx: &ApiAgentTurnsContext, query: &TurnsQuery) -> Vec<TurnListItem> {
    let map = match ctx.active_turns.read() {
        Ok(g) => g,
        Err(_) => return Vec::new(), // poisoned lock — degrade to empty
    };
    let start_us = query.time_range.start_us;
    let end_us = query.time_range.end_us;
    let filter = &query.filter;

    let mut out: Vec<TurnListItem> = map
        .values()
        .filter(|t| {
            // Time-range overlap with the requested window. An in-progress
            // turn is "live" between start_time_us and end_time_us
            // (whatever its latest call's complete_time was). Match if any
            // part of [start_time_us, end_time_us] overlaps the query window.
            t.end_time_us >= start_us && t.start_time_us <= end_us
        })
        .filter(|t| matches_filter(t, filter))
        .filter(|t| {
            // status filter: in-progress rows match only if `in_progress`
            // is in the requested set, OR if the filter is empty (= all).
            query.statuses.is_empty() || query.statuses.iter().any(|s| s == "in_progress")
        })
        .filter(|t| {
            query.agent_kinds.is_empty() || query.agent_kinds.iter().any(|k| k == &t.agent_kind)
        })
        .filter(|t| {
            query.client_ips.is_empty()
                || query
                    .client_ips
                    .iter()
                    .any(|ip| ip == &t.client_ip.to_string())
        })
        .map(agent_turn_to_list_item)
        .collect();

    // Most-recent first by start_time_us — matches the default UI sort.
    out.sort_by(|a, b| b.start_time.cmp(&a.start_time));
    out
}

/// Convert a tracker-side `AgentTurn` to the API list response shape.
fn agent_turn_to_list_item(t: &AgentTurn) -> TurnListItem {
    TurnListItem {
        turn_id: t.turn_id.clone(),
        source_id: t.source_id.clone(),
        session_id: t.session_id.clone(),
        start_time: t.start_time_us,
        end_time: t.end_time_us,
        duration_ms: t.duration_ms,
        wire_api: t.wire_api.clone(),
        agent_kind: t.agent_kind.clone(),
        client_ip: t.client_ip.to_string(),
        server_ip: t.server_ip.to_string(),
        primary_model: t.models_used.first().cloned(),
        models_used: t.models_used.clone(),
        call_count: t.call_count,
        total_input_tokens: t.total_input_tokens,
        total_output_tokens: t.total_output_tokens,
        status: t.status.to_string(),
        final_finish_reason: t.final_finish_reason.clone(),
        user_input_preview: t.user_input_preview.clone(),
        final_answer_preview: t.final_answer_preview.clone(),
    }
}

/// Wire-api / model / server-ip dimension filter — mirrors
/// `query_turns`'s SQL WHERE clause. Each list is an OR group; an empty
/// list means "any". The turn matches if for every non-empty list at
/// least one entry matches the corresponding turn field (with `model`
/// matching against the turn's `models_used` set since a turn can span
/// several models).
fn matches_filter(t: &AgentTurn, f: &ts_storage::query::DimensionFilter) -> bool {
    if !f.wire_apis.is_empty() && !f.wire_apis.iter().any(|w| w == &t.wire_api) {
        return false;
    }
    if !f.models.is_empty() && !f.models.iter().any(|m| t.models_used.iter().any(|tm| tm == m)) {
        return false;
    }
    let server_ip_str = t.server_ip.to_string();
    if !f.server_ips.is_empty() && !f.server_ips.iter().any(|ip| ip == &server_ip_str) {
        return false;
    }
    true
}

pub async fn detail(
    State(ctx): State<ApiAgentTurnsContext>,
    Path(turn_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // First look in the in-memory registry — for in-progress turns
    // there is no DB row yet; the snapshot is the only place where the
    // turn detail lives.
    if let Ok(map) = ctx.active_turns.read() {
        if let Some(t) = map.get(&turn_id) {
            return Ok(ApiResponse::ok(agent_turn_to_detail(t.clone())));
        }
    }
    match ctx.storage.query_turn_by_id(&turn_id).await? {
        Some(detail) => Ok(ApiResponse::ok(detail)),
        None => Err(ApiError::NotFound(format!("turn not found: {turn_id}"))),
    }
}

/// Convert a snapshot `AgentTurn` to a `TurnDetail`-shaped payload. The
/// DB-side `query_turn_by_id` returns a richer `TurnDetail` that pulls
/// full bodies from referenced `llm_calls` rows; for in-progress turns
/// we don't have those joins, so the previews and counts are returned
/// as-is. The frontend tolerates the lighter payload (preview-only).
fn agent_turn_to_detail(t: AgentTurn) -> ts_storage::query::TurnDetail {
    use ts_storage::query::TurnDetail;
    TurnDetail {
        turn_id: t.turn_id,
        source_id: t.source_id,
        session_id: t.session_id,
        wire_api: t.wire_api,
        agent_kind: t.agent_kind,
        client_ip: t.client_ip.to_string(),
        server_ip: t.server_ip.to_string(),
        start_time: t.start_time_us,
        end_time: t.end_time_us,
        duration_ms: t.duration_ms,
        call_count: t.call_count,
        models_used: t.models_used,
        subagents_used: t.subagents_used,
        total_input_tokens: t.total_input_tokens,
        total_output_tokens: t.total_output_tokens,
        total_cache_read_input_tokens: t.total_cache_read_input_tokens,
        total_cache_creation_input_tokens: t.total_cache_creation_input_tokens,
        total_cost_usd: t.total_cost_usd,
        status: t.status.to_string(),
        final_finish_reason: t.final_finish_reason,
        user_call_id: t.user_call_id,
        user_input: t.user_input_preview,
        final_call_id: t.final_call_id,
        final_answer: t.final_answer_preview,
        call_ids: t.call_ids,
        metadata: Some(t.metadata),
    }
}

pub async fn calls(
    State(ctx): State<ApiAgentTurnsContext>,
    Path(turn_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // In-progress turns: pull call_ids from the in-memory registry
    // snapshot, then ask storage to fetch the matching `llm_calls`
    // rows. A call may be ingested into the tracker microseconds before
    // its row gets flushed from `WriteBuffer` to DuckDB; in that
    // narrow window the call is missing from the result and shows up on
    // the next refresh. Total lag is bounded by storage.flush_interval_ms
    // (200 ms after PR #5).
    let in_progress_call_ids: Option<Vec<String>> = ctx
        .active_turns
        .read()
        .ok()
        .and_then(|map| map.get(&turn_id).map(|t| t.call_ids.clone()));

    if let Some(call_ids) = in_progress_call_ids {
        let items = ctx.storage.query_calls_by_ids(&call_ids).await?;
        return Ok(ApiResponse::ok(items));
    }
    let items = ctx.storage.query_turn_calls(&turn_id).await?;
    Ok(ApiResponse::ok(items))
}
