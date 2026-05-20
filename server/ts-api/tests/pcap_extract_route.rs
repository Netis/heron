//! Smoke test: build a minimal Router with the `/api/pcap/extract` route
//! and a stub `Vec<PipelineRoot>`, hit it with a synthetic GET, assert
//! the response shape.
//!
//! Uses `axum::Router::new().route(...).with_state(...)` directly rather
//! than `ts_api::router(...)` to keep the test focused on the new route.

use std::fs::File;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use tempfile::tempdir;
use tower::util::ServiceExt;
use ts_api::ApiPcapExtractContext;
use ts_llm::model::{ApiType, LlmCall};
use ts_llm::wire_apis as wa;
use ts_pcap_extract::output::global_header;
use ts_pcap_extract::PipelineRoot;
use ts_storage::StorageBackend;
use ts_storage_duckdb::DuckDbBackend;
use ts_turn::{AgentTurn, TurnStatus};

fn state(roots: Arc<Vec<PipelineRoot>>) -> ApiPcapExtractContext {
    ApiPcapExtractContext {
        roots,
        storage: Arc::new(DuckDbBackend::open(":memory:").unwrap()),
        active_turns: ts_turn::new_active_turn_registry(),
    }
}

fn state_with(
    roots: Arc<Vec<PipelineRoot>>,
    storage: Arc<dyn StorageBackend>,
    active_turns: ts_turn::ActiveTurnRegistry,
) -> ApiPcapExtractContext {
    ApiPcapExtractContext {
        roots,
        storage,
        active_turns,
    }
}

fn pcap_app(state: ApiPcapExtractContext) -> Router {
    Router::new()
        .route(
            "/api/pcap/extract",
            get(ts_api::routes::pcap_extract::handler),
        )
        .route(
            "/api/pcap/agent-turns/{id}/packets",
            get(ts_api::routes::pcap_extract::agent_turn_handler),
        )
        .with_state(state)
}

#[tokio::test]
async fn returns_header_only_pcap_when_no_files() {
    let roots: Arc<Vec<PipelineRoot>> = Arc::new(vec![PipelineRoot {
        name: "local".into(),
        dump_dir: std::path::PathBuf::from("/nonexistent"),
    }]);
    let app = pcap_app(state(roots));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/pcap/extract?source_id=en0&start=0&end=30000000")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    // Header-only: 24 bytes, magic at start.
    assert_eq!(body.len(), 24);
    assert_eq!(&body[0..4], &0xa1b2_c3d4u32.to_le_bytes());
}

#[tokio::test]
async fn rejects_window_too_wide_with_400() {
    let roots: Arc<Vec<PipelineRoot>> = Arc::new(vec![]);
    let app = pcap_app(state(roots));

    // 1h + 1us
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/pcap/extract?source_id=en0&start=0&end=3600000001")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_turn_download_contains_only_exact_call_flows() {
    let base = tempdir().unwrap();
    let src_dir = base.path().join("local/en0");
    std::fs::create_dir_all(&src_dir).unwrap();

    let c1_req = ipv4_tcp_pkt([10, 0, 0, 1], 50001, [1, 2, 3, 4], 443);
    let c1_resp = ipv4_tcp_pkt([1, 2, 3, 4], 443, [10, 0, 0, 1], 50001);
    let c2_req = ipv4_tcp_pkt([10, 0, 0, 1], 50002, [1, 2, 3, 4], 443);
    let wrong_port = ipv4_tcp_pkt([10, 0, 0, 1], 59999, [1, 2, 3, 4], 443);
    let c2_before_call = ipv4_tcp_pkt([10, 0, 0, 1], 50002, [1, 2, 3, 4], 443);
    let wrong_server = ipv4_tcp_pkt([10, 0, 0, 1], 50001, [5, 6, 7, 8], 443);
    write_minute_file(
        &src_dir,
        "19700101T0000",
        &[
            (1_050_000, &c1_req),
            (1_100_000, &c1_resp),
            (1_200_000, &wrong_port),
            (1_500_000, &c2_before_call),
            (2_500_000, &wrong_server),
            (3_050_000, &c2_req),
        ],
    );

    let backend = Arc::new(DuckDbBackend::open(":memory:").unwrap());
    backend.init().await.unwrap();
    backend
        .write_calls(vec![
            call("c1", 1_000_000, 1_100_000, 50001),
            call("c2", 3_000_000, 3_200_000, 50002),
        ])
        .await
        .unwrap();

    let active_turns = ts_turn::new_active_turn_registry();
    active_turns
        .write()
        .unwrap()
        .insert("turn-1".into(), turn("turn-1", vec!["c1", "c2"]));

    let roots = Arc::new(vec![PipelineRoot {
        name: "local".into(),
        dump_dir: base.path().to_path_buf(),
    }]);
    let app = pcap_app(state_with(roots, backend, active_turns));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/pcap/agent-turns/turn-1/packets")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        pcap_timestamps(&body),
        vec![1_050_000, 1_100_000, 3_050_000]
    );
}

fn call(id: &str, request_time: i64, complete_time: i64, client_port: u16) -> LlmCall {
    LlmCall {
        source_id: "en0".into(),
        id: id.into(),
        wire_api: wa::OPENAI_CHAT,
        model: "gpt-4".into(),
        api_type: ApiType::Chat,
        request_time,
        response_time: Some(request_time + 50_000),
        complete_time: Some(complete_time),
        request_path: "/v1/chat/completions".into(),
        is_stream: false,
        request_body: None,
        status_code: Some(200),
        finish_reason: Some("stop".into()),
        response_body: None,
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        ttft_ms: Some(50.0),
        e2e_latency_ms: Some(((complete_time - request_time) as f64) / 1000.0),
        client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
        client_port,
        server_ip: "1.2.3.4".parse::<IpAddr>().unwrap(),
        server_port: 443,
        response_id: None,
        request_headers: vec![],
        response_headers: vec![],
    }
}

fn turn(id: &str, call_ids: Vec<&str>) -> AgentTurn {
    AgentTurn {
        source_id: "en0".into(),
        turn_id: id.into(),
        session_id: "session-1".into(),
        wire_api: wa::OPENAI_CHAT.into(),
        agent_kind: "codex-cli".into(),
        client_ip: "10.0.0.1".parse().unwrap(),
        server_ip: "1.2.3.4".parse().unwrap(),
        start_time_us: 1_000_000,
        end_time_us: 3_200_000,
        duration_ms: 2_200,
        call_count: call_ids.len() as u32,
        models_used: vec!["gpt-4".into()],
        subagents_used: vec![],
        total_input_tokens: 20,
        total_output_tokens: 10,
        total_cache_read_input_tokens: 0,
        total_cache_creation_input_tokens: 0,
        total_cost_usd: None,
        status: TurnStatus::InProgress,
        final_finish_reason: None,
        user_input_preview: None,
        user_call_id: None,
        final_answer_preview: None,
        final_call_id: None,
        call_ids: call_ids.into_iter().map(String::from).collect(),
        metadata: serde_json::json!({}),
    }
}

fn ipv4_tcp_pkt(src_ip: [u8; 4], src_port: u16, dst_ip: [u8; 4], dst_port: u16) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0u8; 12]);
    frame.extend_from_slice(&[0x08, 0x00]);
    let ip_total_len: u16 = 40;
    let mut ip = vec![0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&ip_total_len.to_be_bytes());
    ip[8] = 64;
    ip[9] = 6;
    ip[12..16].copy_from_slice(&src_ip);
    ip[16..20].copy_from_slice(&dst_ip);
    frame.extend_from_slice(&ip);
    let mut tcp = vec![0u8; 20];
    tcp[0..2].copy_from_slice(&src_port.to_be_bytes());
    tcp[2..4].copy_from_slice(&dst_port.to_be_bytes());
    tcp[12] = 0x50;
    tcp[13] = 0x10;
    frame.extend_from_slice(&tcp);
    frame
}

fn write_minute_file(dir: &Path, label: &str, recs: &[(i64, &[u8])]) {
    let path = dir.join(format!("{label}.pcap"));
    let mut f = File::create(path).unwrap();
    f.write_all(&global_header(1)).unwrap();
    for (ts_us, data) in recs {
        f.write_all(&record_header(*ts_us, data.len() as u32))
            .unwrap();
        f.write_all(data).unwrap();
    }
}

fn record_header(ts_us: i64, len: u32) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&((ts_us / 1_000_000) as u32).to_le_bytes());
    buf[4..8].copy_from_slice(&((ts_us % 1_000_000) as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&len.to_le_bytes());
    buf[12..16].copy_from_slice(&len.to_le_bytes());
    buf
}

fn pcap_timestamps(bytes: &[u8]) -> Vec<i64> {
    assert!(bytes.len() >= 24);
    assert_eq!(&bytes[0..4], &0xa1b2_c3d4u32.to_le_bytes());
    let mut out = Vec::new();
    let mut offset = 24;
    while offset < bytes.len() {
        let ts_sec = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as i64;
        let ts_usec = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as i64;
        let caplen =
            u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
        out.push(ts_sec * 1_000_000 + ts_usec);
        offset += 16 + caplen;
    }
    assert_eq!(offset, bytes.len());
    out
}
