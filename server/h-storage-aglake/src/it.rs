//! Live-server integration tests, gated on `AGLAKE_TEST_URL`.
//!
//! aglake has no in-process mode, so these need a real aglaked. When
//! `AGLAKE_TEST_URL` is unset every test self-skips, keeping
//! `cargo test --workspace` green without a server. To run them:
//!
//! ```bash
//! D=$(mktemp -d) && aglaked --data-dir "$D" --listen 127.0.0.1:5970 \
//!     --hec-token heron-it --no-self-trace --max-hot-raw-mib 2048 &
//! AGLAKE_TEST_URL=http://127.0.0.1:5970 AGLAKE_TEST_TOKEN=heron-it \
//!     cargo test -p h-storage-aglake
//! ```
//!
//! That daemon has no user catalog, so `/api/v1/*` is open. Against one that
//! has users, add `AGLAKE_TEST_USER` / `AGLAKE_TEST_PASSWORD`; the retention
//! tests additionally need that account to hold the `admin` role.
//!
//! **Isolation is per index prefix, not per database.** aglake has no
//! `DROP DATABASE` and no DDL at all — an index exists because something wrote
//! to it. Each test therefore gets a unique `index_prefix` and simply never
//! looks at anyone else's indexes. Nothing is cleaned up; the test data-dir is
//! disposable.
//!
//! **Reads poll instead of sleeping.** A write is durable when HEC returns 200,
//! but "durable" and "searchable" are not the same instant. Polling to a
//! deadline both keeps the suite fast and makes the actual visibility lag
//! observable rather than papered over by a fixed sleep.

#![cfg(test)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use std::net::IpAddr;
use std::sync::Arc;

use h_common::config::AglakeConfig;
use h_metrics::model::LlmFinishMetric;
use h_protocol::model::{HttpRequestData, HttpResponseData};
use h_protocol::net::FlowKey;
use h_protocol::HttpExchange;
use h_storage::query::*;
use h_storage::StorageBackend;

use crate::rows::fixtures;
use crate::AglakeBackend;

/// How long a write may take to become searchable before a test gives up.
const VISIBLE_WITHIN: Duration = Duration::from_secs(20);

static PREFIX_SEQ: AtomicU32 = AtomicU32::new(0);

/// Build a backend against a fresh, unused index prefix. Returns `None` (test
/// self-skips) when `AGLAKE_TEST_URL` is unset.
async fn fresh_backend() -> Option<AglakeBackend> {
    let url = std::env::var("AGLAKE_TEST_URL").ok()?;
    // Unique per process *and* per test, so a re-run against a persistent
    // data-dir never reads a previous run's events.
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let seq = PREFIX_SEQ.fetch_add(1, Ordering::Relaxed);
    let cfg = AglakeConfig {
        url,
        hec_token: std::env::var("AGLAKE_TEST_TOKEN").unwrap_or_default(),
        index_prefix: format!("it{seq}{}", &nonce[..12]),
        ..test_credentials()
    };
    let backend = AglakeBackend::new(&cfg).expect("build backend");
    backend.init().await.expect("init");
    Some(backend)
}

/// Credentials for an aglaked started with a user catalog, as a partial
/// config to spread over. Empty — the no-auth daemon these tests document —
/// unless `AGLAKE_TEST_USER` is set.
fn test_credentials() -> AglakeConfig {
    AglakeConfig {
        username: std::env::var("AGLAKE_TEST_USER").unwrap_or_default(),
        password: std::env::var("AGLAKE_TEST_PASSWORD").unwrap_or_default(),
        ..Default::default()
    }
}

macro_rules! require_backend {
    () => {
        match crate::it::fresh_backend().await {
            Some(b) => b,
            None => {
                eprintln!("skip: AGLAKE_TEST_URL unset");
                return;
            }
        }
    };
}

/// Poll `f` until it returns `Some`, or fail with how long we waited.
async fn eventually<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let start = Instant::now();
    loop {
        if let Some(v) = f().await {
            let waited = start.elapsed();
            if waited > Duration::from_secs(1) {
                eprintln!("note: {what} became searchable after {waited:?}");
            }
            return v;
        }
        assert!(
            start.elapsed() < VISIBLE_WITHIN,
            "{what} never became searchable within {VISIBLE_WITHIN:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn sample_exchange(id: &str, request_time_us: i64) -> HttpExchange {
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
        process: None,
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
        process: None,
    });
    HttpExchange {
        id: id.to_string(),
        request,
        response,
        sse_event_count: 0,
        sse_data_bytes: 0,
    }
}

/// The field-by-field acceptance check: one span written, one span read, every
/// value compared. This is the test that catches the encoding hazards —
/// microsecond truncation, `None` decaying into a zero value, a `Vec` losing
/// its order or collapsing to a scalar.
#[tokio::test]
async fn span_round_trips_every_field() {
    let backend = require_backend!();
    let call = fixtures::full_call();
    backend.write_spans(vec![call.clone()]).await.unwrap();

    let got = eventually("span", || async {
        backend.query_span_by_id(&call.id).await.unwrap()
    })
    .await;

    assert_eq!(got.id, call.id);
    assert_eq!(got.source_id, call.source_id);
    // Detail timestamps are milliseconds. The point of the integer `ts_us`
    // field is that this survives exactly, with no f64 rounding.
    assert_eq!(got.request_time, call.request_time / 1000);
    assert_eq!(got.response_time, Some(call.response_time.unwrap() / 1000));
    assert_eq!(got.complete_time, Some(call.complete_time.unwrap() / 1000));
    assert_eq!(got.wire_api, call.wire_api);
    assert_eq!(got.model, call.model);
    assert_eq!(got.api_type, call.api_type.to_string());
    assert_eq!(got.is_stream, call.is_stream);
    assert_eq!(got.request_path, call.request_path);
    assert_eq!(got.status_code, call.status_code);
    assert_eq!(got.finish_reason, call.finish_reason);
    assert_eq!(got.input_tokens, call.input_tokens);
    assert_eq!(got.output_tokens, call.output_tokens);
    assert_eq!(got.total_tokens, call.total_tokens);
    assert_eq!(got.ttft_ms, call.ttft_ms);
    assert_eq!(got.e2e_latency_ms, call.e2e_latency_ms);
    assert_eq!(got.response_id, call.response_id);
    assert_eq!(got.client_ip, call.client_ip.to_string());
    assert_eq!(got.client_port, call.client_port);
    assert_eq!(got.server_ip, call.server_ip.to_string());
    assert_eq!(got.server_port, call.server_port);
    assert_eq!(got.is_agent_request, call.is_agent_request);
    assert_eq!(got.tool_surface.as_deref(), Some("function_call"));
    assert_eq!(got.agent_topology.as_deref(), Some("single_agent"));
    assert_eq!(got.tool_call_count, call.tool_call_count);
    // Order matters and must not be reordered or collapsed.
    assert_eq!(got.tool_names, call.tool_names);
    assert_eq!(got.process.as_ref().unwrap().pid, 4242);
    assert_eq!(got.process.as_ref().unwrap().comm, "node");
    assert_eq!(
        got.process.as_ref().unwrap().exe.as_deref(),
        Some("/usr/bin/node")
    );
    // Bodies come from the second index.
    assert_eq!(got.request_body, call.request_body);
    assert_eq!(got.response_body, call.response_body);
    assert!(got.request_headers.unwrap().contains("api.example"));
    assert!(got.response_headers.unwrap().contains("uvicorn"));
}

/// The null-vs-absent case. Every `Option` is `None` on the way in, and every
/// one has to still be `None` on the way out — not `Some(0)`, not `Some("")`.
#[tokio::test]
async fn minimal_span_keeps_every_none() {
    let backend = require_backend!();
    let mut call = fixtures::minimal_call();
    call.id = uuid::Uuid::now_v7().to_string();
    backend.write_spans(vec![call.clone()]).await.unwrap();

    let got = eventually("minimal span", || async {
        backend.query_span_by_id(&call.id).await.unwrap()
    })
    .await;

    assert_eq!(got.response_time, None);
    assert_eq!(got.complete_time, None);
    assert_eq!(got.status_code, None);
    assert_eq!(got.finish_reason, None);
    assert_eq!(got.input_tokens, None);
    assert_eq!(got.output_tokens, None);
    assert_eq!(got.total_tokens, None);
    assert_eq!(got.ttft_ms, None);
    assert_eq!(got.e2e_latency_ms, None);
    assert_eq!(got.response_id, None);
    assert_eq!(got.tool_surface, None);
    assert_eq!(got.agent_topology, None);
    assert!(got.tool_names.is_empty());
    assert!(got.process.is_none());
    // No bodies and no headers were stored, so there is no body event at all.
    assert_eq!(got.request_body, None);
    assert_eq!(got.response_body, None);
}

#[tokio::test]
async fn unknown_ids_return_none_not_an_error() {
    let backend = require_backend!();
    assert!(backend
        .query_span_by_id(&uuid::Uuid::now_v7().to_string())
        .await
        .unwrap()
        .is_none());
    assert!(backend
        .query_trace_by_id("no-such-turn")
        .await
        .unwrap()
        .is_none());
    assert!(backend
        .query_http_exchange_by_id("no-such-exchange")
        .await
        .unwrap()
        .is_none());
    assert!(backend
        .query_trace_spans("no-such-turn", true)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn trace_round_trips_and_resolves_its_spans() {
    let backend = require_backend!();
    let mut trace = fixtures::full_trace();
    trace.turn_id = uuid::Uuid::now_v7().to_string();

    // Three calls, written out of order, with the middle two sharing a
    // microsecond so the tie-break is actually exercised.
    let base = trace.start_time_us;
    let mut calls = Vec::new();
    for (i, offset) in [2_000_000_i64, 1_000_000, 1_000_000]
        .into_iter()
        .enumerate()
    {
        let mut c = fixtures::full_call();
        c.id = format!("{}-span-{i}", trace.turn_id);
        c.request_time = base + offset;
        c.complete_time = Some(base + offset + 100_000);
        calls.push(c);
    }
    trace.span_ids = calls.iter().map(|c| c.id.clone()).collect();
    trace.end_time_us = base + 3_000_000;

    backend.write_spans(calls.clone()).await.unwrap();
    backend.write_traces(vec![trace.clone()]).await.unwrap();

    let detail = eventually("trace", || async {
        backend.query_trace_by_id(&trace.turn_id).await.unwrap()
    })
    .await;
    assert_eq!(detail.turn_id, trace.turn_id);
    assert_eq!(detail.session_id, trace.session_id);
    assert_eq!(detail.status, "complete");
    assert_eq!(detail.call_count, trace.call_count);
    assert_eq!(detail.start_time, trace.start_time_us / 1000);
    assert_eq!(detail.end_time, trace.end_time_us / 1000);
    assert_eq!(detail.models_used, trace.models_used);
    assert_eq!(detail.subagents_used, trace.subagents_used);
    assert_eq!(detail.span_ids, trace.span_ids);
    assert_eq!(detail.total_cost_usd, trace.total_cost_usd);
    assert_eq!(detail.total_input_tokens, trace.total_input_tokens);
    assert_eq!(detail.tool_surfaces, vec!["function_call".to_string()]);
    assert_eq!(detail.metadata.unwrap()["proxy"]["pair_id"], "group-9");

    let spans = eventually("trace spans", || async {
        let s = backend
            .query_trace_spans(&trace.turn_id, true)
            .await
            .unwrap();
        (s.len() == 3).then_some(s)
    })
    .await;

    // Ascending by request time, then completion, then id — deterministic even
    // for the two spans sharing a microsecond.
    assert_eq!(spans[0].id, format!("{}-span-1", trace.turn_id));
    assert_eq!(spans[1].id, format!("{}-span-2", trace.turn_id));
    assert_eq!(spans[2].id, format!("{}-span-0", trace.turn_id));
    assert_eq!(
        spans.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(spans[0].request_time <= spans[2].request_time);
    assert!(spans[0].request_body.is_some());

    // Lite mode drops exactly the four heavy fields and nothing else.
    let lite = backend
        .query_trace_spans(&trace.turn_id, false)
        .await
        .unwrap();
    assert_eq!(lite.len(), 3);
    for (full, lite) in spans.iter().zip(lite.iter()) {
        assert_eq!(full.id, lite.id);
        assert_eq!(full.sequence, lite.sequence);
        assert_eq!(full.request_time, lite.request_time);
        assert_eq!(full.input_tokens, lite.input_tokens);
        // Precomputed at write time, so unlike the SQL backends this stays
        // correct without the body.
        assert_eq!(full.tokens_estimated, lite.tokens_estimated);
        assert!(lite.request_body.is_none());
        assert!(lite.response_body.is_none());
        assert!(lite.request_headers.is_none());
        assert!(lite.response_headers.is_none());
    }

    // Same rows, reached by the in-memory-registry path instead of the trace.
    let by_ids = backend
        .query_spans_by_ids(&trace.span_ids, false)
        .await
        .unwrap();
    assert_eq!(
        by_ids.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        spans.iter().map(|s| s.id.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn http_exchange_round_trips_with_microsecond_times() {
    let backend = require_backend!();
    let id = uuid::Uuid::now_v7().to_string();
    let ts = 1_785_638_114_914_200_i64;
    let x = sample_exchange(&id, ts);
    backend.write_exchanges(vec![x]).await.unwrap();

    let got = eventually("exchange", || async {
        backend.query_http_exchange_by_id(&id).await.unwrap()
    })
    .await;

    assert_eq!(got.id, id);
    assert_eq!(got.method, "POST");
    assert_eq!(got.uri, "/v1/chat/completions");
    assert_eq!(got.status, Some(200));
    assert_eq!(got.client_ip, "10.0.0.1");
    assert_eq!(got.client_port, 54321);
    assert_eq!(got.server_ip, "10.0.0.2");
    assert_eq!(got.server_port, 443);
    assert!(!got.is_sse);
    assert_eq!(got.sse_event_count, 0);
    // Milliseconds, matching DuckDB. ClickHouse returns microseconds for this
    // one read and is the outlier — see the exchanges module docs.
    assert_eq!(got.request_time, ts / 1000);
    assert_eq!(got.response_first_byte_time, Some((ts + 500_000) / 1000));
    assert_eq!(got.response_complete_time, Some((ts + 1_000_000) / 1000));
    assert!(got.request_headers.contains("application/json"));
    assert!(got.response_headers.contains("req_abc"));
    assert_eq!(got.request_body.as_deref(), Some(r#"{"model":"gpt-4"}"#));
    assert_eq!(got.response_body.as_deref(), Some(r#"{"choices":[]}"#));
}

/// Metrics have no by-id read yet, so this asserts what Phase 1 can: the
/// writes are accepted and land in the granularity-specific index.
#[tokio::test]
async fn metrics_write_to_their_granularity_index() {
    let backend = require_backend!();
    let mut m = fixtures::sample_metric();
    m.granularity = "10s";
    let f = LlmFinishMetric {
        timestamp_us: m.timestamp_us,
        source_id: m.source_id.clone(),
        granularity: "10s".into(),
        wire_api: m.wire_api.clone(),
        model: m.model.clone(),
        server_ip: m.server_ip.clone(),
        finish_reason: "end_turn".into(),
        count: 4,
    };
    backend.write_metrics(vec![m]).await.unwrap();
    backend.write_finish_metrics(vec![f]).await.unwrap();

    let metrics_ix = backend.ix.metrics_for("10s").unwrap().to_string();
    let finish_ix = backend.ix.finish_for("10s").unwrap().to_string();
    for ix in [metrics_ix, finish_ix] {
        let backend = &backend;
        let ix = &ix;
        eventually(&format!("rows in {ix}"), || async move {
            let n = backend
                .search
                .search_all_time(&format!("search index={ix} | stats count as n | table n"))
                .await
                .unwrap()
                .rows()
                .first()
                .and_then(|r| r.get("n"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (n == 1).then_some(n)
        })
        .await;
    }
}

/// Empty writes must not produce an empty HTTP request — the sink calls these
/// on every flush tick whether or not anything accumulated.
#[tokio::test]
async fn empty_writes_are_no_ops() {
    let backend = require_backend!();
    backend.write_spans(vec![]).await.unwrap();
    backend.write_traces(vec![]).await.unwrap();
    backend.write_metrics(vec![]).await.unwrap();
    backend.write_finish_metrics(vec![]).await.unwrap();
    backend.write_exchanges(vec![]).await.unwrap();
    assert!(backend
        .query_spans_by_ids(&[], true)
        .await
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// Phase 2: lists + pagination
// ---------------------------------------------------------------------------

fn spans_query(page: u32, page_size: u32) -> SpansQuery {
    SpansQuery {
        time_range: TimeRange {
            start_us: 0,
            end_us: 4_000_000_000_000_000,
        },
        filter: DimensionFilter::default(),
        status_codes: vec![],
        finish_reasons: vec![],
        client_ips: vec![],
        server_ports: vec![],
        request_path_contains: None,
        is_stream: None,
        sort_by: "request_time".into(),
        sort_order: "DESC".into(),
        page,
        page_size,
    }
}

/// The test that would catch a broken pagination scheme: walk every page and
/// assert the union is exactly the full set, with nothing repeated and nothing
/// dropped. Unstable ordering, an off-by-one in the `sort N | tail L` window,
/// or a wrong `total` all show up here and nowhere else.
#[tokio::test]
async fn paging_covers_every_row_exactly_once() {
    let backend = require_backend!();
    let base = 1_785_638_000_000_000_i64;
    const N: usize = 23;

    let mut calls = Vec::new();
    for i in 0..N {
        let mut c = fixtures::full_call();
        c.id = format!("page-{i:03}");
        // Two spans deliberately share a microsecond, so the run also proves
        // the tie-break gives a total order.
        c.request_time = base + (i as i64 / 2) * 1_000_000;
        calls.push(c);
    }
    backend.write_spans(calls).await.unwrap();

    eventually("all spans", || async {
        let p = backend.query_spans(&spans_query(1, 100)).await.unwrap();
        (p.total == N as u64).then_some(p)
    })
    .await;

    for page_size in [1u32, 7, 200] {
        let mut seen: Vec<String> = Vec::new();
        let mut page = 1u32;
        loop {
            let p = backend
                .query_spans(&spans_query(page, page_size))
                .await
                .unwrap();
            assert_eq!(p.total, N as u64, "total must not drift between pages");
            if p.items.is_empty() {
                break;
            }
            assert!(
                p.items.len() <= page_size as usize,
                "page {page} returned {} rows for page_size {page_size}",
                p.items.len()
            );
            seen.extend(p.items.iter().map(|i| i.id.clone()));
            page += 1;
            assert!(page < 100, "pagination did not terminate");
        }
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            seen.len(),
            "page_size {page_size} repeated rows across pages"
        );
        assert_eq!(
            unique.len(),
            N,
            "page_size {page_size} lost rows: got {} of {N}",
            unique.len()
        );
    }
}

#[tokio::test]
async fn span_filters_narrow_the_result_and_the_total() {
    let backend = require_backend!();
    let base = 1_785_639_000_000_000_i64;
    let mut calls = Vec::new();
    for (i, (model, status, stream)) in [
        ("gpt-4", 200u16, true),
        ("gpt-4", 404, false),
        ("claude-sonnet", 200, true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut c = fixtures::full_call();
        c.id = format!("filt-{i}");
        c.request_time = base + i as i64 * 1_000_000;
        c.model = model.into();
        c.status_code = Some(status);
        c.is_stream = stream;
        c.request_path = format!("/v1/{}/completions", if stream { "chat" } else { "embed" });
        calls.push(c);
    }
    backend.write_spans(calls).await.unwrap();
    eventually("filter fixture", || async {
        let p = backend.query_spans(&spans_query(1, 50)).await.unwrap();
        (p.total == 3).then_some(p)
    })
    .await;

    let backend = &backend;
    let run = move |mutate: fn(&mut SpansQuery)| async move {
        let mut q = spans_query(1, 50);
        mutate(&mut q);
        backend.query_spans(&q).await.unwrap()
    };

    let p = run(|q| q.filter.models = vec!["gpt-4".into()]).await;
    assert_eq!(p.total, 2);
    assert_eq!(p.items.len(), 2);

    let p = run(|q| q.status_codes = vec![404]).await;
    assert_eq!(p.total, 1);
    assert_eq!(p.items[0].id, "filt-1");

    let p = run(|q| q.is_stream = Some(false)).await;
    assert_eq!(p.total, 1);
    assert_eq!(p.items[0].id, "filt-1");

    let p = run(|q| q.request_path_contains = Some("chat".into())).await;
    assert_eq!(p.total, 2);

    // A filter matching nothing must give an empty page, not everything.
    let p = run(|q| q.filter.models = vec!["no-such-model".into()]).await;
    assert_eq!(p.total, 0);
    assert!(p.items.is_empty());

    // An empty filter list means "no filter" — the trap that silently blanks
    // every page if it is treated as "match nothing".
    let p = run(|q| q.filter.models = vec![]).await;
    assert_eq!(p.total, 3);
}

/// A model name containing `*` must be compared literally. Under a search
/// term it would become a glob and match the other rows too.
#[tokio::test]
async fn wildcard_in_a_filter_value_matches_literally() {
    let backend = require_backend!();
    let base = 1_785_640_000_000_000_i64;
    let mut calls = Vec::new();
    for (i, model) in ["weird*name", "weirdXname", "other"]
        .into_iter()
        .enumerate()
    {
        let mut c = fixtures::full_call();
        c.id = format!("glob-{i}");
        c.request_time = base + i as i64 * 1_000_000;
        c.model = model.into();
        calls.push(c);
    }
    backend.write_spans(calls).await.unwrap();
    eventually("glob fixture", || async {
        let p = backend.query_spans(&spans_query(1, 50)).await.unwrap();
        (p.total == 3).then_some(p)
    })
    .await;

    let mut q = spans_query(1, 50);
    q.filter.models = vec!["weird*name".into()];
    let p = backend.query_spans(&q).await.unwrap();
    assert_eq!(p.total, 1, "`*` must not act as a wildcard");
    assert_eq!(p.items[0].model, "weird*name");
}

#[tokio::test]
async fn invalid_sort_and_deep_offset_are_refused() {
    let backend = require_backend!();

    let mut q = spans_query(1, 10);
    q.sort_by = "id; drop".into();
    assert!(
        backend.query_spans(&q).await.is_err(),
        "sort_by is whitelisted"
    );

    // Deep offset costs offset+limit rows to return limit of them; past the
    // configured ceiling that is refused rather than run.
    let mut q = spans_query(u32::MAX, 100);
    q.sort_by = "request_time".into();
    let err = backend.query_spans(&q).await.unwrap_err().to_string();
    assert!(err.contains("max_page_offset"), "{err}");
}

#[tokio::test]
async fn traces_list_hides_folded_proxy_hops_unless_asked() {
    let backend = require_backend!();
    let base = 1_785_641_000_000_000_i64;
    let mut traces = Vec::new();
    for (i, role) in [None, Some("proxy_in"), Some("proxy_out")]
        .into_iter()
        .enumerate()
    {
        let mut t = fixtures::full_trace();
        t.turn_id = format!("hop-{i}-{}", uuid::Uuid::now_v7());
        t.session_id = "hops".into();
        t.start_time_us = base + i as i64 * 1_000_000;
        t.end_time_us = t.start_time_us + 500_000;
        t.metadata = match role {
            Some(r) => serde_json::json!({ "proxy": { "role": r, "pair_id": "g1" } }),
            None => serde_json::json!({}),
        };
        traces.push(t);
    }
    backend.write_traces(traces.clone()).await.unwrap();

    let q = |include: bool| TracesQuery {
        time_range: TimeRange {
            start_us: base - 1_000_000,
            end_us: base + 60_000_000,
        },
        filter: DimensionFilter::default(),
        client_ips: vec![],
        server_ports: vec![],
        statuses: vec![],
        agent_kinds: vec![],
        sort_by: "start_time".into(),
        sort_order: "DESC".into(),
        page: 1,
        page_size: 50,
        include_proxy_hops: include,
    };

    let all = eventually("traces", || async {
        let p = backend.query_traces(&q(true)).await.unwrap();
        (p.total == 3).then_some(p)
    })
    .await;
    assert_eq!(all.items.len(), 3);

    // `proxy_out` is folded away by default; the other two stay.
    let visible = backend.query_traces(&q(false)).await.unwrap();
    assert_eq!(visible.total, 2);
    assert!(visible
        .items
        .iter()
        .all(|i| i.proxy_role.as_deref() != Some("proxy_out")));
    // The direct turn has no proxy role at all and must survive the filter —
    // the case a naive `role NOT IN (...)` would drop.
    assert!(visible.items.iter().any(|i| i.proxy_role.is_none()));
}

#[tokio::test]
async fn exchanges_list_paginates_and_reports_duration() {
    let backend = require_backend!();
    let base = 1_785_642_000_000_000_i64;
    let mut xs = Vec::new();
    for i in 0..5 {
        xs.push(sample_exchange(
            &format!("x-{i}-{}", uuid::Uuid::now_v7()),
            base + i * 1_000_000,
        ));
    }
    backend.write_exchanges(xs).await.unwrap();

    let q = |page: u32, page_size: u32| HttpExchangesQuery {
        time_range: TimeRange {
            start_us: base - 1_000_000,
            end_us: base + 60_000_000,
        },
        server_ips: vec![],
        client_ips: vec![],
        methods: vec![],
        status_codes: vec![],
        uri_contains: None,
        is_sse: None,
        sort_by: "request_time".into(),
        sort_order: "DESC".into(),
        page,
        page_size,
    };

    let p = eventually("exchanges", || async {
        let p = backend.query_http_exchanges(&q(1, 50)).await.unwrap();
        (p.total == 5).then_some(p)
    })
    .await;
    // Milliseconds, matching DuckDB — see the exchanges module docs.
    assert_eq!(p.items[0].request_time, (base + 4_000_000) / 1000);
    assert_eq!(p.items[0].duration_ms, Some(1000.0));
    assert_eq!(p.items[0].method, "POST");

    // Sorting on the eval-derived duration must not drop rows.
    let mut sorted = q(1, 50);
    sorted.sort_by = "duration_ms".into();
    assert_eq!(
        backend.query_http_exchanges(&sorted).await.unwrap().total,
        5
    );

    // Paging the list covers everything exactly once.
    let mut seen = Vec::new();
    for page in 1..=3 {
        seen.extend(
            backend
                .query_http_exchanges(&q(page, 2))
                .await
                .unwrap()
                .items
                .into_iter()
                .map(|i| i.id),
        );
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 5);

    let mut filtered = q(1, 50);
    filtered.uri_contains = Some("chat".into());
    assert_eq!(
        backend.query_http_exchanges(&filtered).await.unwrap().total,
        5
    );
    filtered.uri_contains = Some("nope".into());
    assert_eq!(
        backend.query_http_exchanges(&filtered).await.unwrap().total,
        0
    );
}

/// Sessions aggregate over a session's **whole life** but are selected by the
/// requested window — the distinction this test exists to pin down.
#[tokio::test]
async fn sessions_aggregate_full_lifetime_but_select_by_window() {
    let backend = require_backend!();
    let base = 1_785_643_000_000_000_i64;
    let sec = |n: i64| base + n * 1_000_000;

    let mk = |turn: &str, session: &str, start: i64, preview: Option<&str>| {
        let mut t = fixtures::full_trace();
        t.turn_id = turn.to_string();
        t.session_id = session.to_string();
        t.start_time_us = start;
        t.end_time_us = start + 500_000;
        t.call_count = 2;
        t.total_input_tokens = 100;
        t.total_cost_usd = Some(0.5);
        t.user_input_preview = preview.map(String::from);
        t.user_call_id = preview.map(|_| format!("call-{turn}"));
        t.metadata = serde_json::json!({});
        t
    };

    backend
        .write_traces(vec![
            // S1: one turn far outside the window, one inside.
            mk("s1-early", "S1", sec(10), Some("opening S1")),
            mk("s1-late", "S1", sec(50), None),
            // S2: both inside.
            mk("s2-a", "S2", sec(45), Some("opening S2")),
            mk("s2-b", "S2", sec(46), None),
            // S3: entirely outside.
            mk("s3", "S3", sec(500), Some("opening S3")),
        ])
        .await
        .unwrap();

    let q = |cursor: Option<SessionListCursor>, page_size: u32| SessionListQuery {
        time_range: TimeRange {
            start_us: sec(40),
            end_us: sec(60),
        },
        source_id: None,
        agent_kinds: vec![],
        cursor,
        page_size,
    };

    let page = eventually("sessions", || async {
        let p = backend.query_sessions(&q(None, 10)).await.unwrap();
        (p.items.len() == 2).then_some(p)
    })
    .await;

    // S1's in-window turn ends latest, so it sorts first.
    assert_eq!(page.items[0].session_id, "S1");
    assert_eq!(page.items[1].session_id, "S2");
    // Lifetime aggregates: both of S1's turns count, including the one
    // outside the window.
    assert_eq!(page.items[0].turn_count, 2);
    assert_eq!(page.items[0].call_count, 4);
    assert_eq!(page.items[0].total_input_tokens, 200);
    assert_eq!(page.items[0].total_cost_usd, Some(1.0));
    // …and first_turn_at is the lifetime minimum, not the in-window one.
    assert_eq!(page.items[0].first_turn_at, sec(10) / 1000);
    assert_eq!(
        page.items[0].last_turn_at_in_window,
        (sec(50) + 500_000) / 1000
    );
    // The preview comes from the earliest turn of the session.
    assert_eq!(
        page.items[0].first_user_input_preview.as_deref(),
        Some("opening S1")
    );
    assert_eq!(
        page.items[0].first_user_call_id.as_deref(),
        Some("call-s1-early")
    );
    assert!(page.items.iter().all(|i| i.session_id != "S3"));
    assert!(page.next_cursor.is_none(), "last page has no cursor");

    // Cursor paging one at a time must visit each session exactly once.
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let p = backend.query_sessions(&q(cursor, 1)).await.unwrap();
        if p.items.is_empty() {
            break;
        }
        seen.extend(p.items.iter().map(|i| i.session_id.clone()));
        match p.next_cursor {
            Some(c) => cursor = decode_session_cursor(&c),
            None => break,
        }
        assert!(seen.len() < 10, "cursor paging did not terminate");
    }
    assert_eq!(seen, vec!["S1".to_string(), "S2".to_string()]);

    // by-id returns the same lifetime aggregate.
    let detail = backend
        .query_session_by_id(&page.items[0].source_id, "S1")
        .await
        .unwrap()
        .expect("session exists");
    assert_eq!(detail.turn_count, 2);
    assert_eq!(detail.call_count, 4);
    assert_eq!(detail.total_input_tokens, 200);
    assert_eq!(detail.first_turn_at, sec(10) / 1000);
    assert_eq!(
        detail.first_user_input_preview.as_deref(),
        Some("opening S1")
    );

    assert!(backend
        .query_session_by_id("src-0", "no-such-session")
        .await
        .unwrap()
        .is_none());

    // The session's turns, newest first, paged by cursor.
    let mut turns = Vec::new();
    let mut cursor = None;
    loop {
        let p = backend
            .query_session_traces(&SessionTracesQuery {
                source_id: page.items[0].source_id.clone(),
                session_id: "S1".into(),
                cursor,
                page_size: 1,
            })
            .await
            .unwrap();
        if p.items.is_empty() {
            break;
        }
        turns.extend(p.items.iter().map(|t| t.turn_id.clone()));
        match p.next_cursor {
            Some(c) => cursor = decode_session_turns_cursor(&c),
            None => break,
        }
        assert!(turns.len() < 10, "turn paging did not terminate");
    }
    assert_eq!(turns, vec!["s1-late".to_string(), "s1-early".to_string()]);
}

#[tokio::test]
async fn session_filters_apply_before_aggregation() {
    let backend = require_backend!();
    let base = 1_785_644_000_000_000_i64;
    let mut a = fixtures::full_trace();
    a.turn_id = format!("ak-a-{}", uuid::Uuid::now_v7());
    a.session_id = "AK1".into();
    a.agent_kind = "claude-cli".into();
    a.start_time_us = base;
    a.end_time_us = base + 100_000;
    a.metadata = serde_json::json!({});
    let mut b = a.clone();
    b.turn_id = format!("ak-b-{}", uuid::Uuid::now_v7());
    b.session_id = "AK2".into();
    b.agent_kind = "codex-cli".into();
    backend.write_traces(vec![a, b]).await.unwrap();

    let q = |kinds: Vec<String>| SessionListQuery {
        time_range: TimeRange {
            start_us: base - 1_000_000,
            end_us: base + 60_000_000,
        },
        source_id: None,
        agent_kinds: kinds,
        cursor: None,
        page_size: 10,
    };

    eventually("agent-kind sessions", || async {
        let p = backend.query_sessions(&q(vec![])).await.unwrap();
        (p.items.len() == 2).then_some(p)
    })
    .await;

    let p = backend
        .query_sessions(&q(vec!["codex-cli".into()]))
        .await
        .unwrap();
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].session_id, "AK2");
    assert_eq!(p.items[0].agent_kind, "codex-cli");
}

// ---------------------------------------------------------------------------
// Phase 3: aggregates
// ---------------------------------------------------------------------------

/// Write metric rows on all four rollup tiers, the way the aggregator does.
/// The rollup rows carry the literal `'*'` sentinel, which is the value a
/// naive translation would read as a wildcard.
async fn seed_tiers(backend: &AglakeBackend, base_us: i64) {
    let mut rows = Vec::new();
    let mut mk = |wire: &str, model: &str, server: &str, calls: u64, ttft: f64| {
        let mut m = fixtures::sample_metric();
        m.timestamp_us = base_us;
        m.granularity = "10s";
        m.wire_api = wire.into();
        m.model = model.into();
        m.server_ip = server.into();
        m.call_count = calls;
        m.ttft_sum = ttft;
        m.ttft_count = calls;
        m.ttft_p95 = Some(ttft / calls as f64);
        m.error_count = 0;
        m.tool_surface = None;
        rows.push(m);
    };
    // Detail rows: 2 models on 1 server.
    mk("openai-chat", "gpt-4", "10.0.0.1", 10, 1000.0);
    mk("openai-chat", "gpt-4o", "10.0.0.1", 5, 250.0);
    // The three rollups the aggregator materializes over them.
    mk("openai-chat", "gpt-4", "*", 10, 1000.0);
    mk("openai-chat", "gpt-4o", "*", 5, 250.0);
    mk("*", "*", "10.0.0.1", 15, 1250.0);
    mk("*", "*", "*", 15, 1250.0);
    backend.write_metrics(rows).await.unwrap();
}

fn tr(base_us: i64) -> TimeRange {
    TimeRange {
        start_us: base_us - 60_000_000,
        end_us: base_us + 60_000_000,
    }
}

/// The headline metrics correctness property. Every filter shape must read
/// exactly one rollup tier: reading two would sum a rollup on top of the
/// detail rows it already contains and silently double every number.
#[tokio::test]
async fn metrics_read_one_tier_and_never_double_count() {
    let backend = require_backend!();
    let base = 1_785_650_000_000_000_i64;
    seed_tiers(&backend, base).await;

    let summary = |filter: DimensionFilter| MetricsSummaryQuery {
        time_range: tr(base),
        filter,
    };

    // No filter → the grand-total row alone. 15, not 15+15+15.
    let s = eventually("metrics", || async {
        let s = backend
            .query_metrics_summary(&summary(DimensionFilter::default()))
            .await
            .unwrap();
        (s.call_count > 0).then_some(s)
    })
    .await;
    assert_eq!(s.call_count, 15, "unfiltered total must read only (*,*,*)");
    assert_eq!(s.ttft_avg, Some(1250.0 / 15.0));

    // Server filter → the per-server rollup alone.
    let s = backend
        .query_metrics_summary(&summary(DimensionFilter {
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(s.call_count, 15, "server filter must read only (*,*,S)");

    // Model filter → that model's (W,M,*) row alone.
    let s = backend
        .query_metrics_summary(&summary(DimensionFilter {
            models: vec!["gpt-4".into()],
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(s.call_count, 10);

    // Both models → both (W,M,*) rows, summed.
    let s = backend
        .query_metrics_summary(&summary(DimensionFilter {
            models: vec!["gpt-4".into(), "gpt-4o".into()],
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(s.call_count, 15);

    // Model + server → the finest tier.
    let s = backend
        .query_metrics_summary(&summary(DimensionFilter {
            models: vec!["gpt-4".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(s.call_count, 10);
}

#[tokio::test]
async fn metrics_timeseries_computes_derived_fields() {
    let backend = require_backend!();
    let base = 1_785_651_000_000_000_i64;
    seed_tiers(&backend, base).await;

    let q = |fields: Vec<String>, group_by: Option<String>| MetricsTimeseriesQuery {
        time_range: tr(base),
        granularity: "10s".into(),
        filter: DimensionFilter::default(),
        fields,
        group_by,
    };

    let rows = eventually("timeseries", || async {
        let r = backend
            .query_metrics_timeseries(&q(vec!["call_count".into()], None))
            .await
            .unwrap();
        (!r.is_empty()).then_some(r)
    })
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values, vec![Some(15.0)]);
    // Timestamps come back in seconds, on the API's grid.
    assert_eq!(rows[0].timestamp, base / 1_000_000);
    assert!(rows[0].group.is_none());

    // Derived fields: an exact ratio and a count-weighted percentile, in the
    // order requested.
    let rows = backend
        .query_metrics_timeseries(&q(
            vec!["ttft_avg".into(), "call_count".into(), "ttft_p95".into()],
            None,
        ))
        .await
        .unwrap();
    let v = &rows[0].values;
    assert_eq!(v[0], Some(1250.0 / 15.0), "ttft_avg is sum/count");
    assert_eq!(v[1], Some(15.0));
    assert!(v[2].is_some(), "weighted percentile should resolve: {v:?}");

    // Grouping by model forces the detail tier even with no model filter.
    let rows = backend
        .query_metrics_timeseries(&q(vec!["call_count".into()], Some("model".into())))
        .await
        .unwrap();
    let mut got: Vec<(String, Option<f64>)> = rows
        .into_iter()
        .map(|r| (r.group.unwrap_or_default(), r.values[0]))
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("gpt-4".to_string(), Some(10.0)),
            ("gpt-4o".to_string(), Some(5.0))
        ],
        "grouping by model must not return the '*' rollup as a group"
    );

    // Unknown fields and group_by are refused, not silently dropped.
    assert!(backend
        .query_metrics_timeseries(&q(vec!["nope".into()], None))
        .await
        .is_err());
    assert!(backend
        .query_metrics_timeseries(&q(vec!["call_count".into()], Some("id".into())))
        .await
        .is_err());
}

#[tokio::test]
async fn metrics_models_and_finish_reasons_aggregate() {
    let backend = require_backend!();
    let base = 1_785_652_000_000_000_i64;
    seed_tiers(&backend, base).await;
    backend
        .write_finish_metrics(vec![
            LlmFinishMetric {
                timestamp_us: base,
                source_id: "src-0".into(),
                granularity: "10s".into(),
                wire_api: "*".into(),
                model: "*".into(),
                server_ip: "*".into(),
                finish_reason: "stop".into(),
                count: 12,
            },
            LlmFinishMetric {
                timestamp_us: base,
                source_id: "src-0".into(),
                granularity: "10s".into(),
                wire_api: "*".into(),
                model: "*".into(),
                server_ip: "*".into(),
                finish_reason: "length".into(),
                count: 3,
            },
        ])
        .await
        .unwrap();

    let models = eventually("models", || async {
        let m = backend
            .query_metrics_models(&MetricsModelsQuery {
                time_range: tr(base),
                filter: DimensionFilter::default(),
                sort_by: "call_count".into(),
                sort_order: "DESC".into(),
                limit: 10,
            })
            .await
            .unwrap();
        (m.len() == 2).then_some(m)
    })
    .await;
    // Sorted by call_count descending, reading the detail tier.
    assert_eq!(models[0].model, "gpt-4");
    assert_eq!(models[0].call_count, 10);
    assert_eq!(models[1].model, "gpt-4o");
    assert_eq!(models[1].call_count, 5);
    assert_eq!(models[0].ttft_avg, Some(100.0));

    let reasons = eventually("finish reasons", || async {
        let r = backend
            .query_finish_reasons(&FinishReasonsQuery {
                time_range: tr(base),
                granularity: "10s".into(),
                wire_apis: vec![],
                models: vec![],
                server_ips: vec![],
            })
            .await
            .unwrap();
        (r.len() == 2).then_some(r)
    })
    .await;
    let mut by_reason: Vec<(String, u64)> = reasons
        .into_iter()
        .map(|s| (s.finish_reason, s.points.iter().map(|(_, c)| c).sum()))
        .collect();
    by_reason.sort();
    assert_eq!(
        by_reason,
        vec![("length".to_string(), 3u64), ("stop".to_string(), 12)]
    );
}

#[tokio::test]
async fn services_aggregate_endpoints_and_build_a_graph() {
    let backend = require_backend!();
    let base = 1_785_653_000_000_000_i64;
    let mut calls = Vec::new();
    for i in 0..6 {
        let mut c = fixtures::full_call();
        c.id = format!("svc-{i}-{}", uuid::Uuid::now_v7());
        c.request_time = base + i * 1_000_000;
        c.model = if i % 2 == 0 { "gpt-4" } else { "gpt-4o" }.into();
        c.status_code = Some(if i == 5 { 500 } else { 200 });
        c.server_ip = "10.0.0.1".parse().unwrap();
        c.server_port = 8000;
        c.client_ip = "203.0.113.9".parse().unwrap();
        calls.push(c);
    }
    backend.write_spans(calls.clone()).await.unwrap();

    let rows = eventually("services", || async {
        let r = backend
            .query_services(&ServicesQuery {
                time_range: tr(base),
                sort_by: "call_count".into(),
                sort_order: "DESC".into(),
                limit: 50,
            })
            .await
            .unwrap();
        (!r.is_empty()).then_some(r)
    })
    .await;
    let ep = &rows[0];
    assert_eq!(ep.server_ip, "10.0.0.1");
    assert_eq!(ep.server_port, 8000);
    assert_eq!(ep.call_count, 6);
    assert_eq!(
        ep.error_count, 1,
        "the 5xx call must be counted as an error"
    );
    assert_eq!(ep.stream_count, 6);
    let mut models = ep.models.clone();
    models.sort();
    assert_eq!(models, vec!["gpt-4".to_string(), "gpt-4o".to_string()]);
    assert!(ep.ttft_avg_ms.is_some());
    assert!(ep.ttft_p95_ms.is_some(), "perc95 must resolve");
    assert_eq!(ep.first_seen_ms, base / 1000);
    assert_eq!(ep.last_seen_ms, (base + 5_000_000) / 1000);
    // `Server: uvicorn` is lifted out of the headers at write time.
    assert_eq!(ep.server_header.as_deref(), Some("uvicorn"));

    // A trace over those calls gives the graph an entry edge.
    let mut t = fixtures::full_trace();
    t.turn_id = format!("svc-turn-{}", uuid::Uuid::now_v7());
    t.start_time_us = base;
    t.end_time_us = base + 6_000_000;
    t.span_ids = vec![calls[0].id.clone()];
    t.metadata = serde_json::json!({});
    backend.write_traces(vec![t]).await.unwrap();

    let topo = eventually("topology", || async {
        let g = backend
            .query_services_topology(&ServicesTopologyQuery {
                time_range: tr(base),
            })
            .await
            .unwrap();
        (!g.edges.is_empty()).then_some(g)
    })
    .await;
    // The client IP hosts no known service, so it is an external client.
    assert_eq!(topo.edges.len(), 1);
    assert_eq!(topo.edges[0].kind, "client");
    assert_eq!(topo.edges[0].to_ip, "10.0.0.1");
    assert_eq!(topo.edges[0].to_port, 8000);

    // Two nodes: the endpoint, and the synthetic super-node the client edge
    // originates from. An earlier version of this test asserted one node and
    // so locked in a graph whose edge pointed at a node that was never sent —
    // it took a cross-backend replay to notice.
    let endpoint = topo
        .nodes
        .iter()
        .find(|n| n.server_ip == "10.0.0.1")
        .expect("the endpoint node");
    assert_eq!(endpoint.call_count, 6);
    let clients = topo
        .nodes
        .iter()
        .find(|n| n.server_ip == "__clients__")
        .expect("the synthetic __clients__ node");
    assert_eq!(clients.server_port, 0);
    assert_eq!(clients.app.as_deref(), Some("clients"));
    assert_eq!(
        clients.call_count, topo.edges[0].turn_count,
        "the super-node's total is the sum of the client edges"
    );
    assert_eq!(topo.nodes.len(), 2);

    assert!(backend
        .query_services(&ServicesQuery {
            time_range: tr(base),
            sort_by: "bogus".into(),
            sort_order: "DESC".into(),
            limit: 10,
        })
        .await
        .is_err());
}

#[tokio::test]
async fn distincts_and_agent_rollups() {
    let backend = require_backend!();
    let base = 1_785_654_000_000_000_i64;

    let mut calls = Vec::new();
    for (i, (wire, model, reason)) in [
        ("openai-chat", "gpt-4", Some("stop")),
        ("anthropic", "claude-sonnet", Some("end_turn")),
        // Still in flight: no finish reason, so not a filter option.
        ("openai-chat", "gpt-4", None),
    ]
    .into_iter()
    .enumerate()
    {
        let mut c = fixtures::full_call();
        c.id = format!("dst-{i}-{}", uuid::Uuid::now_v7());
        c.request_time = base + i as i64 * 1_000_000;
        c.wire_api = wire;
        c.model = model.into();
        c.finish_reason = reason.map(String::from);
        calls.push(c);
    }
    backend.write_spans(calls).await.unwrap();

    let models = eventually("distinct models", || async {
        let m = backend.query_distinct_models().await.unwrap();
        (m.len() == 2).then_some(m)
    })
    .await;
    assert_eq!(
        models,
        vec!["claude-sonnet".to_string(), "gpt-4".to_string()]
    );

    let wires = backend.query_distinct_wire_apis().await.unwrap();
    assert_eq!(
        wires,
        vec!["anthropic".to_string(), "openai-chat".to_string()]
    );
    assert_eq!(
        backend.query_distinct_server_ips().await.unwrap(),
        vec!["10.0.0.1".to_string()]
    );

    let reasons = backend.query_distinct_finish_reasons().await.unwrap();
    let mut pairs: Vec<(String, String)> = reasons
        .into_iter()
        .map(|r| (r.wire_api, r.finish_reason))
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("anthropic".to_string(), "end_turn".to_string()),
            ("openai-chat".to_string(), "stop".to_string())
        ],
        "the in-flight call must not contribute an empty finish reason"
    );

    // Agent rollups, over traces.
    let mut traces = Vec::new();
    for (i, kind) in ["claude-cli", "claude-cli", "codex-cli"]
        .into_iter()
        .enumerate()
    {
        let mut t = fixtures::full_trace();
        t.turn_id = format!("agt-{i}-{}", uuid::Uuid::now_v7());
        t.agent_kind = kind.into();
        t.start_time_us = base + i as i64 * 1_000_000;
        t.end_time_us = t.start_time_us + 1_000_000;
        t.duration_ms = 1000;
        t.total_input_tokens = 100;
        t.metadata = serde_json::json!({});
        traces.push(t);
    }
    backend.write_traces(traces).await.unwrap();

    let summary = eventually("agent summary", || async {
        let s = backend
            .query_agent_summary(&AgentSummaryQuery {
                time_range: tr(base),
            })
            .await
            .unwrap();
        (s.len() == 2).then_some(s)
    })
    .await;
    assert_eq!(summary[0].agent_kind, "claude-cli");
    assert_eq!(summary[0].turn_count, 2);
    assert_eq!(summary[0].total_input_tokens, 200);
    assert_eq!(summary[0].avg_duration_ms, Some(1000.0));
    assert_eq!(summary[1].agent_kind, "codex-cli");
    assert_eq!(summary[1].turn_count, 1);

    let kinds = backend
        .query_distinct_agent_kinds(&DistinctAgentKindsQuery {
            time_range: tr(base),
            filter: DimensionFilter::default(),
            include_proxy_hops: false,
        })
        .await
        .unwrap();
    assert_eq!(
        kinds,
        vec!["claude-cli".to_string(), "codex-cli".to_string()]
    );

    // The summary's contract comes from the SQL backends: `last_seen_ms` is
    // the latest turn *start*, and rows come back busiest-first.
    //
    // claude-cli's two turns start at `base` and `base + 1s` and each run for
    // a second, so max(start) and max(end) are a full second apart — which is
    // what makes this able to tell them apart at all.
    assert_eq!(
        summary[0].agent_kind, "claude-cli",
        "busiest kind first: {summary:?}"
    );
    assert_eq!(
        summary[0].last_seen_ms,
        (base + 1_000_000) / 1000,
        "last_seen_ms is max(start); max(end) would be {}",
        (base + 2_000_000) / 1000
    );
    assert!(
        summary[0].turn_count >= summary[1].turn_count,
        "summary must be ordered by turn_count DESC: {summary:?}"
    );

    let activity = backend
        .query_agent_activity(&AgentActivityQuery {
            time_range: tr(base),
            bucket_seconds: Some(3600),
        })
        .await
        .unwrap();
    assert!(!activity.is_empty(), "bin + stats must produce buckets");
    let total: u64 = activity.iter().map(|p| p.turn_count).sum();
    assert_eq!(total, 3);
    assert!(activity.iter().all(|p| p.timestamp_ms > 0));

    // No hint: the bucket has to be derived the same way the SQL backends
    // derive it, or the same request lands on a different time grid depending
    // on which backend answered. `tr()` spans 120 s, so the target rounds up
    // to the 60 s floor.
    let auto = backend
        .query_agent_activity(&AgentActivityQuery {
            time_range: tr(base),
            bucket_seconds: None,
        })
        .await
        .unwrap();
    let total: u64 = auto.iter().map(|p| p.turn_count).sum();
    assert_eq!(total, 3, "the default bucket must not drop turns");
    assert!(
        auto.iter().all(|p| p.timestamp_ms % 60_000 == 0),
        "a 120 s window must bucket at 60 s, not the old hardcoded hour: {auto:?}"
    );
}

// ---------------------------------------------------------------------------
// Phase 4: retention, acks, metrics dedup
// ---------------------------------------------------------------------------

/// Retention has to reach aglake as a real per-index TTL, or the whole design
/// is a no-op that logs cheerfully. This asserts it round-trips: push the
/// policy, then read the value back out of aglake's own index catalogue.
///
/// Skips — rather than fails — when the admin API is unreachable, since that
/// depends on the aglaked under test (its version, and whether it has a user
/// catalog this run has credentials for), not on this code.
#[tokio::test]
async fn retention_reaches_aglake_as_a_per_index_ttl() {
    let backend = require_backend!();
    if backend.management.list_indexes().await.is_err() {
        eprintln!("skip: aglaked admin API unreachable (needs the 0.3-or-later admin face, and admin credentials when it has users)");
        return;
    }

    let base = 1_760_000_000_000_000i64;
    let mut call = fixtures::full_call();
    call.id = uuid::Uuid::now_v7().to_string();
    call.request_time = base;
    backend.write_spans(vec![call]).await.unwrap();
    backend
        .write_metrics(vec![fixtures::sample_metric()])
        .await
        .unwrap();

    // The index has to exist before it can be configured, and it exists only
    // once a write has landed in it.
    eventually("indexes to appear in the catalogue", || async {
        let names: Vec<String> = backend
            .management
            .list_indexes()
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        (names.contains(&backend.ix.spans) && names.contains(&backend.ix.bodies)).then_some(())
    })
    .await;

    let now = std::time::SystemTime::now();
    let policy = h_storage::retention::RetentionPolicy {
        spans_before: Some(now - Duration::from_secs(7 * 86_400)),
        traces_before: Some(now - Duration::from_secs(30 * 86_400)),
        http_exchanges_before: None,
        metrics_before: vec![("10s".into(), now - Duration::from_secs(86_400))],
    };
    let report = backend.apply_retention(policy).await.unwrap();
    assert_eq!(
        report.total(),
        0,
        "aglake declares TTLs rather than deleting rows, so it must not \
         invent a deletion count"
    );

    let by_name: std::collections::HashMap<String, Option<i64>> = backend
        .management
        .list_indexes()
        .await
        .unwrap()
        .into_iter()
        .map(|i| (i.name, i.frozen_after_secs))
        .collect();

    assert_eq!(by_name.get(&backend.ix.spans), Some(&Some(7 * 86_400)));
    // Bodies inherit their parent's schedule when not configured separately.
    assert_eq!(by_name.get(&backend.ix.bodies), Some(&Some(7 * 86_400)));
    let m10s = backend.ix.metrics_for("10s").unwrap().to_string();
    assert_eq!(by_name.get(&m10s), Some(&Some(86_400)));

    // No cutoff must mean "keep", not "some default appeared".
    assert!(
        !by_name.contains_key(&backend.ix.http) || by_name[&backend.ix.http].is_none(),
        "http_exchanges had no cutoff and must not have been given one"
    );

    // Re-applying an unchanged policy is a normal occurrence — the retention
    // loop runs on a timer — and must stay correct rather than drift.
    let policy = h_storage::retention::RetentionPolicy {
        spans_before: Some(now - Duration::from_secs(7 * 86_400)),
        ..Default::default()
    };
    backend.apply_retention(policy).await.unwrap();
    let again = backend.management.list_indexes().await.unwrap();
    let spans = again.iter().find(|i| i.name == backend.ix.spans).unwrap();
    assert_eq!(spans.frozen_after_secs, Some(7 * 86_400));
}

/// `manage_retention = false` hands retention to whoever runs aglaked. It has
/// to mean *nothing is sent*, not "sent, but quietly".
#[tokio::test]
async fn manage_retention_off_sends_nothing() {
    let Ok(url) = std::env::var("AGLAKE_TEST_URL") else {
        eprintln!("skip: AGLAKE_TEST_URL unset");
        return;
    };
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let cfg = AglakeConfig {
        url,
        hec_token: std::env::var("AGLAKE_TEST_TOKEN").unwrap_or_default(),
        index_prefix: format!("itnoret{}", &nonce[..12]),
        manage_retention: false,
        ..test_credentials()
    };
    let backend = AglakeBackend::new(&cfg).unwrap();
    backend.init().await.unwrap();
    if backend.management.list_indexes().await.is_err() {
        eprintln!("skip: aglaked admin API unreachable");
        return;
    }

    let mut call = fixtures::full_call();
    call.id = uuid::Uuid::now_v7().to_string();
    backend.write_spans(vec![call]).await.unwrap();
    eventually("spans index to appear", || async {
        backend
            .management
            .list_indexes()
            .await
            .unwrap()
            .iter()
            .any(|i| i.name == backend.ix.spans)
            .then_some(())
    })
    .await;

    // Compare against a baseline rather than against `None`: the catalogue
    // reports the *effective* TTL, so a aglaked started with a server-wide
    // --retention-days shows a value here that nobody pushed. "Sends nothing"
    // is therefore "nothing changed", not "nothing is set".
    let before = backend
        .management
        .list_indexes()
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.name == backend.ix.spans)
        .unwrap()
        .frozen_after_secs;

    let now = std::time::SystemTime::now();
    backend
        .apply_retention(h_storage::retention::RetentionPolicy {
            spans_before: Some(now - Duration::from_secs(7 * 86_400)),
            ..Default::default()
        })
        .await
        .unwrap();

    let after = backend
        .management
        .list_indexes()
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.name == backend.ix.spans)
        .unwrap()
        .frozen_after_secs;
    assert_eq!(
        after, before,
        "retention was pushed despite manage_retention = false"
    );
    assert_ne!(
        after,
        Some(7 * 86_400),
        "the policy's TTL reached aglake despite manage_retention = false"
    );
}

/// Acks are on by default, so every write in this suite already exercises the
/// channel header. What this adds is the part the other tests cannot show:
/// that the header does not disturb ingest, and that the ack endpoint answers
/// the question the retry path asks it.
#[tokio::test]
async fn writes_carry_an_ack_channel_and_the_answer_is_usable() {
    let backend = require_backend!();
    let base = 1_760_000_100_000_000i64;
    let mut call = fixtures::full_call();
    call.id = uuid::Uuid::now_v7().to_string();
    call.request_time = base;
    backend.write_spans(vec![call.clone()]).await.unwrap();

    let got = eventually("span written with an ack channel", || async {
        backend.query_span_by_id(&call.id).await.unwrap()
    })
    .await;
    assert_eq!(got.id, call.id);

    // An unused channel must answer "not committed" — that is the negative
    // half of the signal, and without it a resend would be suppressed for a
    // batch that never landed.
    let unused = uuid::Uuid::now_v7().to_string();
    assert_eq!(
        backend.hec.ack_committed(&unused).await,
        Some(false),
        "a channel that never carried a request must not report a commit"
    );
}

/// The read-side half of the `row_id` scheme. Writing the same metric batch
/// twice is exactly what an at-least-once resend does; with dedup on, the sums
/// must not double.
#[tokio::test]
async fn metrics_dedup_collapses_a_duplicated_write() {
    let Ok(url) = std::env::var("AGLAKE_TEST_URL") else {
        eprintln!("skip: AGLAKE_TEST_URL unset");
        return;
    };
    let token = std::env::var("AGLAKE_TEST_TOKEN").unwrap_or_default();
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let prefix = format!("itdedup{}", &nonce[..12]);

    let plain = AglakeBackend::new(&AglakeConfig {
        url: url.clone(),
        hec_token: token.clone(),
        index_prefix: prefix.clone(),
        ..test_credentials()
    })
    .unwrap();
    plain.init().await.unwrap();

    // The summary query reads the grand-total rollup tier, so the row has to
    // be one — a detail row would simply not be selected, and the test would
    // fail waiting for data that was never in scope.
    let mut m = fixtures::sample_metric();
    m.wire_api = "*".into();
    m.model = "*".into();
    m.server_ip = "*".into();
    m.tool_surface = None;
    let base = m.timestamp_us;
    plain.write_metrics(vec![m.clone()]).await.unwrap();

    let range = TimeRange {
        start_us: base - 60_000_000,
        end_us: base + 60_000_000,
    };

    async fn summary_of(b: &AglakeBackend, range: &TimeRange) -> MetricsSummaryRow {
        b.query_metrics_summary(&MetricsSummaryQuery {
            time_range: range.clone(),
            filter: DimensionFilter::default(),
        })
        .await
        .unwrap()
    }

    let once = eventually("first metric row", || async {
        let s = summary_of(&plain, &range).await;
        (s.call_count > 0).then_some(s)
    })
    .await;

    // Now duplicate it the way a resend would: the same event bytes, including
    // the same `row_id`, posted a second time.
    let dup = crate::rows::metric_event(&m, "dup-row-id-fixed".into());
    let ix = plain.ix.metrics_for(m.granularity).unwrap().to_string();
    for _ in 0..2 {
        let ev = crate::rows::Envelope::new(
            m.timestamp_us,
            &m.source_id,
            crate::rows::ST_METRIC,
            &ix,
            &dup,
        )
        .encode()
        .unwrap();
        plain.hec.send(vec![ev]).await.unwrap();
    }

    let doubled = eventually("the duplicated rows", || async {
        let s = summary_of(&plain, &range).await;
        (s.call_count > once.call_count).then_some(s)
    })
    .await;
    assert_eq!(
        doubled.call_count,
        once.call_count * 3,
        "precondition: without dedup, duplicates really do inflate the sum"
    );

    let deduped = AglakeBackend::new(&AglakeConfig {
        url,
        hec_token: token,
        index_prefix: prefix,
        metrics_dedup: true,
        ..test_credentials()
    })
    .unwrap();
    let s = summary_of(&deduped, &range).await;
    assert_eq!(
        s.call_count,
        once.call_count * 2,
        "dedup must collapse the two identical row_ids to one, leaving the \
         original row and one copy of the duplicate"
    );
}

// ---------------------------------------------------------------------------
// Fault injection: aglaked goes away mid-write
// ---------------------------------------------------------------------------
//
// The DuckDB backend has a `--features fault-injection` suite that kills the
// database underneath a write and asserts the write either commits or returns
// `Err` — never silently vanishes. Nothing there transfers to an HTTP client,
// so this is its counterpart: it starts a private aglaked, kills it by its exact
// pid mid-stream, and checks what Heron does about it.
//
// Opt-in through `AGLAKE_AGLAKED_BIN`, because unlike the rest of the suite it
// spawns and kills processes. It no longer needs `--splunk-web-dir` to reach
// the management API: the native admin face is always mounted, which is the
// point of the 0.3 migration.

/// Flags that switch off one dedicated receiver.
///
/// aglake 1.5 starts all of these **by default**, each bound to a fixed
/// `0.0.0.0` port (OTLP 4318/4317, syslog 514, S2S 9997, ES-compat 9200). A
/// test-owned daemon wants none of them: they collide with anything else on the
/// host — another aglaked, someone's real Elasticsearch on 9200 — and syslog's
/// 514 is privileged, which this process is not. Left at their defaults, the
/// daemon exits before it ever listens and the test reports a timeout that says
/// nothing about why.
///
/// None of them exist on 0.3, where `--listen` is the only thing bound, and an
/// unrecognized flag is a hard parse error. So each is passed only when the
/// binary's own `--help` lists it — the suite runs against whichever aglaked
/// the operator points `AGLAKE_AGLAKED_BIN` at.
///
/// HEC is deliberately not in this list. `--hec-http-enabled false` switches
/// off the HEC *input*, including the alias on `--listen` that every write in
/// this suite goes through; its dedicated listener gets moved instead.
const RECEIVERS_TO_DISABLE: &[&str] = &[
    "--otlp-http-enabled",
    "--otlp-grpc-enabled",
    "--syslog-udp-enabled",
    "--syslog-tcp-enabled",
    "--s2s-tcp-enabled",
    "--es-http-enabled",
];

/// Which receiver flags this binary understands, read out of its `--help`.
///
/// The match has to be exact, not a substring: `--hec-http` is a prefix of
/// `--hec-http-enabled`, so a plain `contains` would report the former as
/// supported on a build that has only the latter, and passing `--hec-http
/// <addr>` there is a hard parse error. clap prints one flag per line followed
/// by a space (before its value placeholder) or a newline, so requiring that
/// separator distinguishes the two.
fn supported_flags(bin: &str) -> std::collections::HashSet<&'static str> {
    let help = std::process::Command::new(bin)
        .arg("--help")
        .output()
        .map(|o| {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            text
        })
        .unwrap_or_default();
    flags_in_help(&help)
}

/// The parsing half of [`supported_flags`], split out so the prefix rule can be
/// tested without a binary to run.
fn flags_in_help(help: &str) -> std::collections::HashSet<&'static str> {
    RECEIVERS_TO_DISABLE
        .iter()
        .copied()
        .chain(["--hec-http"])
        .filter(|flag| {
            help.lines().any(|line| {
                line.split_whitespace()
                    .any(|word| word.trim_end_matches(',') == *flag)
            })
        })
        .collect()
}

/// A flag must not be reported as supported just because a longer flag starts
/// with its name — passing `--hec-http <addr>` to a build that has only
/// `--hec-http-enabled` is a parse error, and the daemon never starts.
#[test]
fn flag_detection_does_not_confuse_a_prefix_for_the_flag() {
    // `--hec-http` is a prefix of `--hec-http-enabled`, and only the former is
    // ever passed a value. A build carrying just the longer flag must not be
    // credited with the shorter one.
    let only_enabled = "      --hec-http-enabled <HEC_HTTP_ENABLED>\n          Start it\n";
    assert!(
        !flags_in_help(only_enabled).contains("--hec-http"),
        "--hec-http-enabled must not imply --hec-http"
    );

    // Both present, as on a current build.
    let both = "      --hec-http <HEC_HTTP>\n      --hec-http-enabled <HEC_HTTP_ENABLED>\n";
    assert!(flags_in_help(both).contains("--hec-http"));

    // A receiver switch is detected on its own line, value placeholder and all.
    let receiver = "      --es-http-enabled <ES_HTTP_ENABLED>\n          [default: true]\n";
    assert!(flags_in_help(receiver).contains("--es-http-enabled"));

    // A build predating all of them: nothing is passed, which is what keeps
    // this working against a release from before the flags existed.
    assert!(flags_in_help("      --listen <LISTEN>\n").is_empty());
}

/// A aglaked this test owns, on its own ports and data directory.
///
/// Every write in this suite goes to `/services/collector/*` on the
/// **application** port, which the daemon serves as an alias of the dedicated
/// HEC input ("for clients that were pointed at the application port first",
/// per its own `--hec-http` help text). That alias is load-bearing: if it ever
/// stops answering, writes do not degrade, they 404.
///
/// Both ports are passed in rather than derived here, because they must not
/// collide with the *other* self-spawning test's: each caller owns a disjoint
/// 10k band (see `PORT_BANDS`). Deriving the HEC port as `port + 1` was wrong
/// for exactly that reason — the top of one band is the bottom of the next.
struct OwnedAglaked {
    bin: String,
    dir: std::path::PathBuf,
    /// The application port: API, search, and the HEC alias the suite writes
    /// through.
    port: u16,
    /// The dedicated HEC listener. Never contacted by these tests; it exists
    /// only because the daemon insists on binding one, and 8088 is not ours to
    /// take.
    hec_port: u16,
    child: Option<std::process::Child>,
}

/// Disjoint port bands, one pair per self-spawning test.
///
/// Four non-overlapping 10k ranges, so two concurrent `cargo test` processes —
/// or one running both spawn-tests in parallel — cannot have one daemon's HEC
/// listener land on another daemon's application port. That failure mode is
/// invisible: the loser reports only "never became ready".
mod port_bands {
    /// `(application, dedicated HEC)` base for the fault-injection test.
    pub(super) const FAULT: (u16, u16) = (20_000, 30_000);
    /// Same, for the props test.
    pub(super) const PROPS: (u16, u16) = (40_000, 50_000);
    /// Width of each band. Keeps every derived port inside its own range.
    pub(super) const WIDTH: u16 = 10_000;

    /// Derive a port pair from a nonce, inside the given bases.
    pub(super) fn pick(bases: (u16, u16), nonce: &str) -> (u16, u16) {
        let offset = u16::from_str_radix(&nonce[..4], 16).unwrap_or(0) % WIDTH;
        (bases.0 + offset, bases.1 + offset)
    }
}

impl OwnedAglaked {
    fn spawn(&mut self) {
        let supported = supported_flags(&self.bin);
        let mut cmd = std::process::Command::new(&self.bin);
        cmd.arg("--data-dir")
            .arg(&self.dir)
            .arg("--listen")
            .arg(format!("127.0.0.1:{}", self.port))
            .arg("--hec-token")
            .arg("heron-fault")
            .arg("--no-self-trace")
            .arg("--max-hot-raw-mib")
            .arg("2048")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // A build new enough to have `--hec-http` is new enough to have all of
        // these. If one is missing, the daemon has renamed a flag rather than
        // dropped the feature — in which case the receiver stays at its default,
        // fails to bind a port someone else owns, and the test reports only
        // "never became ready". Say so instead.
        if supported.contains("--hec-http") {
            let missing: Vec<&str> = RECEIVERS_TO_DISABLE
                .iter()
                .copied()
                .filter(|flag| !supported.contains(flag))
                .collect();
            assert!(
                missing.is_empty(),
                "aglaked has --hec-http but not {missing:?}. Either the flag was \
                 renamed (in which case this list needs the new spelling, or the \
                 receiver stays on and the daemon cannot bind its port) or the \
                 receiver was removed entirely (in which case drop it from the \
                 list). Both need a human; degrading quietly would put back the \
                 opaque 'never became ready' this check exists to prevent."
            );
        }
        for flag in RECEIVERS_TO_DISABLE {
            if supported.contains(flag) {
                cmd.arg(flag).arg("false");
            }
        }
        if supported.contains("--hec-http") {
            // Keep the input, move the listener: on loopback, and off the
            // conventional 8088 so two test daemons — or a test daemon and
            // whatever the developer is already running — do not fight for it.
            cmd.arg("--hec-http")
                .arg(format!("127.0.0.1:{}", self.hec_port));
        }
        self.child = Some(cmd.spawn().expect("spawn aglaked"));
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    async fn wait_ready(&self) {
        let probe = format!("{}/api/v1/indexes", self.url());
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if reqwest_get_ok(&probe).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("aglaked never became ready on port {}", self.port);
    }

    async fn wait_down(&self) {
        let probe = format!("{}/api/v1/indexes", self.url());
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if !reqwest_get_ok(&probe).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("aglaked on port {} refused to go down", self.port);
    }

    /// SIGKILL, by this child's exact pid and nothing else. Abrupt on purpose:
    /// a clean shutdown would flush, and the point is to test what survives
    /// when nothing gets to flush.
    async fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.wait_down().await;
    }
}

impl Drop for OwnedAglaked {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn reqwest_get_ok(url: &str) -> bool {
    matches!(reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(1))
        .send()
        .await, Ok(r) if r.status().is_success())
}

fn span_batch(base_us: i64, tag: &str, n: usize) -> Vec<h_llm::model::LlmCall> {
    (0..n)
        .map(|i| {
            let mut c = fixtures::full_call();
            c.id = format!("{tag}-{i}-{}", uuid::Uuid::now_v7());
            c.request_time = base_us + i as i64 * 1_000;
            c
        })
        .collect()
}

/// Killing aglaked mid-stream must produce an error, not a panic and not a
/// silent drop — and writes must resume once it is back, with everything that
/// was acknowledged before the kill still there.
#[tokio::test]
async fn a_write_against_a_dead_aglaked_errors_and_recovers() {
    let Ok(bin) = std::env::var("AGLAKE_AGLAKED_BIN") else {
        eprintln!("skip: AGLAKE_AGLAKED_BIN unset");
        return;
    };
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let mut sg = OwnedAglaked {
        bin,
        dir: std::env::temp_dir().join(format!("aglake-fault-{}", &nonce[..12])),
        // Ports nobody else in this suite uses, derived from the nonce so two
        // concurrent runs do not collide either.
        port: port_bands::pick(port_bands::FAULT, &nonce).0,
        hec_port: port_bands::pick(port_bands::FAULT, &nonce).1,
        child: None,
    };
    std::fs::create_dir_all(&sg.dir).unwrap();
    sg.spawn();
    sg.wait_ready().await;

    let cfg = AglakeConfig {
        url: sg.url(),
        hec_token: "heron-fault".into(),
        index_prefix: format!("flt{}", &nonce[..12]),
        // Fail fast: the point is to observe the error, not to sit through a
        // full retry schedule.
        write_retries: 1,
        retry_backoff_ms: 50,
        request_timeout_secs: 5,
        search_timeout_secs: 10,
        ..Default::default()
    };
    let backend = AglakeBackend::new(&cfg).unwrap();
    backend.init().await.unwrap();

    let base = 1_770_000_000_000_000i64;
    let before = span_batch(base, "before", 20);
    let before_ids: Vec<String> = before.iter().map(|c| c.id.clone()).collect();
    backend.write_spans(before).await.expect("healthy write");

    eventually("the pre-kill batch", || async {
        (backend
            .query_span_by_id(before_ids.last().unwrap())
            .await
            .unwrap()
            .is_some())
        .then_some(())
    })
    .await;

    sg.kill().await;

    // The contract WriteBuffer depends on: a flush that did not land returns
    // `Err`. Returning `Ok` here would make the buffer discard the batch as
    // committed, which is exactly the silent-loss failure this guards.
    let err = backend
        .write_spans(span_batch(base + 1_000_000, "during", 5))
        .await
        .expect_err("a write to a dead server must not report success");
    let msg = err.to_string();
    assert!(
        msg.contains("aglake"),
        "the error must name the backend that failed: {msg}"
    );

    // Reads fail loudly too, rather than answering "no results".
    assert!(
        backend.query_span_by_id(&before_ids[0]).await.is_err(),
        "a read against a dead server must error, not report an empty result \
         — an empty result is indistinguishable from real absence"
    );

    // Retention degrades to a no-op rather than propagating an error and
    // taking down the shared retention loop with it.
    let now = std::time::SystemTime::now();
    backend
        .apply_retention(h_storage::retention::RetentionPolicy {
            spans_before: Some(now - Duration::from_secs(7 * 86_400)),
            ..Default::default()
        })
        .await
        .expect("retention must degrade, not fail the sweep");

    sg.spawn();
    sg.wait_ready().await;

    let after = span_batch(base + 2_000_000, "after", 20);
    let after_ids: Vec<String> = after.iter().map(|c| c.id.clone()).collect();
    backend
        .write_spans(after)
        .await
        .expect("write after recovery");

    // Everything acknowledged before the SIGKILL is still on disk: HEC returns
    // 200 only after the WAL fsync, so an abrupt kill cannot take it back.
    let backend = &backend;
    for id in before_ids.iter().chain(after_ids.iter()) {
        eventually("a span to survive the restart", || async move {
            backend.query_span_by_id(id).await.unwrap().map(|_| ())
        })
        .await;
    }
}

/// Stored bodies must still be readable when aglaked has a props.toml.
///
/// Every other test in this file runs against a aglaked with **no props.toml**,
/// which is not a configuration anyone deploys — `docs/design/10-aglake.md` and
/// `heron aglake-props` both tell the operator to install one. That gap hid a
/// total failure of the body read path: the body indexes are written as
/// pre-serialized JSON strings with `auto_json = false`, so they have no
/// extracted fields, and the lookups asked for `span_id="…"`. Without props
/// that still matched (aglake fell back to matching the raw event); with props
/// loaded it matched nothing, so every Request/Response body in the console was
/// empty — and slow, because a miss sends `fetch_raw_by_id` into its unbounded
/// retry across the whole retention window.
///
/// So this test owns its aglaked specifically to write props.toml into the data
/// directory before starting it. Gated on `AGLAKE_AGLAKED_BIN` like the fault
/// injection above, for the same reason: it spawns a process.
#[tokio::test]
async fn bodies_are_readable_when_aglaked_has_props() {
    let Ok(bin) = std::env::var("AGLAKE_AGLAKED_BIN") else {
        eprintln!("skip: AGLAKE_AGLAKED_BIN unset");
        return;
    };
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let mut sg = OwnedAglaked {
        bin,
        dir: std::env::temp_dir().join(format!("aglake-props-{}", &nonce[..12])),
        port: port_bands::pick(port_bands::PROPS, &nonce).0,
        hec_port: port_bands::pick(port_bands::PROPS, &nonce).1,
        child: None,
    };
    std::fs::create_dir_all(&sg.dir).unwrap();
    // The whole point of the test: the daemon starts holding the extraction
    // config Heron itself tells operators to install.
    std::fs::write(sg.dir.join("props.toml"), crate::props::render()).unwrap();
    sg.spawn();
    sg.wait_ready().await;

    let cfg = AglakeConfig {
        url: sg.url(),
        hec_token: "heron-fault".into(),
        index_prefix: format!("prp{}", &nonce[..12]),
        request_timeout_secs: 5,
        search_timeout_secs: 30,
        ..Default::default()
    };
    let backend = AglakeBackend::new(&cfg).unwrap();
    backend.init().await.unwrap();

    let base = 1_770_000_000_000_000i64;

    let mut call = fixtures::full_call();
    call.id = uuid::Uuid::now_v7().to_string();
    call.request_time = base;
    let call_id = call.id.clone();
    let want_request = call.request_body.clone();
    let want_response = call.response_body.clone();
    assert!(
        want_request.is_some() && want_response.is_some(),
        "the fixture must carry bodies or this test proves nothing"
    );
    backend.write_spans(vec![call]).await.unwrap();

    let exchange_id = uuid::Uuid::now_v7().to_string();
    backend
        .write_exchanges(vec![sample_exchange(&exchange_id, base)])
        .await
        .unwrap();

    let backend = &backend;
    let detail = eventually("the span with its body", || async {
        backend.query_span_by_id(&call_id).await.unwrap()
    })
    .await;
    assert_eq!(
        detail.request_body, want_request,
        "the span's request body came back empty — the body index lookup is not matching"
    );
    assert_eq!(detail.response_body, want_response);

    let exchange = eventually("the exchange with its body", || async {
        backend
            .query_http_exchange_by_id(&exchange_id)
            .await
            .unwrap()
    })
    .await;
    assert_eq!(
        exchange.request_body.as_deref(),
        Some(r#"{"model":"gpt-4"}"#),
        "the exchange's request body came back empty"
    );
    assert_eq!(exchange.response_body.as_deref(), Some(r#"{"choices":[]}"#));

    // A trace whose spans are spread over more than one lookup window must
    // come back with ALL of its bodies. This is the failure mode a windowed
    // multi-id lookup has and a single-id one does not: the ids inside the
    // window hit, the query returns non-empty, and the ids outside it are
    // dropped — so the answer looks complete and is not. Observed as 90 of 184
    // bodies on a live trace the moment the window was narrowed.
    let spread: Vec<h_llm::model::LlmCall> = (0..6)
        .map(|i| {
            let mut c = fixtures::full_call();
            c.id = uuid::Uuid::now_v7().to_string();
            // 10 minutes apart: wider than the narrow window, so a lookup that
            // stops at the first window that returns anything will miss most.
            c.request_time = base + i * 600 * 1_000_000;
            c
        })
        .collect();
    let spread_ids: Vec<String> = spread.iter().map(|c| c.id.clone()).collect();
    backend.write_spans(spread).await.unwrap();

    let all = eventually("every span in the spread set", || async {
        let got = backend.query_spans_by_ids(&spread_ids, true).await.unwrap();
        (got.len() == spread_ids.len()).then_some(got)
    })
    .await;
    let with_bodies = all.iter().filter(|s| s.request_body.is_some()).count();
    assert_eq!(
        with_bodies,
        spread_ids.len(),
        "only {with_bodies} of {} spans came back with a body — a partial \
         window miss reports success",
        spread_ids.len()
    );
}
