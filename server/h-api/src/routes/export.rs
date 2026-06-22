//! `/api/export/trajectory{,ies}` — reconstruct captured agent turns/sessions
//! into OpenAI-style SFT JSONL for fine-tuning.
//!
//! * `GET /api/export/trajectory` — one turn (`scope=turn&turn_id=…`) or one session
//!   (`scope=session&source_id=…&session_id=…`, the whole multi-turn conversation as a single
//!   trajectory).
//! * `GET /api/export/trajectories` — **batch**: every turn matching the same filters as
//!   `/api/agent-turns` (time range + wire_api/model/status/…), one trajectory line per turn. This
//!   is how a training set is built from a slice of captured traffic.
//!
//! Reconstruction uses the turn's terminal call, whose bodies already carry the
//! full cumulative history (`h_export::reconstruct_trajectory`). Responses are
//! raw NDJSON with a `Content-Disposition` attachment (NOT the `ApiResponse`
//! envelope). The batch endpoint pre-resolves every turn so it can report
//! `X-Export-Skipped` in a header, and skips (rather than fails) turns whose
//! wire format is unsupported or whose body is unavailable.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Response, StatusCode};
use h_storage::query::{TraceDetail, TracesQuery};
use h_storage::StorageBackend;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::extractors::Query;
use crate::params::{parse_csv, to_dimension_filter, to_time_range};
use crate::response::ApiError;

/// Hard cap on a single batch export, to bound the N+1 terminal-call lookups.
const MAX_BATCH_TURNS: u32 = 2000;

#[derive(Debug, Deserialize)]
pub struct ExportTrajectoryParams {
    /// `"turn"` | `"session"`.
    pub scope: String,
    /// Required when `scope == "turn"`.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Required when `scope == "session"`.
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Mirrors `agent_turns::TurnsParams` (the list filters) so a batch export
/// covers exactly the turns the user is looking at.
#[derive(Debug, Deserialize)]
pub struct ExportBatchParams {
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
    pub server_port: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub agent_kind: Option<String>,
    #[serde(default)]
    pub include_proxy_hops: bool,
    /// Max turns to export (default 1000, capped at `MAX_BATCH_TURNS`).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Export a single turn or session as one JSONL line.
pub async fn single(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(p): Query<ExportTrajectoryParams>,
) -> Result<Response<Body>, ApiError> {
    let (detail, scope_label, filename) = match p.scope.as_str() {
        "turn" => {
            let turn_id = p
                .turn_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ApiError::InvalidParam("turn scope requires turn_id".into()))?;
            let detail = storage
                .query_trace_by_id(turn_id)
                .await?
                .ok_or_else(|| ApiError::NotFound(format!("turn not found: {turn_id}")))?;
            (detail, "turn", format!("trajectory-{turn_id}.jsonl"))
        }
        "session" => {
            let source_id = p.source_id.as_deref().unwrap_or_default();
            let session_id = p
                .session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ApiError::InvalidParam("session scope requires session_id".into())
                })?;
            let detail = resolve_session_last_turn(&storage, source_id, session_id)
                .await?
                .ok_or_else(|| {
                    ApiError::NotFound(format!("session has no turns: {source_id}/{session_id}"))
                })?;
            (detail, "session", format!("trajectory-{session_id}.jsonl"))
        }
        other => return Err(ApiError::InvalidParam(format!("invalid scope: {other}"))),
    };

    match line_from_turn_detail(&storage, &detail, scope_label).await? {
        Ok(line) => ndjson_response(line, &filename, None),
        Err(reason) => Err(ApiError::InvalidParam(reason)),
    }
}

/// Batch-export every turn matching the agent-turns filters, one line per turn.
pub async fn batch(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(p): Query<ExportBatchParams>,
) -> Result<Response<Body>, ApiError> {
    let server_ports = parse_csv(&p.server_port)
        .iter()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| ApiError::InvalidParam(format!("invalid server_port: {s}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let page_size = p.limit.unwrap_or(1000).clamp(1, MAX_BATCH_TURNS);
    let query = TracesQuery {
        time_range: to_time_range(p.start, p.end)?,
        filter: to_dimension_filter(&p.wire_api, &p.model, &p.server_ip, &None),
        client_ips: parse_csv(&p.client_ip),
        server_ports,
        statuses: parse_csv(&p.status),
        agent_kinds: parse_csv(&p.agent_kind),
        sort_by: "start_time".to_string(),
        sort_order: "desc".to_string(),
        page: 1,
        page_size,
        include_proxy_hops: p.include_proxy_hops,
    };

    let page = storage.query_traces(&query).await?;
    let total = page.items.len();
    let mut lines: Vec<String> = Vec::new();
    for item in &page.items {
        // TraceListItem lacks final_call_id; resolve the full detail per turn.
        let Some(detail) = storage.query_trace_by_id(&item.turn_id).await? else {
            continue;
        };
        match line_from_turn_detail(&storage, &detail, "turn").await {
            Ok(Ok(line)) => lines.push(line),
            // skip unsupported wire / unavailable body — don't fail the batch
            Ok(Err(_reason)) => {}
            Err(_api) => {}
        }
    }
    let written = lines.len();
    let skipped = total - written;
    ndjson_response(
        lines.join(""),
        "trajectories.jsonl",
        Some((total, written, skipped)),
    )
}

/// The session's most-recent turn — its terminal call's request body holds the
/// whole multi-turn session history.
async fn resolve_session_last_turn(
    storage: &Arc<dyn StorageBackend>,
    source_id: &str,
    session_id: &str,
) -> Result<Option<TraceDetail>, ApiError> {
    let page = storage
        .query_session_traces(&h_storage::query::SessionTracesQuery {
            source_id: source_id.to_string(),
            session_id: session_id.to_string(),
            cursor: None,
            page_size: 1,
        })
        .await?;
    let Some(last) = page.items.first() else {
        return Ok(None);
    };
    Ok(storage.query_trace_by_id(&last.turn_id).await?)
}

/// Reconstruct one trajectory line from a resolved `TraceDetail`.
///
/// Outer `Result` = a genuine storage/internal failure (500 on the single path;
/// counted as skipped on the batch path). Inner `Result` = `Ok(line)` or
/// `Err(reason)` for export-level issues (no terminal call, unsupported wire,
/// truncated/missing body) the caller turns into a 400 (single) or a skip (batch).
async fn line_from_turn_detail(
    storage: &Arc<dyn StorageBackend>,
    detail: &TraceDetail,
    scope_label: &str,
) -> Result<Result<String, String>, ApiError> {
    let Some(final_call_id) = detail.final_call_id.clone() else {
        return Ok(Err("turn has no terminal call".to_string()));
    };
    let calls = storage
        .query_spans_by_ids(&[final_call_id.clone()], true)
        .await?;
    let Some(call) = calls.into_iter().next() else {
        return Ok(Err("terminal call body unavailable".to_string()));
    };

    // A truncated body manifests as a JSON parse failure.
    let req: Value = match call.request_body.as_deref().map(serde_json::from_str) {
        Some(Ok(v)) => v,
        Some(Err(_)) => return Ok(Err("malformed/truncated request body".to_string())),
        None => return Ok(Err("missing request body".to_string())),
    };
    let resp: Value = match call.response_body.as_deref().map(serde_json::from_str) {
        Some(Ok(v)) => v,
        Some(Err(_)) => return Ok(Err("malformed/truncated response body".to_string())),
        None => return Ok(Err("missing response body".to_string())),
    };

    let traj = match h_export::reconstruct_trajectory(&detail.wire_api, &req, &resp) {
        Ok(t) => t,
        Err(e) => return Ok(Err(e.to_string())),
    };

    let meta = json!({
        "source": "heron",
        "scope": scope_label,
        "source_id": detail.source_id,
        "session_id": detail.session_id,
        "turn_id": detail.turn_id,
        "terminal_call_id": final_call_id,
        "wire_api": detail.wire_api,
        "agent_kind": detail.agent_kind,
        "model": detail.models_used.first().cloned(),
        "final_finish_reason": detail.final_finish_reason,
        "total_input_tokens": detail.total_input_tokens,
        "total_output_tokens": detail.total_output_tokens,
        "captured_at_ms": detail.end_time,
    });

    let line = serde_json::to_string(&traj.with_meta(meta))
        .map_err(|e| ApiError::Internal(format!("serialize trajectory: {e}")))?;
    Ok(Ok(format!("{line}\n")))
}

/// Build a raw NDJSON attachment response. `counts = (total, written, skipped)`
/// adds the `X-Export-*` headers (batch only).
fn ndjson_response(
    body: String,
    filename: &str,
    counts: Option<(usize, usize, usize)>,
) -> Result<Response<Body>, ApiError> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}\""),
        );
    if let Some((total, written, skipped)) = counts {
        builder = builder
            .header("x-export-total", total.to_string())
            .header("x-export-written", written.to_string())
            .header("x-export-skipped", skipped.to_string());
    }
    builder
        .body(Body::from(body))
        .map_err(|e| ApiError::Internal(format!("build response: {e}")))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use h_storage::StorageBackend;
    use h_storage_duckdb::DuckDbBackend;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::routes;

    async fn app() -> Router {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        let storage: Arc<dyn StorageBackend> = Arc::new(backend);
        Router::new()
            .route("/api/export/trajectory", get(routes::export::single))
            .route("/api/export/trajectories", get(routes::export::batch))
            .with_state(storage)
    }

    #[tokio::test]
    async fn single_turn_missing_returns_404() {
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/export/trajectory?scope=turn&turn_id=ghost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn single_turn_without_id_is_400() {
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/export/trajectory?scope=turn")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn batch_over_empty_db_is_empty_ok() {
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/export/trajectories?start=0&end=4000000000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("x-export-total").unwrap(), "0");
        assert_eq!(resp.headers().get("x-export-written").unwrap(), "0");
        assert_eq!(resp.headers().get("x-export-skipped").unwrap(), "0");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.is_empty());
    }
}
