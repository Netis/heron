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
use ts_pcap_extract::{prepare, stream_extract, ExtractRequest, PipelineRoot};

use crate::extractors::Query;
use crate::response::ApiError;

const MAX_WINDOW_US: i64 = 60 * 60 * 1_000_000;   // 1 hour

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
    State(roots): State<Arc<Vec<PipelineRoot>>>,
    Query(params): Query<ExtractParams>,
) -> Result<Response, ApiError> {
    let req = build_request(params)?;

    // Run prepare on the blocking pool: file opens may stall a worker, and
    // surfacing the rare link_type mismatch as 500 must happen BEFORE any
    // 200 OK body byte hits the wire.
    let roots_for_prep = roots.clone();
    let prep = tokio::task::spawn_blocking(move || prepare(req, &roots_for_prep))
        .await
        .map_err(|e| ApiError::Internal(format!("pcap-extract prepare panicked: {e}")))?
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let stream = stream_extract(prep);
    let body = Body::from_stream(stream);
    let filename = format!("ts-extract-{}.pcap", Utc::now().format("%Y%m%dT%H%M%S"));
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/vnd.tcpdump.pcap".to_string()),
            (CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
        ],
        body,
    )
        .into_response())
}

fn build_request(p: ExtractParams) -> Result<ExtractRequest, ApiError> {
    if !is_safe_path_component(&p.source_id) {
        return Err(ApiError::InvalidParam(format!("invalid source_id: {}", p.source_id)));
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

fn parse_optional_ip(name: &str, value: Option<&str>) -> Result<Option<IpAddr>, ApiError> {
    match value {
        Some(s) if !s.is_empty() => s.parse::<IpAddr>()
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
            start, end,
            client_ip: None, client_port: None,
            server_ip: None, server_port: None,
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
}
