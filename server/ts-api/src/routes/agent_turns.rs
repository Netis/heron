use std::collections::BTreeMap;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use ts_storage::query::{
    AgentActivityPoint, AgentActivityQuery, AgentKindSummary, AgentSummaryQuery, CallDetail,
    HeaderDiffEntry, HeaderDiffKind, HeaderValueByLeg, LatencyBreakdown, ModelRewrite,
    ProxyViewMember, ProxyViewResponse, TurnDetail, TurnListItem, TurnsQuery,
};
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
    /// CSV of u16 server ports. Resolved through the turn's first
    /// call_id against `llm_calls.server_port`.
    #[serde(default)]
    pub server_port: Option<String>,
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
    /// When true, return turns the pair sweeper has hidden (proxy_out /
    /// mirror_secondary). Default false — the list folds duplicates by
    /// default so the user sees one row per logical call.
    #[serde(default)]
    pub include_proxy_hops: bool,
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

    let server_ports: Vec<u16> = parse_csv(&params.server_port)
        .iter()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| ApiError::InvalidParam(format!("invalid server_port: {s}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let query = TurnsQuery {
        time_range: to_time_range(params.start, params.end)?,
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        client_ips: parse_csv(&params.client_ip),
        server_ports,
        statuses: parse_csv(&params.status),
        agent_kinds: parse_csv(&params.agent_kind),
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        page: params.page,
        page_size,
        include_proxy_hops: params.include_proxy_hops,
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
        .filter(|_t| {
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
/// In-progress turns never have a pair annotation yet — the sweeper
/// only inspects finalized rows in the DB — so `proxy_role` /
/// `proxy_peer_turn_id` are always `None` here. Once the turn
/// finalizes and the sweeper sees it, the DB row carries the role.
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
        proxy_role: None,
        proxy_peer_turn_id: None,
        proxy_peer_turn_ids: None,
        tool_surfaces: t.tool_surfaces.iter().map(|s| s.to_string()).collect(),
        tool_call_total: t.tool_call_total,
        agent_topology: t.agent_topology.as_ref().map(|a| a.to_string()),
        suspicious_skills: t
            .suspicious_skills
            .iter()
            .filter_map(|s| serde_json::to_value(s).ok())
            .collect(),
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
    if !f.models.is_empty()
        && !f
            .models
            .iter()
            .any(|m| t.models_used.iter().any(|tm| tm == m))
    {
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
        tool_surfaces: t.tool_surfaces.iter().map(|s| s.to_string()).collect(),
        tool_call_total: t.tool_call_total,
        agent_topology: t.agent_topology.as_ref().map(|a| a.to_string()),
        suspicious_skills: t
            .suspicious_skills
            .iter()
            .filter_map(|s| serde_json::to_value(s).ok())
            .collect(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct CallsParams {
    /// `lite=1` strips request_body, response_body, request_headers,
    /// response_headers from the response so a mega-turn (hundreds of
    /// agentic iterations × hundreds of KB body each) doesn't OOM the
    /// browser. Use `GET /api/llm-calls/{id}` to fetch a specific
    /// call's bodies on demand.
    #[serde(default)]
    pub lite: u8,
}

pub async fn calls(
    State(ctx): State<ApiAgentTurnsContext>,
    Path(turn_id): Path<String>,
    Query(params): Query<CallsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let include_bodies = params.lite == 0;

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
        let items = ctx
            .storage
            .query_calls_by_ids(&call_ids, include_bodies)
            .await?;
        return Ok(ApiResponse::ok(items));
    }
    let items = ctx
        .storage
        .query_turn_calls(&turn_id, include_bodies)
        .await?;
    Ok(ApiResponse::ok(items))
}

// ---- Proxy view ----
//
// Returns the multi-leg fold for one logical LLM call: every member of
// the requested turn's `metadata.proxy.peer_turn_ids` group plus the
// turn itself, with a header diff, optional model-rewrite annotation,
// and a latency breakdown. Used by the Agent Turn detail panel's
// "Proxy View" tab; the same response is suitable for diagnostics tooling
// that wants to inspect what the proxy mutated.

pub async fn proxy_view(
    State(ctx): State<ApiAgentTurnsContext>,
    Path(turn_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let turn = match ctx.storage.query_turn_by_id(&turn_id).await? {
        Some(t) => t,
        None => return Err(ApiError::NotFound(format!("turn not found: {turn_id}"))),
    };

    let proxy = turn.metadata.as_ref().and_then(|m| m.get("proxy"));
    let group_id = proxy
        .and_then(|p| p.get("pair_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::NotFound(format!("turn {turn_id} is not part of a proxy group")))?
        .to_string();
    let mut peer_ids: Vec<String> = proxy
        .and_then(|p| p.get("peer_turn_ids"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    // Backward compat: pre-N-leg writes only set peer_turn_id (a single
    // string). If peer_turn_ids is empty but the legacy field is set,
    // promote the singleton.
    if peer_ids.is_empty() {
        if let Some(legacy) = proxy
            .and_then(|p| p.get("peer_turn_id"))
            .and_then(|v| v.as_str())
        {
            peer_ids.push(legacy.to_string());
        }
    }

    let self_role = proxy
        .and_then(|p| p.get("role"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Build the member list. Canonical (proxy_in / mirror_primary)
    // goes first; remaining members preserve their lex order from
    // peer_turn_ids. If the requested turn isn't the canonical, we
    // still surface it as-is in its lex position — the role field
    // tells the UI how to render it.
    let mut order: Vec<(String, String)> = Vec::with_capacity(peer_ids.len() + 1);
    order.push((turn_id.clone(), self_role));
    for pid in &peer_ids {
        // Resolve each peer's role by fetching its metadata.proxy.role.
        // Cheap relative to the request bodies we're about to pull.
        let peer_turn = ctx.storage.query_turn_by_id(pid).await?;
        let role = peer_turn
            .as_ref()
            .and_then(|t| t.metadata.as_ref())
            .and_then(|m| m.get("proxy"))
            .and_then(|p| p.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        order.push((pid.clone(), role));
    }
    // Sort so the canonical-by-role goes first, then proxy_out, then
    // mirror_secondary. Within the same role, lex by turn_id.
    order.sort_by(|(a_id, a_role), (b_id, b_role)| {
        role_sort_key(a_role)
            .cmp(&role_sort_key(b_role))
            .then_with(|| a_id.cmp(b_id))
    });

    // Fetch each member's TurnDetail + first call body.
    let mut members: Vec<ProxyViewMember> = Vec::with_capacity(order.len());
    for (tid, role) in &order {
        let detail = match ctx.storage.query_turn_by_id(tid).await? {
            Some(t) => t,
            None => continue, // peer evicted by retention since the sweep — skip
        };
        let first_call_id = detail.call_ids.first().cloned();
        let first_call: Option<CallDetail> = match first_call_id {
            Some(ref id) => ctx.storage.query_call_by_id(id).await?,
            None => None,
        };
        members.push(member_from(detail, role.clone(), first_call));
    }

    let request_header_diff = diff_headers(&members, |m| &m.request_headers);
    let response_header_diff = diff_headers(&members, |m| &m.response_headers);
    let model_rewrite = detect_model_rewrite(&members);
    let latency_breakdown = compute_latency_breakdown(&members);

    Ok(ApiResponse::ok(ProxyViewResponse {
        group_id,
        members,
        request_header_diff,
        response_header_diff,
        model_rewrite,
        latency_breakdown,
    }))
}

/// Sort key — lower value goes first. Canonical roles surface above
/// hops, which surface above mirrors.
fn role_sort_key(role: &str) -> u8 {
    match role {
        "proxy_in" => 0,
        "mirror_primary" => 1,
        "proxy_out" => 2,
        "mirror_secondary" => 3,
        _ => 9,
    }
}

fn member_from(
    detail: TurnDetail,
    role: String,
    first_call: Option<CallDetail>,
) -> ProxyViewMember {
    let (
        request_headers,
        response_headers,
        client_port,
        server_port,
        request_path,
        status_code,
        ttft_ms,
        e2e_latency_ms,
        request_model,
    ) = match first_call {
        Some(c) => (
            parse_headers_json(c.request_headers.as_deref()),
            parse_headers_json(c.response_headers.as_deref()),
            Some(c.client_port),
            Some(c.server_port),
            Some(c.request_path),
            c.status_code,
            c.ttft_ms,
            c.e2e_latency_ms,
            extract_model_from_body(c.request_body.as_deref()),
        ),
        None => (
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    };
    ProxyViewMember {
        turn_id: detail.turn_id,
        role,
        client_ip: detail.client_ip,
        client_port,
        server_ip: detail.server_ip,
        server_port,
        start_time: detail.start_time,
        end_time: detail.end_time,
        duration_ms: detail.duration_ms,
        ttft_ms,
        e2e_latency_ms,
        request_model,
        wire_api: detail.wire_api,
        request_path,
        status_code,
        request_headers,
        response_headers,
    }
}

/// Headers are stored as a JSON-encoded `Vec<[name, value]>` in the
/// `request_headers` / `response_headers` columns. Parse into a Vec
/// preserving order; bail to empty if the blob is malformed or missing.
fn parse_headers_json(blob: Option<&str>) -> Vec<(String, String)> {
    let Some(s) = blob else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<(String, String)>>(s).unwrap_or_default()
}

/// Pull the `model` field out of a request body JSON. Used to surface
/// the proxy's model rewrite (client sent X, proxy forwarded as Y).
fn extract_model_from_body(body: Option<&str>) -> Option<String> {
    let s = body?;
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    v.get("model").and_then(|m| m.as_str()).map(String::from)
}

/// Compute the cross-leg diff for one header dimension (request OR
/// response). Each header NAME yields one `HeaderDiffEntry`. Header
/// names are case-insensitively compared (HTTP semantics) but the
/// canonical-case spelling of the first occurrence is preserved in the
/// output.
fn diff_headers<F>(members: &[ProxyViewMember], pick: F) -> Vec<HeaderDiffEntry>
where
    F: Fn(&ProxyViewMember) -> &[(String, String)],
{
    // Bucket: lowercased-name → (canonical_name, per-leg entries).
    let mut by_name: BTreeMap<String, (String, Vec<HeaderValueByLeg>)> = BTreeMap::new();
    for m in members {
        for (name, value) in pick(m) {
            let key = name.to_ascii_lowercase();
            let entry = by_name
                .entry(key)
                .or_insert_with(|| (name.clone(), Vec::new()));
            entry.1.push(HeaderValueByLeg {
                turn_id: m.turn_id.clone(),
                role: m.role.clone(),
                value: value.clone(),
            });
        }
    }
    let leg_count = members.len();
    let mut out = Vec::with_capacity(by_name.len());
    for (_lc, (canonical_name, values)) in by_name {
        let kind = if values.len() < leg_count {
            HeaderDiffKind::PerLeg
        } else {
            // Every leg supplied this header at least once. Decide
            // common vs modified by whether all values match.
            let first = values.first().map(|v| v.value.as_str()).unwrap_or("");
            if values.iter().all(|v| v.value == first) {
                HeaderDiffKind::Common
            } else {
                HeaderDiffKind::Modified
            }
        };
        out.push(HeaderDiffEntry {
            name: canonical_name,
            kind,
            values,
        });
    }
    out
}

/// If the canonical leg's request body advertised one model name and
/// the proxy_out leg advertised another, surface the rewrite.
fn detect_model_rewrite(members: &[ProxyViewMember]) -> Option<ModelRewrite> {
    let canon = members
        .iter()
        .find(|m| m.role == "proxy_in" || m.role == "mirror_primary");
    let upstream = members.iter().find(|m| m.role == "proxy_out");
    match (canon, upstream) {
        (Some(c), Some(u)) if c.request_model != u.request_model => Some(ModelRewrite {
            client_requested: c.request_model.clone(),
            upstream_received: u.request_model.clone(),
        }),
        _ => None,
    }
}

fn compute_latency_breakdown(members: &[ProxyViewMember]) -> LatencyBreakdown {
    let client = members
        .iter()
        .find(|m| m.role == "proxy_in" || m.role == "mirror_primary")
        .and_then(|m| m.e2e_latency_ms);
    let upstream = members
        .iter()
        .find(|m| m.role == "proxy_out")
        .and_then(|m| m.e2e_latency_ms);
    let overhead = match (client, upstream) {
        (Some(c), Some(u)) => Some(c - u),
        _ => None,
    };
    LatencyBreakdown {
        client_observed_ms: client,
        upstream_observed_ms: upstream,
        proxy_overhead_ms: overhead,
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentSummaryParams {
    pub start: i64,
    pub end: i64,
}

#[derive(serde::Serialize)]
struct AgentSummaryResp {
    summary: Vec<AgentKindSummary>,
}

pub async fn summary(
    State(ctx): State<ApiAgentTurnsContext>,
    Query(params): Query<AgentSummaryParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = AgentSummaryQuery {
        time_range: to_time_range(params.start, params.end)?,
    };
    let summary = ctx.storage.query_agent_summary(&query).await?;
    Ok(ApiResponse::ok(AgentSummaryResp { summary }))
}

#[derive(Debug, Deserialize)]
pub struct AgentActivityParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub bucket: Option<u32>,
}

#[derive(serde::Serialize)]
struct AgentActivityResp {
    points: Vec<AgentActivityPoint>,
}

pub async fn activity(
    State(ctx): State<ApiAgentTurnsContext>,
    Query(params): Query<AgentActivityParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = AgentActivityQuery {
        time_range: to_time_range(params.start, params.end)?,
        bucket_seconds: params.bucket,
    };
    let points = ctx.storage.query_agent_activity(&query).await?;
    Ok(ApiResponse::ok(AgentActivityResp { points }))
}

#[cfg(test)]
mod proxy_view_tests {
    use super::*;

    fn member(turn_id: &str, role: &str, model: Option<&str>, e2e: Option<f64>) -> ProxyViewMember {
        ProxyViewMember {
            turn_id: turn_id.into(),
            role: role.into(),
            client_ip: "x".into(),
            client_port: None,
            server_ip: "y".into(),
            server_port: None,
            start_time: 0,
            end_time: 0,
            duration_ms: 0,
            ttft_ms: None,
            e2e_latency_ms: e2e,
            request_model: model.map(String::from),
            wire_api: "openai-chat".into(),
            request_path: None,
            status_code: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
        }
    }

    fn h(name: &str, value: &str) -> (String, String) {
        (name.into(), value.into())
    }

    #[test]
    fn diff_headers_classifies_common_modified_and_per_leg() {
        let mut m1 = member("t1", "proxy_in", None, None);
        let mut m2 = member("t2", "proxy_out", None, None);
        m1.response_headers = vec![
            h("Content-Type", "application/json"),
            h("X-LiteLLM-Call-Id", "abc-123"), // proxy-added
            h("Server", "uvicorn"),
        ];
        m2.response_headers = vec![
            h("content-type", "application/json"), // case-insensitive match → Common
            h("Server", "envoy"),                  // Modified
            h("Anthropic-Request-Id", "req-9"),    // upstream-only
        ];
        let members = vec![m1, m2];
        let diff = diff_headers(&members, |m| &m.response_headers);
        let by_name: std::collections::HashMap<String, &HeaderDiffEntry> =
            diff.iter().map(|e| (e.name.to_lowercase(), e)).collect();
        assert!(matches!(
            by_name["content-type"].kind,
            HeaderDiffKind::Common
        ));
        assert!(matches!(by_name["server"].kind, HeaderDiffKind::Modified));
        assert!(matches!(
            by_name["x-litellm-call-id"].kind,
            HeaderDiffKind::PerLeg
        ));
        assert!(matches!(
            by_name["anthropic-request-id"].kind,
            HeaderDiffKind::PerLeg
        ));
        // PerLeg entries carry the role of the leg that sent them — UI
        // colors "x-litellm-call-id" by proxy_in (litellm injected),
        // "anthropic-request-id" by proxy_out (upstream returned).
        let litellm_entry = by_name["x-litellm-call-id"];
        assert_eq!(litellm_entry.values.len(), 1);
        assert_eq!(litellm_entry.values[0].role, "proxy_in");
        let anth_entry = by_name["anthropic-request-id"];
        assert_eq!(anth_entry.values.len(), 1);
        assert_eq!(anth_entry.values[0].role, "proxy_out");
    }

    #[test]
    fn detect_model_rewrite_surfaces_when_canonical_and_upstream_differ() {
        // Realistic LiteLLM scenario: client requests
        // "claude-3-5-sonnet-20241022", proxy forwards as "qwen36-27b".
        let members = vec![
            member(
                "client",
                "proxy_in",
                Some("claude-3-5-sonnet-20241022"),
                None,
            ),
            member("upstream", "proxy_out", Some("qwen36-27b"), None),
        ];
        let rewrite = detect_model_rewrite(&members).expect("rewrite detected");
        assert_eq!(
            rewrite.client_requested.as_deref(),
            Some("claude-3-5-sonnet-20241022")
        );
        assert_eq!(rewrite.upstream_received.as_deref(), Some("qwen36-27b"));
    }

    #[test]
    fn detect_model_rewrite_returns_none_when_same() {
        let members = vec![
            member("a", "proxy_in", Some("GLM-5.1"), None),
            member("b", "proxy_out", Some("GLM-5.1"), None),
        ];
        assert!(detect_model_rewrite(&members).is_none());
    }

    #[test]
    fn compute_latency_breakdown_yields_proxy_overhead() {
        let members = vec![
            member("a", "proxy_in", None, Some(2294.0)),
            member("b", "proxy_out", None, Some(2291.0)),
        ];
        let b = compute_latency_breakdown(&members);
        assert_eq!(b.client_observed_ms, Some(2294.0));
        assert_eq!(b.upstream_observed_ms, Some(2291.0));
        assert_eq!(b.proxy_overhead_ms, Some(3.0));
    }

    #[test]
    fn compute_latency_breakdown_returns_none_overhead_without_upstream() {
        // Mirror-only group (br0 + docker0 capture, no proxy hop visible) —
        // overhead can't be computed, but the canonical latency still
        // surfaces.
        let members = vec![
            member("a", "mirror_primary", None, Some(2000.0)),
            member("b", "mirror_secondary", None, Some(2000.5)),
        ];
        let b = compute_latency_breakdown(&members);
        assert_eq!(b.client_observed_ms, Some(2000.0));
        assert_eq!(b.upstream_observed_ms, None);
        assert_eq!(b.proxy_overhead_ms, None);
    }

    #[test]
    fn extract_model_from_body_handles_missing_field_gracefully() {
        assert_eq!(extract_model_from_body(None), None);
        assert_eq!(extract_model_from_body(Some("not json")), None);
        assert_eq!(extract_model_from_body(Some(r#"{}"#)), None);
        assert_eq!(
            extract_model_from_body(Some(r#"{"model": "qwen36-27b"}"#)).as_deref(),
            Some("qwen36-27b")
        );
    }

    #[test]
    fn parse_headers_json_round_trips_pairs() {
        let blob = r#"[["X-A","1"],["X-B","2"]]"#;
        let parsed = parse_headers_json(Some(blob));
        assert_eq!(
            parsed,
            vec![
                ("X-A".to_string(), "1".to_string()),
                ("X-B".to_string(), "2".to_string())
            ]
        );
        assert!(parse_headers_json(None).is_empty());
        assert!(parse_headers_json(Some("garbage")).is_empty());
    }
}
