//! Live-server integration tests, gated on `CLICKHOUSE_TEST_URL`.
//!
//! ClickHouse has no in-process `:memory:` mode, so these need a real server.
//! When `CLICKHOUSE_TEST_URL` is unset every test self-skips, keeping
//! `cargo test --workspace` green on machines/CI without a server. To run them:
//!
//! ```bash
//! CLICKHOUSE_TEST_URL=http://localhost:8123 cargo test -p h-storage-clickhouse
//! ```
//!
//! Each test gets its own database (dropped + recreated for isolation), so the
//! suite is safe to run in parallel.

#![cfg(test)]

use clickhouse::Row;
use serde::Deserialize;

use h_common::config::ClickHouseConfig;
use h_storage::query::*;
use h_storage::retention::RetentionPolicy;
use h_storage::StorageBackend;

use crate::ClickHouseBackend;

#[derive(Row, Deserialize)]
pub(crate) struct CountRow {
    pub(crate) n: u64,
}

/// Build a backend against a per-test database, dropped + re-`init()`ed for a
/// clean slate. Returns `None` (test self-skips) when `CLICKHOUSE_TEST_URL` is
/// unset.
pub(crate) async fn fresh_backend(db: &str) -> Option<ClickHouseBackend> {
    let url = std::env::var("CLICKHOUSE_TEST_URL").ok()?;
    let cfg = ClickHouseConfig {
        url,
        database: db.to_string(),
        user: std::env::var("CLICKHOUSE_TEST_USER").unwrap_or_else(|_| "default".into()),
        password: std::env::var("CLICKHOUSE_TEST_PASSWORD").unwrap_or_default(),
        optimize_on_sweep: false,
    };
    let backend = ClickHouseBackend::new(&cfg).expect("build backend");
    backend
        .admin_client()
        .query(&format!("DROP DATABASE IF EXISTS `{db}`"))
        .execute()
        .await
        .expect("drop test database");
    backend.init().await.expect("init schema");
    Some(backend)
}

/// `SELECT count() AS n FROM {table}` on the backend's db-scoped client.
pub(crate) async fn count(backend: &ClickHouseBackend, table: &str) -> u64 {
    backend
        .client
        .query(&format!("SELECT count() AS n FROM {table}"))
        .fetch_one::<CountRow>()
        .await
        .expect("count query")
        .n
}

macro_rules! require_backend {
    ($db:expr) => {
        match crate::it::fresh_backend($db).await {
            Some(b) => b,
            None => {
                eprintln!("skip: CLICKHOUSE_TEST_URL unset");
                return;
            }
        }
    };
}

const ALL_TABLES: &[&str] = &[
    "llm_calls",
    "llm_metrics",
    "llm_finish_metrics",
    "agent_turns",
    "http_exchanges",
];

#[tokio::test]
async fn init_creates_all_tables_and_is_idempotent() {
    let backend = require_backend!("heron_it_init");
    // Re-init must be a no-op (CREATE ... IF NOT EXISTS).
    backend.init().await.expect("re-init");
    for t in ALL_TABLES {
        assert_eq!(count(&backend, t).await, 0, "table {t} should exist + be empty");
    }
}

// ---------------------------------------------------------------------------
// Shared domain fixtures (mirror the DuckDB backend's test fixtures so both
// backends are exercised against identical data).
// ---------------------------------------------------------------------------

pub(crate) mod fixtures {
    use std::net::IpAddr;
    use std::sync::Arc;

    use bytes::Bytes;
    use h_llm::model::{ApiType, LlmCall};
    use h_llm::wire_apis as wa;
    use h_metrics::model::{LlmFinishMetric, LlmMetric};
    use h_protocol::model::{HttpRequestData, HttpResponseData};
    use h_protocol::net::FlowKey;
    use h_protocol::HttpExchange;
    use h_turn::{AgentTurn, TurnStatus};

    pub(crate) fn sample_call(id: &str, request_time_us: i64) -> LlmCall {
        LlmCall {
            source_id: "src-0".to_string(),
            id: id.to_string(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            request_time: request_time_us,
            response_time: Some(request_time_us + 500_000),
            complete_time: Some(request_time_us + 1_000_000),
            request_path: "/v1/chat/completions".to_string(),
            is_stream: true,
            request_body: Some(r#"{"model":"gpt-4"}"#.to_string()),
            status_code: Some(200),
            finish_reason: Some("stop".to_string()),
            response_body: Some(r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50}}"#.to_string()),
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: Some(500.0),
            e2e_latency_ms: Some(1000.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 54321,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 8080,
            response_id: Some("chatcmpl-test123".to_string()),
            request_headers: vec![("content-type".to_string(), "application/json".to_string())],
            response_headers: vec![("x-request-id".to_string(), "req_abc123".to_string())],
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
            body_bytes_dropped: 0,
        }
    }

    pub(crate) fn sample_metric(granularity: &'static str, ts_us: i64) -> LlmMetric {
        sample_metric_dim(granularity, ts_us, wa::OPENAI_CHAT, "gpt-4", "10.0.0.2")
    }

    /// Like `sample_metric` but with explicit `(wire_api, model, server_ip)` —
    /// the live aggregator emits one row per GROUPING-SETS tier (specific +
    /// `'*'` rollups), so tests that exercise dimension filtering must seed the
    /// matching tier (e.g. `('*', '*', '*')` for the default/unfiltered query).
    pub(crate) fn sample_metric_dim(
        granularity: &'static str,
        ts_us: i64,
        wire_api: &str,
        model: &str,
        server_ip: &str,
    ) -> LlmMetric {
        LlmMetric {
            timestamp_us: ts_us,
            source_id: "src-0".into(),
            granularity,
            wire_api: wire_api.into(),
            model: model.into(),
            server_ip: server_ip.into(),
            call_count: 1,
            stream_count: 1,
            non_stream_count: 0,
            active_calls_sum: 1,
            active_calls_sample_count: 1,
            active_calls_max: 1,
            total_input_tokens: 100,
            input_token_count: 1,
            total_output_tokens: 50,
            output_token_count: 1,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            error_count: 0,
            error_4xx_count: 0,
            error_429_count: 0,
            error_5xx_count: 0,
            ttft_sum: 500.0,
            ttft_count: 1,
            ttft_p50: Some(500.0),
            ttft_p95: Some(500.0),
            ttft_p99: Some(500.0),
            ttft_stream_sum: 500.0,
            ttft_stream_count: 1,
            ttft_stream_p50: Some(500.0),
            ttft_stream_p95: Some(500.0),
            ttft_stream_p99: Some(500.0),
            ttft_nonstream_sum: 0.0,
            ttft_nonstream_count: 0,
            ttft_nonstream_p50: None,
            ttft_nonstream_p95: None,
            ttft_nonstream_p99: None,
            e2e_sum: 1000.0,
            e2e_count: 1,
            e2e_p50: Some(1000.0),
            e2e_p95: Some(1000.0),
            e2e_p99: Some(1000.0),
            tpot_sum: 10.0,
            tpot_count: 1,
            tpot_p50: Some(10.0),
            tpot_p95: Some(10.0),
            tpot_p99: Some(10.0),
            tool_surface: None,
        }
    }

    pub(crate) fn sample_finish_metric(
        granularity: &str,
        ts_us: i64,
        finish_reason: &str,
    ) -> LlmFinishMetric {
        sample_finish_metric_dim(granularity, ts_us, wa::OPENAI_CHAT, "gpt-4", "10.0.0.2", finish_reason)
    }

    pub(crate) fn sample_finish_metric_dim(
        granularity: &str,
        ts_us: i64,
        wire_api: &str,
        model: &str,
        server_ip: &str,
        finish_reason: &str,
    ) -> LlmFinishMetric {
        LlmFinishMetric {
            timestamp_us: ts_us,
            source_id: "src-0".into(),
            granularity: granularity.into(),
            wire_api: wire_api.into(),
            model: model.into(),
            server_ip: server_ip.into(),
            finish_reason: finish_reason.into(),
            count: 1,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sample_turn(
        turn_id: &str,
        session_id: &str,
        start_us: i64,
        call_ids: Vec<&str>,
    ) -> AgentTurn {
        AgentTurn {
            source_id: "src-0".into(),
            turn_id: turn_id.into(),
            session_id: session_id.into(),
            wire_api: wa::OPENAI_CHAT.into(),
            agent_kind: "claude-cli".into(),
            client_ip: "10.0.0.1".parse().unwrap(),
            server_ip: "10.0.0.2".parse().unwrap(),
            start_time_us: start_us,
            end_time_us: start_us + 5_000_000,
            duration_ms: 5_000,
            call_count: call_ids.len() as u32,
            models_used: vec!["gpt-4".into()],
            subagents_used: vec![],
            total_input_tokens: 100,
            total_output_tokens: 50,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_cost_usd: None,
            status: TurnStatus::Complete,
            final_finish_reason: Some("stop".into()),
            user_input_preview: Some("hello".into()),
            user_call_id: call_ids.first().map(|s| s.to_string()),
            final_answer_preview: Some("world".into()),
            final_call_id: call_ids.last().map(|s| s.to_string()),
            call_ids: call_ids.into_iter().map(String::from).collect(),
            metadata: serde_json::json!({}),
            tool_surfaces: vec![],
            tool_call_total: 0,
            agent_topology: None,
            suspicious_skills: vec![],
        }
    }

    pub(crate) fn sample_exchange(id: &str, request_time_us: i64) -> HttpExchange {
        let client_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let server_ip: IpAddr = "10.0.0.2".parse().unwrap();
        let request = Arc::new(HttpRequestData {
            flow_key: FlowKey::new("src-0".into(), client_ip, 54321, server_ip, 443),
            client_addr: (client_ip, 54321),
            server_addr: (server_ip, 443),
            method: "POST".into(),
            uri: "/v1/chat/completions".into(),
            version: 1,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from_static(br#"{"model":"gpt-4"}"#),
            timestamp_us: request_time_us,
        });
        let response = Arc::new(HttpResponseData {
            flow_key: request.flow_key.clone(),
            client_addr: request.client_addr,
            server_addr: request.server_addr,
            status: 200,
            version: 1,
            headers: vec![("x-request-id".into(), "req_abc".into())],
            body: Bytes::from_static(br#"{"choices":[]}"#),
            first_byte_timestamp_us: request_time_us + 500_000,
            complete_timestamp_us: request_time_us + 1_000_000,
        });
        HttpExchange {
            id: id.to_string(),
            request,
            response,
            sse_event_count: 0,
            sse_data_bytes: 0,
        }
    }
}

#[derive(Row, Deserialize)]
struct RtCall {
    id: String,
    model: String,
    input_tokens: Option<u32>,
    request_time_us: i64,
}

#[tokio::test]
async fn write_paths_round_trip_all_tables() {
    let backend = require_backend!("heron_it_writes");
    let ts = 1_700_000_000_000_000_i64;

    backend
        .write_calls(vec![fixtures::sample_call("call-1", ts)])
        .await
        .expect("write_calls");
    backend
        .write_metrics(vec![fixtures::sample_metric("1m", ts)])
        .await
        .expect("write_metrics");
    backend
        .write_finish_metrics(vec![fixtures::sample_finish_metric("1m", ts, "stop")])
        .await
        .expect("write_finish_metrics");
    backend
        .write_turns(vec![fixtures::sample_turn("turn-1", "sess-1", ts, vec!["call-1"])])
        .await
        .expect("write_turns");
    backend
        .write_exchanges(vec![fixtures::sample_exchange("xchg-1", ts)])
        .await
        .expect("write_exchanges");

    for t in ALL_TABLES {
        assert_eq!(count(&backend, t).await, 1, "table {t} should have 1 row");
    }

    // Verify the DateTime64(6) <-> i64-micros round-trip survives exactly, plus
    // a couple of scalar columns.
    let rt = backend
        .client
        .query(
            "SELECT id, model, input_tokens, toUnixTimestamp64Micro(request_time) AS request_time_us \
             FROM llm_calls WHERE id = 'call-1'",
        )
        .fetch_one::<RtCall>()
        .await
        .expect("read back call");
    assert_eq!(rt.id, "call-1");
    assert_eq!(rt.model, "gpt-4");
    assert_eq!(rt.input_tokens, Some(100));
    assert_eq!(rt.request_time_us, ts, "DateTime64(6) micros round-trip");
}

#[tokio::test]
async fn query_calls_paginates_filters_and_by_id() {
    let backend = require_backend!("heron_it_calls");
    let ts = 1_700_000_000_000_000_i64;
    backend
        .write_calls(vec![
            fixtures::sample_call("c1", ts),
            fixtures::sample_call("c2", ts + 60_000_000),
        ])
        .await
        .unwrap();

    let base = CallsQuery {
        time_range: TimeRange { start_us: ts - 1, end_us: ts + 120_000_000 },
        filter: DimensionFilter::default(),
        status_codes: vec![],
        finish_reasons: vec![],
        client_ips: vec![],
        server_ports: vec![],
        request_path_contains: None,
        is_stream: None,
        sort_by: "request_time".into(),
        sort_order: "desc".into(),
        page: 1,
        page_size: 10,
    };
    let page = backend.query_calls(&base).await.unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].id, "c2", "DESC by request_time");
    assert_eq!(page.items[0].request_time, (ts + 60_000_000) / 1000);

    // Filter narrows to one row.
    let filtered = CallsQuery {
        server_ports: vec![8080],
        request_path_contains: Some("chat/completions".into()),
        ..base.clone()
    };
    assert_eq!(backend.query_calls(&filtered).await.unwrap().total, 2);
    let none = CallsQuery { server_ports: vec![9999], ..base.clone() };
    assert_eq!(backend.query_calls(&none).await.unwrap().total, 0);

    let detail = backend.query_call_by_id("c1").await.unwrap().expect("c1");
    assert_eq!(detail.model, "gpt-4");
    assert_eq!(detail.total_tokens, Some(150));
    assert_eq!(detail.request_time, ts / 1000);
    assert!(backend.query_call_by_id("missing").await.unwrap().is_none());
}

#[tokio::test]
async fn query_turn_calls_no_join_two_step() {
    let backend = require_backend!("heron_it_turncalls");
    let ts = 1_700_000_000_000_000_i64;
    backend
        .write_calls(vec![
            fixtures::sample_call("tc1", ts),
            fixtures::sample_call("tc2", ts + 1_000_000),
        ])
        .await
        .unwrap();
    backend
        .write_turns(vec![fixtures::sample_turn(
            "turn-x",
            "sess-x",
            ts,
            vec!["tc1", "tc2"],
        )])
        .await
        .unwrap();

    let full = backend.query_turn_calls("turn-x", true).await.unwrap();
    assert_eq!(full.len(), 2);
    assert_eq!(full[0].id, "tc1", "ordered by request_time ASC");
    assert_eq!(full[0].sequence, 1);
    assert!(full[0].request_body.is_some(), "bodies included");

    let lite = backend.query_turn_calls("turn-x", false).await.unwrap();
    assert_eq!(lite.len(), 2);
    assert!(lite[0].request_body.is_none(), "lite drops bodies");

    let by_ids = backend
        .query_calls_by_ids(&["tc2".to_string()], true)
        .await
        .unwrap();
    assert_eq!(by_ids.len(), 1);
    assert_eq!(by_ids[0].id, "tc2");
}

const TS: i64 = 1_700_000_000_000_000;
fn full_range() -> TimeRange {
    TimeRange { start_us: TS - 1, end_us: TS + 3_600_000_000 }
}

#[tokio::test]
async fn metrics_reads_smoke() {
    let backend = require_backend!("heron_it_metrics");
    // Seed both the `('*','*','*')` rollup tier (read by the default-filter
    // summary/timeseries/finish queries) and the `(W,M,'*')` tier (read by the
    // model-axis + group-by-model queries), mirroring the live aggregator.
    backend
        .write_metrics(vec![
            fixtures::sample_metric_dim("10s", TS, "*", "*", "*"),
            fixtures::sample_metric_dim("1m", TS, "*", "*", "*"),
            fixtures::sample_metric_dim("10s", TS, "openai-chat", "gpt-4", "*"),
            fixtures::sample_metric_dim("1m", TS, "openai-chat", "gpt-4", "*"),
        ])
        .await
        .unwrap();
    backend
        .write_finish_metrics(vec![fixtures::sample_finish_metric_dim(
            "1m", TS, "*", "*", "*", "stop",
        )])
        .await
        .unwrap();

    // Timeseries (ungrouped + grouped) with a mix of sum/avg/percentile fields.
    let q = MetricsTimeseriesQuery {
        time_range: full_range(),
        granularity: "1m".into(),
        filter: DimensionFilter::default(),
        fields: vec!["call_count".into(), "ttft_avg".into(), "ttft_p95".into()],
        group_by: None,
    };
    let ts = backend.query_metrics_timeseries(&q).await.unwrap();
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].values.len(), 3);
    assert_eq!(ts[0].values[0], Some(1.0)); // call_count summed
    let grouped = MetricsTimeseriesQuery { group_by: Some("model".into()), ..q.clone() };
    let g = backend.query_metrics_timeseries(&grouped).await.unwrap();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].group.as_deref(), Some("gpt-4"));

    let summary = backend
        .query_metrics_summary(&MetricsSummaryQuery {
            time_range: full_range(),
            filter: DimensionFilter::default(),
        })
        .await
        .unwrap();
    assert_eq!(summary.call_count, 1);

    let models = backend
        .query_metrics_models(&MetricsModelsQuery {
            time_range: full_range(),
            filter: DimensionFilter::default(),
            sort_by: "call_count".into(),
            sort_order: "desc".into(),
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model, "gpt-4");

    let fr = backend
        .query_finish_reasons(&FinishReasonsQuery {
            time_range: full_range(),
            granularity: "1m".into(),
            wire_apis: vec![],
            models: vec![],
            server_ips: vec![],
        })
        .await
        .unwrap();
    assert_eq!(fr.len(), 1);
    assert_eq!(fr[0].finish_reason, "stop");
}

#[tokio::test]
async fn agent_and_turns_reads_smoke() {
    let backend = require_backend!("heron_it_turns");
    backend
        .write_calls(vec![fixtures::sample_call("ac1", TS)])
        .await
        .unwrap();
    backend
        .write_turns(vec![fixtures::sample_turn("t1", "s1", TS, vec!["ac1"])])
        .await
        .unwrap();

    // agent summary/activity (over agent_turns FINAL).
    let summ = backend
        .query_agent_summary(&AgentSummaryQuery { time_range: full_range() })
        .await
        .unwrap();
    assert_eq!(summ.len(), 1);
    assert_eq!(summ[0].agent_kind, "claude-cli");
    assert_eq!(summ[0].turn_count, 1);
    let act = backend
        .query_agent_activity(&AgentActivityQuery {
            time_range: full_range(),
            bucket_seconds: Some(60),
        })
        .await
        .unwrap();
    assert_eq!(act.len(), 1);

    // query_turns + by_id.
    let page = backend
        .query_turns(&TurnsQuery {
            time_range: full_range(),
            filter: DimensionFilter::default(),
            client_ips: vec![],
            server_ports: vec![],
            statuses: vec![],
            agent_kinds: vec![],
            sort_by: "start_time".into(),
            sort_order: "desc".into(),
            page: 1,
            page_size: 10,
            include_proxy_hops: false,
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].turn_id, "t1");
    assert_eq!(page.items[0].primary_model.as_deref(), Some("gpt-4"));

    let detail = backend.query_turn_by_id("t1").await.unwrap().expect("t1");
    assert_eq!(detail.call_ids, vec!["ac1".to_string()]);

    // pair candidates: the un-proxied turn shows up.
    let cands = backend.query_pair_candidates(TS - 1, TS + 3_600_000_000).await.unwrap();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].turn_id, "t1");

    // update_turn_metadata: merge a proxy role, then it's excluded from
    // candidates and visible in the detail metadata — and the row count stays 1
    // (ReplacingMergeTree FINAL dedup).
    backend
        .update_turn_metadata("t1", serde_json::json!({"proxy": {"role": "proxy_in"}}))
        .await
        .unwrap();
    assert_eq!(count(&backend, "agent_turns FINAL").await, 1);
    let after = backend.query_turn_by_id("t1").await.unwrap().expect("t1");
    assert_eq!(
        after.metadata.as_ref().and_then(|m| m.pointer("/proxy/role")).and_then(|v| v.as_str()),
        Some("proxy_in")
    );
    let cands2 = backend.query_pair_candidates(TS - 1, TS + 3_600_000_000).await.unwrap();
    assert_eq!(cands2.len(), 0, "proxy-tagged turn excluded from candidates");
    // update_turn_metadata on a missing turn is a silent no-op.
    backend
        .update_turn_metadata("nope", serde_json::json!({"x": 1}))
        .await
        .unwrap();
}

#[tokio::test]
async fn sessions_reads_smoke() {
    let backend = require_backend!("heron_it_sessions");
    backend
        .write_turns(vec![
            fixtures::sample_turn("st1", "sess-A", TS, vec!["c1"]),
            fixtures::sample_turn("st2", "sess-A", TS + 10_000_000, vec!["c2"]),
        ])
        .await
        .unwrap();

    let list = backend
        .query_sessions(&SessionListQuery {
            time_range: full_range(),
            source_id: None,
            agent_kinds: vec![],
            cursor: None,
            page_size: 10,
        })
        .await
        .unwrap();
    assert_eq!(list.items.len(), 1, "two turns collapse to one session");
    assert_eq!(list.items[0].session_id, "sess-A");
    assert_eq!(list.items[0].turn_count, 2);

    let detail = backend
        .query_session_by_id("src-0", "sess-A")
        .await
        .unwrap()
        .expect("session");
    assert_eq!(detail.turn_count, 2);

    let turns = backend
        .query_session_turns(&SessionTurnsQuery {
            source_id: "src-0".into(),
            session_id: "sess-A".into(),
            cursor: None,
            page_size: 10,
        })
        .await
        .unwrap();
    assert_eq!(turns.items.len(), 2);
}

#[tokio::test]
async fn exchanges_reads_smoke() {
    let backend = require_backend!("heron_it_exch");
    backend
        .write_exchanges(vec![fixtures::sample_exchange("x1", TS)])
        .await
        .unwrap();

    let detail = backend
        .query_http_exchange_by_id("x1")
        .await
        .unwrap()
        .expect("x1");
    assert_eq!(detail.method, "POST");
    assert_eq!(detail.status, Some(200));
    assert_eq!(detail.request_body.as_deref(), Some(r#"{"model":"gpt-4"}"#));

    let page = backend
        .query_http_exchanges(&HttpExchangesQuery {
            time_range: full_range(),
            server_ips: vec![],
            client_ips: vec![],
            methods: vec![],
            status_codes: vec![],
            uri_contains: Some("chat".into()),
            is_sse: None,
            sort_by: "request_time".into(),
            sort_order: "desc".into(),
            page: 1,
            page_size: 10,
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id, "x1");
}

#[tokio::test]
async fn distincts_reads_smoke() {
    let backend = require_backend!("heron_it_distincts");
    backend
        .write_metrics(vec![fixtures::sample_metric("1m", TS)])
        .await
        .unwrap();
    backend
        .write_finish_metrics(vec![fixtures::sample_finish_metric("1m", TS, "stop")])
        .await
        .unwrap();
    backend
        .write_turns(vec![fixtures::sample_turn("dt1", "ds1", TS, vec!["dc1"])])
        .await
        .unwrap();

    assert!(backend.query_distinct_wire_apis().await.unwrap().contains(&"openai-chat".to_string()));
    assert!(backend.query_distinct_models().await.unwrap().contains(&"gpt-4".to_string()));
    assert!(backend.query_distinct_server_ips().await.unwrap().contains(&"10.0.0.2".to_string()));
    let kinds = backend
        .query_distinct_agent_kinds(&DistinctAgentKindsQuery {
            time_range: full_range(),
            filter: DimensionFilter::default(),
            include_proxy_hops: true,
        })
        .await
        .unwrap();
    assert!(kinds.contains(&"claude-cli".to_string()));
    let frs = backend.query_distinct_finish_reasons().await.unwrap();
    assert!(frs.iter().any(|d| d.finish_reason == "stop"));
}

#[tokio::test]
async fn services_reads_smoke() {
    let backend = require_backend!("heron_it_services");
    backend
        .write_calls(vec![
            fixtures::sample_call("sc1", TS),
            fixtures::sample_call("sc2", TS + 1_000_000),
        ])
        .await
        .unwrap();

    let svcs = backend
        .query_services(&ServicesQuery {
            time_range: full_range(),
            sort_by: "call_count".into(),
            sort_order: "desc".into(),
            limit: 50,
        })
        .await
        .unwrap();
    assert_eq!(svcs.len(), 1, "one (server_ip, server_port) endpoint");
    assert_eq!(svcs[0].server_ip, "10.0.0.2");
    assert_eq!(svcs[0].server_port, 8080);
    assert_eq!(svcs[0].call_count, 2);

    let topo = backend
        .query_services_topology(&ServicesTopologyQuery { time_range: full_range() })
        .await
        .unwrap();
    assert!(!topo.nodes.is_empty());
}

#[tokio::test]
async fn retention_deletes_old_rows() {
    use std::time::SystemTime;
    let backend = require_backend!("heron_it_retention");
    backend
        .write_calls(vec![fixtures::sample_call("rc1", TS)])
        .await
        .unwrap();
    backend
        .write_turns(vec![fixtures::sample_turn("rt1", "rs1", TS, vec!["rc1"])])
        .await
        .unwrap();
    backend
        .write_metrics(vec![fixtures::sample_metric("1m", TS)])
        .await
        .unwrap();
    assert_eq!(count(&backend, "llm_calls").await, 1);

    // Cutoff = now → everything older than now (the 2023 fixtures) is deleted.
    let policy = RetentionPolicy {
        calls_before: Some(SystemTime::now()),
        turns_before: Some(SystemTime::now()),
        http_exchanges_before: None,
        metrics_before: vec![("1m".to_string(), SystemTime::now())],
    };
    let report = backend.apply_retention(policy).await.unwrap();
    assert_eq!(report.calls_deleted, 1);
    assert_eq!(report.turns_deleted, 1);
    assert_eq!(report.metrics_deleted.get("1m").copied(), Some(1));
    assert_eq!(count(&backend, "llm_calls").await, 0, "old calls swept");
}
