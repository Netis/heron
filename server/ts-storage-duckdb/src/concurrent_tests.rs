//! Cross-entity concurrency tests — spans calls / metrics / turns.

use crate::DuckDbBackend;
use std::net::IpAddr;
use std::sync::Arc;
use tempfile::TempDir;
use ts_llm::model::{ApiType, LlmCall};
use ts_llm::wire_apis as wa;
use ts_metrics::model::LlmMetric;
use ts_storage::StorageBackend;
use ts_turn::{AgentTurn, TurnStatus};

fn mk_call(i: usize) -> LlmCall {
    LlmCall {
        source_id: String::new(),
        id: format!("call-{i:08}"),
        wire_api: wa::OPENAI_CHAT,
        model: "gpt-4".into(),
        api_type: ApiType::Chat,
        request_time: 1_700_000_000_000_000 + i as i64,
        response_time: None,
        complete_time: None,
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
        ttft_ms: None,
        e2e_latency_ms: None,
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
    }
}

fn mk_turn(i: usize) -> AgentTurn {
    AgentTurn {
        source_id: String::new(),
        turn_id: format!("turn-{i:08}"),
        session_id: format!("session-{}", i % 10),
        wire_api: wa::OPENAI_CHAT.into(),
        agent_kind: "test".into(),
        client_ip: "127.0.0.1".parse().unwrap(),
        server_ip: "127.0.0.1".parse().unwrap(),
        start_time_us: 1_700_000_000_000_000 + i as i64,
        end_time_us: 1_700_000_000_000_000 + i as i64 + 1_000_000,
        duration_ms: 1000,
        call_count: 1,
        models_used: vec!["gpt-4".into()],
        subagents_used: vec![],
        total_input_tokens: 10,
        total_output_tokens: 5,
        total_cache_read_input_tokens: 0,
        total_cache_creation_input_tokens: 0,
        total_cost_usd: None,
        status: TurnStatus::Complete,
        final_finish_reason: None,
        user_input_preview: None,
        user_call_id: None,
        final_answer_preview: None,
        final_call_id: None,
        call_ids: vec![format!("call-{i:08}")],
        metadata: serde_json::json!({}),
        tool_surfaces: vec![],
        tool_call_total: 0,
        agent_topology: None,
        suspicious_skills: vec![],
    }
}

fn mk_metric(i: usize) -> LlmMetric {
    LlmMetric {
        timestamp_us: 1_700_000_000_000_000 + i as i64 * 10_000_000,
        source_id: String::new(),
        granularity: "10s",
        wire_api: wa::OPENAI_CHAT.into(),
        model: "gpt-4".into(),
        server_ip: "10.0.0.2".into(),
        call_count: 1,
        stream_count: 0,
        non_stream_count: 1,
        active_calls_sum: 1,
        active_calls_sample_count: 1,
        active_calls_max: 1,
        total_input_tokens: 10,
        input_token_count: 1,
        total_output_tokens: 5,
        output_token_count: 1,
        total_cache_read_input_tokens: 0,
        total_cache_creation_input_tokens: 0,
        error_count: 0,
        error_4xx_count: 0,
        error_429_count: 0,
        error_5xx_count: 0,
        ttft_sum: 0.0,
        ttft_count: 0,
        ttft_p50: None,
        ttft_p95: None,
        ttft_p99: None,
        ttft_stream_sum: 0.0,
        ttft_stream_count: 0,
        ttft_stream_p50: None,
        ttft_stream_p95: None,
        ttft_stream_p99: None,
        ttft_nonstream_sum: 0.0,
        ttft_nonstream_count: 0,
        ttft_nonstream_p50: None,
        ttft_nonstream_p95: None,
        ttft_nonstream_p99: None,
        e2e_sum: 0.0,
        e2e_count: 0,
        e2e_p50: None,
        e2e_p95: None,
        e2e_p99: None,
        tpot_sum: 0.0,
        tpot_count: 0,
        tpot_p50: None,
        tpot_p95: None,
        tpot_p99: None,
        tool_surface: None,
    }
}

// Exercises the three-writer split against a real on-disk file so the
// DuckDB WAL path is hit: three tasks flush concurrent batches to
// disjoint tables and all data must round-trip. A deadlock in the
// writer-mutex refactor would hang this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_to_three_tables() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("concurrent.duckdb");
    let backend = Arc::new(DuckDbBackend::open(path.to_str().unwrap()).unwrap());
    backend.init().await.unwrap();

    const PER_TABLE: usize = 200;
    const BATCHES: usize = 5;

    let calls_backend = backend.clone();
    let calls_task = tokio::spawn(async move {
        for b in 0..BATCHES {
            let batch: Vec<_> = (0..PER_TABLE).map(|i| mk_call(b * PER_TABLE + i)).collect();
            calls_backend.write_calls(batch).await.unwrap();
        }
    });

    let turns_backend = backend.clone();
    let turns_task = tokio::spawn(async move {
        for b in 0..BATCHES {
            let batch: Vec<_> = (0..PER_TABLE).map(|i| mk_turn(b * PER_TABLE + i)).collect();
            turns_backend.write_turns(batch).await.unwrap();
        }
    });

    let metrics_backend = backend.clone();
    let metrics_task = tokio::spawn(async move {
        for b in 0..BATCHES {
            let batch: Vec<_> = (0..PER_TABLE)
                .map(|i| mk_metric(b * PER_TABLE + i))
                .collect();
            metrics_backend.write_metrics(batch).await.unwrap();
        }
    });

    let (a, b, c) = tokio::join!(calls_task, turns_task, metrics_task);
    a.unwrap();
    b.unwrap();
    c.unwrap();

    let expected = (PER_TABLE * BATCHES) as i64;
    let conn = backend.test_conn().lock().unwrap();
    let calls_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM llm_calls", [], |r| r.get(0))
        .unwrap();
    let turns_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_turns", [], |r| r.get(0))
        .unwrap();
    let metrics_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM llm_metrics", [], |r| r.get(0))
        .unwrap();
    assert_eq!(calls_count, expected);
    assert_eq!(turns_count, expected);
    assert_eq!(metrics_count, expected);
}
