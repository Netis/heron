//! Storage-backend benchmark: write throughput + read latency, driven through
//! the `StorageBackend` trait so DuckDB and ClickHouse run the identical
//! workload for a fair comparison.
//!
//! ```bash
//! # DuckDB (embedded, single file)
//! storage_bench --backend duckdb --duckdb-path /tmp/bench.duckdb --calls 200000
//! # ClickHouse (server over its HTTP interface)
//! storage_bench --backend clickhouse --ch-url http://localhost:8123 --calls 200000
//! ```
//!
//! Emits a JSON result object on stdout (and a human table on stderr). Run both
//! backends on the SAME host (ClickHouse on loopback) for an apples-to-apples
//! engine comparison; see `scripts/bench-storage.sh`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;

use h_common::config::StorageConfig;
use h_llm::model::{ApiType, LlmCall};
use h_llm::wire_apis as wa;
use h_metrics::model::LlmMetric;
use h_storage::query::*;
use h_storage::StorageBackend;
use h_turn::{Trace, TraceStatus};
use heron::create_backend;

#[derive(Parser, Debug)]
#[command(about = "Heron storage backend write/read benchmark")]
struct Args {
    /// Backend to benchmark: "duckdb" or "clickhouse".
    #[arg(long, default_value = "duckdb")]
    backend: String,
    /// Number of llm_calls to write.
    #[arg(long, default_value_t = 100_000)]
    calls: usize,
    /// Number of agent_turns to write.
    #[arg(long, default_value_t = 20_000)]
    turns: usize,
    /// Number of llm_metrics rows to write.
    #[arg(long, default_value_t = 50_000)]
    metrics: usize,
    /// Rows per write batch (one INSERT / appender flush per batch).
    #[arg(long, default_value_t = 1000)]
    batch: usize,
    /// Approx request+response body size per call, in bytes (realistic payload).
    #[arg(long, default_value_t = 2048)]
    body_bytes: usize,
    /// ClickHouse HTTP URL (or env CLICKHOUSE_URL). Used when backend=clickhouse.
    #[arg(long)]
    ch_url: Option<String>,
    /// DuckDB file path. Used when backend=duckdb.
    #[arg(long, default_value = "/tmp/heron-bench.duckdb")]
    duckdb_path: String,
    /// Iterations per read query when measuring latency.
    #[arg(long, default_value_t = 30)]
    query_iters: usize,
}

/// Window the synthetic timestamps span (1 hour), so time-ranged reads hit data.
const WINDOW_US: i64 = 3_600_000_000;
const BASE_US: i64 = 1_700_000_000_000_000;

fn body_of(size: usize) -> String {
    // A JSON-ish payload of the requested size with a real `usage` block so
    // `tokens_estimated` derivation runs the same code path on reads.
    let filler = "x".repeat(size.saturating_sub(80));
    format!(
        r#"{{"model":"gpt-4","usage":{{"prompt_tokens":1200,"completion_tokens":400}},"pad":"{filler}"}}"#
    )
}

fn make_call(i: usize, body: &str) -> LlmCall {
    let ts = BASE_US + (i as i64 * WINDOW_US / 100_000).min(WINDOW_US);
    LlmCall {
        source_id: "bench".into(),
        id: format!("call-{i:012}"),
        wire_api: wa::OPENAI_CHAT,
        model: if i % 3 == 0 { "gpt-4o".into() } else { "gpt-4".into() },
        api_type: ApiType::Chat,
        request_time: ts,
        response_time: Some(ts + 200_000),
        complete_time: Some(ts + 1_500_000),
        request_path: "/v1/chat/completions".into(),
        is_stream: i % 2 == 0,
        request_body: Some(body.to_string()),
        status_code: Some(if i % 50 == 0 { 500 } else { 200 }),
        finish_reason: Some("stop".into()),
        response_body: Some(body.to_string()),
        input_tokens: Some(1200),
        output_tokens: Some(400),
        total_tokens: Some(1600),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        ttft_ms: Some(200.0),
        e2e_latency_ms: Some(1500.0),
        client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
        client_port: 50000 + (i % 10000) as u16,
        server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
        server_port: 8080,
        response_id: Some(format!("chatcmpl-{i}")),
        request_headers: vec![("content-type".into(), "application/json".into())],
        response_headers: vec![("server".into(), "uvicorn".into())],
        is_agent_request: true,
        tool_surface: None,
        agent_topology: None,
        tool_call_count: 0,
        tool_names: vec![],
        body_bytes_dropped: 0,
        process: None,
    }
}

fn make_turn(i: usize) -> Trace {
    let ts = BASE_US + (i as i64 * WINDOW_US / 20_000).min(WINDOW_US);
    Trace {
        source_id: "bench".into(),
        turn_id: format!("turn-{i:012}"),
        session_id: format!("sess-{}", i % 2000),
        wire_api: wa::OPENAI_CHAT.into(),
        agent_kind: if i % 2 == 0 { "claude-cli".into() } else { "codex-cli".into() },
        client_ip: "10.0.0.1".parse().unwrap(),
        server_ip: "10.0.0.2".parse().unwrap(),
        start_time_us: ts,
        end_time_us: ts + 5_000_000,
        duration_ms: 5000,
        call_count: 3,
        models_used: vec!["gpt-4".into()],
        subagents_used: vec![],
        total_input_tokens: 3600,
        total_output_tokens: 1200,
        total_cache_read_input_tokens: 0,
        total_cache_creation_input_tokens: 0,
        total_cost_usd: Some(0.05),
        status: TraceStatus::Complete,
        final_finish_reason: Some("stop".into()),
        user_input_preview: Some("hello".into()),
        user_call_id: Some(format!("call-{:012}", i * 3)),
        final_answer_preview: Some("done".into()),
        final_call_id: Some(format!("call-{:012}", i * 3 + 2)),
        span_ids: vec![format!("call-{:012}", i * 3)],
        metadata: serde_json::json!({}),
        tool_surfaces: vec![],
        tool_call_total: 0,
        agent_topology: None,
        suspicious_skills: vec![],
    }
}

fn make_metric(i: usize) -> LlmMetric {
    // Alternate the wildcard rollup tier and the (W,M,*) tier so default-filter
    // and model-axis reads both find rows, mirroring the live aggregator.
    let wildcard = i % 2 == 0;
    let ts = BASE_US + (i as i64 * WINDOW_US / 50_000).min(WINDOW_US);
    LlmMetric {
        timestamp_us: ts,
        source_id: "bench".into(),
        granularity: if i % 4 == 0 { "10s" } else { "1m" },
        wire_api: if wildcard { "*".into() } else { wa::OPENAI_CHAT.into() },
        model: if wildcard { "*".into() } else { "gpt-4".into() },
        server_ip: "*".into(),
        call_count: 10,
        stream_count: 5,
        non_stream_count: 5,
        active_calls_sum: 30,
        active_calls_sample_count: 10,
        active_calls_max: 8,
        total_input_tokens: 12000,
        input_token_count: 10,
        total_output_tokens: 4000,
        output_token_count: 10,
        total_cache_read_input_tokens: 0,
        total_cache_creation_input_tokens: 0,
        error_count: 0,
        error_4xx_count: 0,
        error_429_count: 0,
        error_5xx_count: 0,
        ttft_sum: 2000.0,
        ttft_count: 10,
        ttft_p50: Some(180.0),
        ttft_p95: Some(350.0),
        ttft_p99: Some(500.0),
        ttft_stream_sum: 1000.0,
        ttft_stream_count: 5,
        ttft_stream_p50: Some(120.0),
        ttft_stream_p95: Some(200.0),
        ttft_stream_p99: Some(260.0),
        ttft_nonstream_sum: 1000.0,
        ttft_nonstream_count: 5,
        ttft_nonstream_p50: Some(240.0),
        ttft_nonstream_p95: Some(420.0),
        ttft_nonstream_p99: Some(520.0),
        e2e_sum: 15000.0,
        e2e_count: 10,
        e2e_p50: Some(1400.0),
        e2e_p95: Some(2500.0),
        e2e_p99: Some(3800.0),
        tpot_sum: 220.0,
        tpot_count: 10,
        tpot_p50: Some(22.0),
        tpot_p95: Some(30.0),
        tpot_p99: Some(40.0),
        tool_surface: None,
    }
}

/// Run `f` `iters` times, return (p50_ms, p95_ms, mean_ms).
async fn time_query<F, Fut>(iters: usize, mut f: F) -> (f64, f64, f64)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        f().await;
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    (p(0.5), p(0.95), mean)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let mut cfg = StorageConfig::default();
    cfg.backend = args.backend.clone();
    cfg.duckdb.path = args.duckdb_path.clone();
    if let Some(url) = args.ch_url.clone().or_else(|| std::env::var("CLICKHOUSE_URL").ok()) {
        cfg.clickhouse.url = url;
    }
    cfg.clickhouse.database = "heron_bench".into();

    let backend: Arc<dyn StorageBackend> = create_backend(&cfg)?;
    backend.init().await?;

    let body = body_of(args.body_bytes);

    // ---- Write throughput ----
    let t = Instant::now();
    for chunk_start in (0..args.calls).step_by(args.batch) {
        let end = (chunk_start + args.batch).min(args.calls);
        let batch: Vec<LlmCall> = (chunk_start..end).map(|i| make_call(i, &body)).collect();
        backend.write_spans(batch).await?;
    }
    let calls_secs = t.elapsed().as_secs_f64();
    let calls_per_sec = args.calls as f64 / calls_secs;

    let t = Instant::now();
    for chunk_start in (0..args.metrics).step_by(args.batch) {
        let end = (chunk_start + args.batch).min(args.metrics);
        let batch: Vec<LlmMetric> = (chunk_start..end).map(make_metric).collect();
        backend.write_metrics(batch).await?;
    }
    let metrics_per_sec = args.metrics as f64 / t.elapsed().as_secs_f64();

    let t = Instant::now();
    for chunk_start in (0..args.turns).step_by(args.batch) {
        let end = (chunk_start + args.batch).min(args.turns);
        let batch: Vec<Trace> = (chunk_start..end).map(make_turn).collect();
        backend.write_traces(batch).await?;
    }
    let turns_per_sec = args.turns as f64 / t.elapsed().as_secs_f64();

    // ---- Read latency ----
    let range = TimeRange { start_us: BASE_US - 1, end_us: BASE_US + WINDOW_US + 1 };

    let (qc_p50, qc_p95, qc_mean) = time_query(args.query_iters, || {
        let b = backend.clone();
        let r = range.clone();
        async move {
            let _ = b
                .query_spans(&SpansQuery {
                    time_range: r,
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
                    page_size: 50,
                })
                .await
                .unwrap();
        }
    })
    .await;

    let (qs_p50, qs_p95, qs_mean) = time_query(args.query_iters, || {
        let b = backend.clone();
        let r = range.clone();
        async move {
            let _ = b
                .query_metrics_summary(&MetricsSummaryQuery {
                    time_range: r,
                    filter: DimensionFilter::default(),
                })
                .await
                .unwrap();
        }
    })
    .await;

    let (qt_p50, qt_p95, qt_mean) = time_query(args.query_iters, || {
        let b = backend.clone();
        let r = range.clone();
        async move {
            let _ = b
                .query_metrics_timeseries(&MetricsTimeseriesQuery {
                    time_range: r,
                    granularity: "1m".into(),
                    filter: DimensionFilter::default(),
                    fields: vec!["call_count".into(), "ttft_avg".into(), "e2e_p95".into()],
                    group_by: None,
                })
                .await
                .unwrap();
        }
    })
    .await;

    let (qn_p50, qn_p95, qn_mean) = time_query(args.query_iters, || {
        let b = backend.clone();
        let r = range.clone();
        async move {
            let _ = b
                .query_traces(&TracesQuery {
                    time_range: r,
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
                })
                .await
                .unwrap();
        }
    })
    .await;

    let (qv_p50, qv_p95, qv_mean) = time_query(args.query_iters, || {
        let b = backend.clone();
        let r = range.clone();
        async move {
            let _ = b
                .query_services(&ServicesQuery {
                    time_range: r,
                    sort_by: "call_count".into(),
                    sort_order: "desc".into(),
                    limit: 50,
                })
                .await
                .unwrap();
        }
    })
    .await;

    // ---- Output ----
    let result = serde_json::json!({
        "backend": args.backend,
        "rows": { "calls": args.calls, "metrics": args.metrics, "turns": args.turns },
        "body_bytes": args.body_bytes,
        "batch": args.batch,
        "write_rows_per_sec": {
            "calls": calls_per_sec.round(),
            "metrics": metrics_per_sec.round(),
            "turns": turns_per_sec.round(),
        },
        "query_ms": {
            "query_spans":      { "p50": qc_p50, "p95": qc_p95, "mean": qc_mean },
            "query_summary":    { "p50": qs_p50, "p95": qs_p95, "mean": qs_mean },
            "query_timeseries": { "p50": qt_p50, "p95": qt_p95, "mean": qt_mean },
            "query_traces":      { "p50": qn_p50, "p95": qn_p95, "mean": qn_mean },
            "query_services":   { "p50": qv_p50, "p95": qv_p95, "mean": qv_mean },
        },
    });

    eprintln!("\n=== {} ===", args.backend);
    eprintln!(
        "write calls/s   : {:>12}",
        calls_per_sec.round() as u64
    );
    eprintln!("write metrics/s : {:>12}", metrics_per_sec.round() as u64);
    eprintln!("write turns/s   : {:>12}", turns_per_sec.round() as u64);
    eprintln!("query (p50 ms)  calls={qc_p50:.2} summary={qs_p50:.2} timeseries={qt_p50:.2} turns={qn_p50:.2} services={qv_p50:.2}");
    println!("{result}");
    Ok(())
}
