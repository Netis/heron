//! `GET /api/pcap/extract` — stream a 5-tuple-filtered, time-bounded `.pcap`
//! slice out of the on-disk `pcap_dump` minute files. See
//! `docs/superpowers/specs/2026-05-02-pcap-extract-download-design.md`.

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use ts_common::path::is_safe_path_component;
use ts_pcap_extract::{
    prepare_many, stream_extract, ExtractFlow, ExtractRequest, ExtractRequestSet,
};
use ts_storage::query::TurnCallItem;

use crate::extractors::{Path, Query};
use crate::response::ApiError;
use crate::ApiPcapExtractContext;

const MAX_WINDOW_US: i64 = 60 * 60 * 1_000_000; // 1 hour
const SECOND_US: i64 = 1_000_000;
const DEFAULT_CALL_DURATION_MS: i64 = 5_000;

#[derive(Debug, Deserialize)]
pub struct ExtractParams {
    pub source_id: String,
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub client_ip: Option<String>,
    #[serde(default)]
    pub client_port: Option<u16>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub server_port: Option<u16>,
}

pub async fn handler(
    State(ctx): State<ApiPcapExtractContext>,
    Query(params): Query<ExtractParams>,
) -> Result<Response, ApiError> {
    let req = build_request(params)?;
    stream_request(
        ctx.roots,
        ExtractRequestSet::from(req),
        timestamped_filename("ts-extract"),
    )
    .await
}

pub async fn agent_turn_handler(
    State(ctx): State<ApiPcapExtractContext>,
    Path(turn_id): Path<String>,
) -> Result<Response, ApiError> {
    let (source_id, calls) = load_turn_calls(&ctx, &turn_id).await?;
    let req = build_turn_request(source_id, &calls)?;
    stream_request(ctx.roots, req, timestamped_filename("ts-agent-turn")).await
}

async fn stream_request(
    roots: Arc<Vec<ts_pcap_extract::PipelineRoot>>,
    req: ExtractRequestSet,
    filename: String,
) -> Result<Response, ApiError> {
    // Run prepare on the blocking pool: file opens may stall a worker, and
    // surfacing the rare link_type mismatch as 500 must happen BEFORE any
    // 200 OK body byte hits the wire.
    let roots_for_prep = roots.clone();
    let prep = tokio::task::spawn_blocking(move || prepare_many(req, &roots_for_prep))
        .await
        .map_err(|e| ApiError::Internal(format!("pcap-extract prepare panicked: {e}")))?
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let stream = stream_extract(prep);
    let body = Body::from_stream(stream);
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/vnd.tcpdump.pcap".to_string()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response())
}

fn timestamped_filename(prefix: &str) -> String {
    format!("{}-{}.pcap", prefix, Utc::now().format("%Y%m%dT%H%M%S"))
}

fn build_request(p: ExtractParams) -> Result<ExtractRequest, ApiError> {
    if !is_safe_path_component(&p.source_id) {
        return Err(ApiError::InvalidParam(format!(
            "invalid source_id: {}",
            p.source_id
        )));
    }
    if p.start >= p.end {
        return Err(ApiError::InvalidParam("start must be < end".into()));
    }
    if p.end - p.start > MAX_WINDOW_US {
        return Err(ApiError::InvalidParam("time window exceeds 1 hour".into()));
    }
    let client_ip = parse_optional_ip("client_ip", p.client_ip.as_deref())?;
    let server_ip = parse_optional_ip("server_ip", p.server_ip.as_deref())?;
    Ok(ExtractRequest {
        source_id: p.source_id,
        start_us: p.start,
        end_us: p.end,
        client_ip,
        client_port: p.client_port,
        server_ip,
        server_port: p.server_port,
    })
}

async fn load_turn_calls(
    ctx: &ApiPcapExtractContext,
    turn_id: &str,
) -> Result<(String, Vec<TurnCallItem>), ApiError> {
    if let Some((source_id, call_ids)) = ctx.active_turns.read().ok().and_then(|map| {
        map.get(turn_id)
            .map(|t| (t.source_id.clone(), t.call_ids.clone()))
    }) {
        let calls = ctx.storage.query_calls_by_ids(&call_ids).await?;
        return Ok((source_id, calls));
    }

    let turn = ctx
        .storage
        .query_turn_by_id(turn_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("turn not found: {turn_id}")))?;
    let calls = ctx.storage.query_turn_calls(turn_id).await?;
    Ok((turn.source_id, calls))
}

fn build_turn_request(
    source_id: String,
    calls: &[TurnCallItem],
) -> Result<ExtractRequestSet, ApiError> {
    if !is_safe_path_component(&source_id) {
        return Err(ApiError::InvalidParam(format!(
            "invalid source_id: {source_id}"
        )));
    }
    if calls.is_empty() {
        return Err(ApiError::InvalidParam(
            "agent turn has no captured call flows".into(),
        ));
    }

    let mut flows = Vec::with_capacity(calls.len());
    for call in calls {
        let client_ip = call.client_ip.parse::<IpAddr>().map_err(|_| {
            ApiError::Internal(format!(
                "stored call {} has invalid client_ip: {}",
                call.id, call.client_ip
            ))
        })?;
        let server_ip = call.server_ip.parse::<IpAddr>().map_err(|_| {
            ApiError::Internal(format!(
                "stored call {} has invalid server_ip: {}",
                call.id, call.server_ip
            ))
        })?;
        let start_us = ts_ms_to_us(call.request_time) - SECOND_US;
        let end_ms = call
            .complete_time
            .or(call.response_time)
            .unwrap_or(call.request_time + DEFAULT_CALL_DURATION_MS);
        let end_us = ts_ms_to_us(end_ms) + SECOND_US;
        if start_us >= end_us {
            return Err(ApiError::InvalidParam(format!(
                "call {} has invalid packet extraction window",
                call.id
            )));
        }
        flows.push(ExtractFlow {
            start_us,
            end_us,
            client_ip: Some(client_ip),
            client_port: Some(call.client_port),
            server_ip: Some(server_ip),
            server_port: Some(call.server_port),
        });
    }

    let start_us = flows.iter().map(|f| f.start_us).min().unwrap_or(0);
    let end_us = flows.iter().map(|f| f.end_us).max().unwrap_or(0);
    if end_us - start_us > MAX_WINDOW_US {
        return Err(ApiError::InvalidParam("time window exceeds 1 hour".into()));
    }
    Ok(ExtractRequestSet {
        source_id,
        start_us,
        end_us,
        flows,
    })
}

fn ts_ms_to_us(ts_ms: i64) -> i64 {
    ts_ms * 1000
}

fn parse_optional_ip(name: &str, value: Option<&str>) -> Result<Option<IpAddr>, ApiError> {
    match value {
        Some(s) if !s.is_empty() => s
            .parse::<IpAddr>()
            .map(Some)
            .map_err(|_| ApiError::InvalidParam(format!("invalid {name}: {s}"))),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(source_id: &str, start: i64, end: i64) -> ExtractParams {
        ExtractParams {
            source_id: source_id.into(),
            start,
            end,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        }
    }

    #[test]
    fn rejects_unsafe_source_id() {
        let err = build_request(p("..", 0, 1000)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn rejects_start_geq_end() {
        let err = build_request(p("x", 100, 100)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn rejects_window_too_wide() {
        let err = build_request(p("x", 0, MAX_WINDOW_US + 1)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn rejects_bad_ip() {
        let mut params = p("x", 0, 1_000_000);
        params.client_ip = Some("not.an.ip".into());
        let err = build_request(params).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
    #[test]
    fn happy_request() {
        let mut params = p("en0", 0, 1_000_000);
        params.client_ip = Some("10.0.0.1".into());
        params.client_port = Some(54321);
        let req = build_request(params).unwrap();
        assert_eq!(req.source_id, "en0");
        assert_eq!(req.client_port, Some(54321));
        assert!(req.client_ip.is_some());
    }

    fn call(
        id: &str,
        request_time: i64,
        complete_time: Option<i64>,
        client_port: u16,
    ) -> TurnCallItem {
        TurnCallItem {
            id: id.into(),
            sequence: 1,
            request_time,
            response_time: None,
            complete_time,
            wire_api: "openai".into(),
            model: "m".into(),
            status_code: Some(200),
            is_stream: false,
            finish_reason: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            input_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
            request_path: "/v1/chat/completions".into(),
            client_ip: "10.0.0.1".into(),
            client_port,
            server_ip: "1.2.3.4".into(),
            server_port: 443,
            request_body: None,
            response_body: None,
            request_headers: None,
            response_headers: None,
        }
    }

    #[test]
    fn turn_request_uses_exact_call_flows() {
        let calls = vec![
            call("c1", 1_000, Some(1_100), 50001),
            call("c2", 2_000, Some(2_250), 50002),
        ];
        let req = build_turn_request("en0".into(), &calls).unwrap();
        assert_eq!(req.source_id, "en0");
        assert_eq!(req.flows.len(), 2);
        assert_eq!(req.start_us, 0);
        assert_eq!(req.end_us, 3_250_000);
        assert_eq!(req.flows[0].client_port, Some(50001));
        assert_eq!(req.flows[0].server_port, Some(443));
        assert_eq!(req.flows[0].start_us, 0);
        assert_eq!(req.flows[0].end_us, 2_100_000);
        assert_eq!(req.flows[1].client_port, Some(50002));
        assert_eq!(req.flows[1].start_us, 1_000_000);
        assert_eq!(req.flows[1].end_us, 3_250_000);
    }

    #[test]
    fn turn_request_rejects_empty_calls() {
        let err = build_turn_request("en0".into(), &[]).unwrap_err();
        assert!(matches!(err, ApiError::InvalidParam(_)));
    }
}
