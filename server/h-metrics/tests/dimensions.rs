//! Verify that calls with different `tool_surface` values produce distinct
//! metric rows after a cadence drain.

use std::net::IpAddr;
use std::sync::Arc;

use h_common::agent::ToolSurface;
use h_common::internal_metrics::{Metric, MetricsSystem, MetricsWorker};
use h_llm::model::{ApiType, LlmCall, LlmEvent};
use h_llm::wire_apis as wa;
use h_metrics::aggregator::MetricsAggregator;

fn metrics() -> MetricsWorker {
    let mut sys = MetricsSystem::new();
    let w = sys.register_worker(
        "test-dim",
        &[
            Metric::MetricsLlmEventsStart,
            Metric::MetricsLlmEventsComplete,
            Metric::MetricsHeartbeatsReceived,
            Metric::MetricsWindowsEmitted,
        ],
    );
    let _svc = sys.start();
    w
}

fn make_completed_call(request_time: i64, tool_surface: Option<ToolSurface>) -> LlmEvent {
    LlmEvent::Complete {
        call: Arc::new(LlmCall {
            source_id: String::new(),
            id: format!("c-{request_time}-{tool_surface:?}"),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            request_time,
            response_time: Some(request_time + 100_000),
            complete_time: Some(request_time + 500_000),
            request_path: "/v1/chat/completions".to_string(),
            is_stream: true,
            request_body: None,
            status_code: Some(200),
            finish_reason: Some("stop".to_string()),
            response_body: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: Some(50.0),
            e2e_latency_ms: Some(400.0),
            client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
            client_port: 12345,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            server_port: 443,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
            is_agent_request: tool_surface.is_some(),
            tool_surface,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
        }),
        agent: None,
    }
}

#[test]
fn distinct_surfaces_produce_distinct_metric_rows() {
    let mut agg = MetricsAggregator::new(metrics());
    let t0 = 1_700_000_000_000_000i64;

    // Two completes into the same window/dim, differing only in tool_surface.
    agg.process(&make_completed_call(t0, Some(ToolSurface::FunctionCall)));
    agg.process(&make_completed_call(t0, Some(ToolSurface::Mcp)));

    let batches = agg.flush_all();
    // Aggregator emits per-dim (4 fanned dims) per granularity (4) per surface.
    // We only assert on the exact-dim 10s row to keep the check tight.
    let rows: Vec<_> = batches
        .iter()
        .filter(|b| {
            b.metric.granularity == "10s"
                && b.metric.timestamp_us == t0
                && b.metric.wire_api == wa::OPENAI_CHAT
                && b.metric.model == "gpt-4"
                && b.metric.server_ip == "10.0.0.1"
        })
        .collect();

    let surfaces: Vec<_> = rows.iter().map(|r| r.metric.tool_surface.clone()).collect();
    assert!(
        surfaces.contains(&Some("function_call".to_string())),
        "missing function_call row, got {surfaces:?}",
    );
    assert!(
        surfaces.contains(&Some("mcp".to_string())),
        "missing mcp row, got {surfaces:?}",
    );
    assert_eq!(
        rows.len(),
        2,
        "expected exactly two rows on exact dim (one per surface), got {rows:?}",
    );
}

#[test]
fn complete_with_no_surface_keeps_none_dimension() {
    let mut agg = MetricsAggregator::new(metrics());
    let t0 = 1_700_000_000_000_000i64;
    agg.process(&make_completed_call(t0, None));

    let batches = agg.flush_all();
    let exact_dim: Vec<_> = batches
        .iter()
        .filter(|b| {
            b.metric.granularity == "10s"
                && b.metric.timestamp_us == t0
                && b.metric.wire_api == wa::OPENAI_CHAT
                && b.metric.model == "gpt-4"
                && b.metric.server_ip == "10.0.0.1"
        })
        .collect();
    assert_eq!(exact_dim.len(), 1, "got {exact_dim:?}");
    assert_eq!(exact_dim[0].metric.tool_surface, None);
}
