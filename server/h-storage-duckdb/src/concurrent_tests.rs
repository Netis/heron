//! Cross-entity concurrency tests — spans calls / metrics / turns.

use crate::DuckDbBackend;
use std::net::IpAddr;
use std::sync::Arc;
use tempfile::TempDir;
use h_common::agent::{AgentTopology, ToolSurface};
use h_llm::model::{ApiType, LlmCall};
use h_llm::wire_apis as wa;
use h_metrics::model::LlmMetric;
use h_storage::query::{
    DimensionFilter, DistinctAgentKindsQuery, TimeRange, TurnsQuery,
};
use h_storage::StorageBackend;
use h_turn::{AgentTurn, TurnStatus};

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
        body_bytes_dropped: 0,
        process: None,
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
        .query_row("SELECT COUNT(*) FROM spans", [], |r| r.get(0))
        .unwrap();
    let turns_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM traces", [], |r| r.get(0))
        .unwrap();
    let metrics_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM llm_metrics", [], |r| r.get(0))
        .unwrap();
    assert_eq!(calls_count, expected);
    assert_eq!(turns_count, expected);
    assert_eq!(metrics_count, expected);
}

#[tokio::test]
async fn llm_call_round_trip_with_agent_fields() {
    let backend = DuckDbBackend::open(":memory:").unwrap();
    backend.init().await.unwrap();

    let mut call = mk_call(0);
    call.is_agent_request = true;
    call.tool_surface = Some(ToolSurface::FunctionCall);
    call.agent_topology = Some(AgentTopology::SingleAgent);
    call.tool_call_count = 3;
    call.tool_names = vec!["Read".to_string(), "Edit".to_string()];
    let call_id = call.id.clone();

    backend.write_calls(vec![call]).await.unwrap();

    let back = backend
        .query_call_by_id(&call_id)
        .await
        .unwrap()
        .expect("call should round-trip");

    assert!(back.is_agent_request);
    assert_eq!(back.tool_surface.as_deref(), Some("function_call"));
    assert_eq!(back.agent_topology.as_deref(), Some("single_agent"));
    assert_eq!(back.tool_call_count, 3);
    assert_eq!(
        back.tool_names,
        vec!["Read".to_string(), "Edit".to_string()]
    );
}

// Verify `reopen_all_connections` rebuilds **every** connection — all
// writers AND every reader-pool entry — so a real DuckDB FATAL can be
// recovered without a process restart. Before the fix this method
// touched only the turns writer; reads kept failing because the pool
// still held `try_clone`'d handles to the original (poisoned)
// in-process instance.
//
// We can't deterministically trigger the DuckDB
// "Failed to remove all rows while merging checkpoint deltas" FATAL
// in a unit test, but we can exercise the recovery code path and
// assert that every downstream surface (writes, table-scoped reads,
// pool-backed reads) keeps working across the reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopen_all_connections_keeps_reads_and_writes_alive() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("reopen.duckdb");
    // Pool size = 2: small enough that any stale-handle reuse would
    // surface on the second query.
    let backend =
        Arc::new(DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).unwrap());
    backend.init().await.unwrap();

    backend.write_turns(vec![mk_turn(0)]).await.unwrap();

    backend.reopen_all_connections().await.unwrap();

    // Read path 1: turn list query — goes through the read pool.
    let turns_q = TurnsQuery {
        time_range: TimeRange {
            start_us: 1_700_000_000_000_000 - 1_000_000,
            end_us: 1_700_000_000_000_000 + 60_000_000_000,
        },
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
    };
    let page = backend.query_turns(&turns_q).await.unwrap();
    assert_eq!(
        page.total, 1,
        "query_turns after reopen must see the pre-reopen row"
    );

    // Read path 2: distinct agent kinds — different code path but
    // also uses the read pool. Loop a few times to drain at least
    // pool_size + 1 connections so any stale-handle reuse would
    // surface.
    for _ in 0..3 {
        let kinds = backend
            .query_distinct_agent_kinds(&DistinctAgentKindsQuery {
                time_range: turns_q.time_range.clone(),
                filter: DimensionFilter::default(),
                include_proxy_hops: false,
            })
            .await
            .unwrap();
        assert!(
            kinds.iter().any(|k| k == "test"),
            "query_distinct_agent_kinds after reopen must see the inserted agent_kind"
        );
    }

    // Write path on every writer mutex — proves all four writers
    // were rebuilt, not just turns.
    backend.write_turns(vec![mk_turn(1)]).await.unwrap();
    backend.write_calls(vec![mk_call(0)]).await.unwrap();
    backend.write_metrics(vec![mk_metric(0)]).await.unwrap();

    // And re-confirm the post-reopen reads pick up the new row.
    let page2 = backend.query_turns(&turns_q).await.unwrap();
    assert_eq!(
        page2.total, 2,
        "post-reopen writes must persist and be queryable"
    );
}

// Deterministic counterpart to the test above. Where that one can only
// say "if we call reopen, downstream surfaces keep working", this one
// drives the actual sequence the production sweeper sees:
//
//   1. A write fails with a FATAL invalidation,
//   2. `reopen_all_connections` rebuilds the writer + reader set,
//   3. Subsequent writes and reads on every code path succeed.
//
// PR#48 was missed because the prod failure could not be reproduced from
// a unit test (real DuckDB FATAL needs real load pressure). The
// fault-injection path closes that gap: armed faults look identical to
// the real FATAL from the recovery code's perspective.
#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopen_recovers_from_injected_duckdb_invalidate() {
    use crate::fault_injection::{FaultGuard, FaultPoint};

    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("reopen-injected.duckdb");
    let backend =
        Arc::new(DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).unwrap());
    backend.init().await.unwrap();

    // Baseline write, before any fault — exercises the writer set.
    backend.write_turns(vec![mk_turn(0)]).await.unwrap();

    // ARM: every write path now synthesizes a FATAL invalidation error
    // identical in shape to what DuckDB returns when its in-process
    // instance dies.
    {
        let _guard = FaultGuard::arm(backend.fault_set(), FaultPoint::DuckDbInvalidate);
        assert!(backend.fault_set().should_fire(FaultPoint::DuckDbInvalidate));

        let err = backend
            .write_turns(vec![mk_turn(1)])
            .await
            .expect_err("write_turns must surface the injected FATAL");
        assert!(
            format!("{err}").contains("FATAL"),
            "injected error message must mention FATAL: got {err}"
        );

        let err = backend
            .write_calls(vec![mk_call(0)])
            .await
            .expect_err("write_calls must surface the injected FATAL");
        assert!(format!("{err}").contains("FATAL"));

        let err = backend
            .write_metrics(vec![mk_metric(0)])
            .await
            .expect_err("write_metrics must surface the injected FATAL");
        assert!(format!("{err}").contains("FATAL"));

        // Recover. reopen_all_connections itself is NOT gated by the
        // fault — it must succeed even while the fault is armed,
        // because in production the sweeper invokes reopen *because*
        // writes are FATAL'ing.
        backend.reopen_all_connections().await.unwrap();
        // _guard drops here, disarming DuckDbInvalidate.
    }

    assert!(
        !backend.fault_set().should_fire(FaultPoint::DuckDbInvalidate),
        "FaultGuard::drop must disarm"
    );

    // Post-recovery: every writer mutex must accept fresh work, and
    // every reader path (turn list, distinct-agent-kinds — backed by
    // the pool that `reopen_all_connections` is supposed to have fully
    // replaced) must serve the new row plus the pre-fault baseline.
    backend.write_turns(vec![mk_turn(2)]).await.unwrap();
    backend.write_calls(vec![mk_call(0)]).await.unwrap();
    backend.write_metrics(vec![mk_metric(0)]).await.unwrap();

    let turns_q = TurnsQuery {
        time_range: TimeRange {
            start_us: 1_700_000_000_000_000 - 1_000_000,
            end_us: 1_700_000_000_000_000 + 60_000_000_000,
        },
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
    };
    let page = backend.query_turns(&turns_q).await.unwrap();
    assert_eq!(
        page.total, 2,
        "post-recovery list query must see baseline turn-0 + new turn-2"
    );

    // Drain the pool more than its size so any stale-handle reuse on
    // any reader slot would surface.
    for _ in 0..4 {
        let kinds = backend
            .query_distinct_agent_kinds(&DistinctAgentKindsQuery {
                time_range: turns_q.time_range.clone(),
                filter: DimensionFilter::default(),
                include_proxy_hops: false,
            })
            .await
            .unwrap();
        assert!(
            kinds.iter().any(|k| k == "test"),
            "post-recovery distinct-kinds must see the seeded agent_kind"
        );
    }
}

// Independently verify the ReadPoolPoisoned injection point —
// `pair_sweeper` and a few API surfaces depend on pool acquires never
// silently returning a stale connection. The fault makes the next
// `acquire()` return a poisoned-pool error so tests can exercise the
// downstream handling path.
#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_pool_poisoned_fault_propagates_to_reads() {
    use crate::fault_injection::{FaultGuard, FaultPoint};

    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("pool-poison.duckdb");
    let backend =
        Arc::new(DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).unwrap());
    backend.init().await.unwrap();
    backend.write_turns(vec![mk_turn(0)]).await.unwrap();

    let turns_q = TurnsQuery {
        time_range: TimeRange {
            start_us: 1_700_000_000_000_000 - 1_000_000,
            end_us: 1_700_000_000_000_000 + 60_000_000_000,
        },
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
    };

    {
        let _guard = FaultGuard::arm(backend.fault_set(), FaultPoint::ReadPoolPoisoned);
        let err = backend
            .query_turns(&turns_q)
            .await
            .expect_err("query must surface poisoned-pool error");
        assert!(
            format!("{err}").contains("read pool poisoned"),
            "injected error must mention pool poisoning: got {err}"
        );
    }

    // After guard drops, queries work again.
    let page = backend.query_turns(&turns_q).await.unwrap();
    assert_eq!(page.total, 1);
}

// ── Chaos under load (PR3) ──────────────────────────────────────────────────
//
// The fault tests above are deterministic single writes: arm → one write fails
// → reopen → one write succeeds. These drive a SUSTAINED concurrent write load
// and arm the fault *while writers are hammering the storage*, asserting the
// property that actually matters in production: a write either commits (its rows
// are queryable) or returns Err (the caller is told) — never silently lost, never
// partially inserted — and the backend stays alive and recovers via the same
// `reopen_all_connections` path the pair-sweeper drives on a real FATAL.
//
// Structured in phases (healthy → armed → recovered) rather than timing-raced so
// the fault is *guaranteed* exercised under concurrency without flakiness.
//
// Deliberately out of scope (documented, not silently omitted): a real ENOSPC
// via a small tmpfs and a checkpoint-at-102 GB repro — both need a mounted
// filesystem / privileges a unit test doesn't have. The injected `DiskFull`
// here exercises the write-path *handling* of that error class.

#[cfg(feature = "fault-injection")]
const CHAOS_BATCH: usize = 25;

/// Run a concurrent burst: `writers` tasks each flush `rounds` batches of
/// `CHAOS_BATCH` calls. Ids are offset by `id_base` so they never collide across
/// phases (→ row COUNT equals exactly the rows inserted). Returns
/// `(ok_batches, err_batches)` summed across writers. A faulted write must come
/// back as Err — a panicking writer task fails the test.
#[cfg(feature = "fault-injection")]
async fn chaos_burst(
    backend: &Arc<DuckDbBackend>,
    writers: usize,
    rounds: usize,
    id_base: usize,
) -> (usize, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ok = Arc::new(AtomicUsize::new(0));
    let err = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for w in 0..writers {
        let be = backend.clone();
        let ok = ok.clone();
        let err = err.clone();
        tasks.push(tokio::spawn(async move {
            for r in 0..rounds {
                let base = id_base + (w * rounds + r) * CHAOS_BATCH;
                let batch: Vec<_> = (0..CHAOS_BATCH).map(|i| mk_call(base + i)).collect();
                match be.write_calls(batch).await {
                    Ok(_) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        err.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for t in tasks {
        t.await
            .expect("writer task must not panic — an injected fault must surface as Err");
    }
    (ok.load(Ordering::Relaxed), err.load(Ordering::Relaxed))
}

#[cfg(feature = "fault-injection")]
fn count_calls(backend: &DuckDbBackend) -> usize {
    let conn = backend.test_conn().lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM spans", [], |r| r.get(0))
        .unwrap();
    n as usize
}

#[cfg(feature = "fault-injection")]
async fn chaos_under_load(fault: crate::fault_injection::FaultPoint) {
    const WRITERS: usize = 3;
    const ROUNDS: usize = 8;
    let healthy = WRITERS * ROUNDS; // batches every burst commits when un-faulted

    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("chaos.duckdb");
    let backend = Arc::new(DuckDbBackend::open(path.to_str().unwrap()).unwrap());
    backend.init().await.unwrap();

    // Phase 1 — healthy concurrent load: everything commits.
    let (ok1, err1) = chaos_burst(&backend, WRITERS, ROUNDS, 0).await;
    assert_eq!((ok1, err1), (healthy, 0), "no fault armed yet");

    // Phase 2 — fault armed while writers hammer: every write must fail with Err
    // (no panic, no partial insert) and nothing must land in the table.
    backend.fault_set().arm(fault);
    let (ok2, err2) = chaos_burst(&backend, WRITERS, ROUNDS, 10_000).await;
    backend.fault_set().disarm(fault);
    assert_eq!(ok2, 0, "every write under the armed fault must fail");
    assert_eq!(err2, healthy, "the fault must be exercised on every batch");
    assert_eq!(
        count_calls(&backend),
        healthy * CHAOS_BATCH,
        "faulted batches inserted nothing — only Phase 1 rows present"
    );

    // Recover via the production path (the pair-sweeper calls this on a FATAL).
    backend.reopen_all_connections().await.unwrap();

    // Phase 3 — post-recovery concurrent load: writes succeed again.
    let (ok3, err3) = chaos_burst(&backend, WRITERS, ROUNDS, 20_000).await;
    assert_eq!((ok3, err3), (healthy, 0), "writes must succeed after recovery");

    // No silent loss, no double-count: rows == exactly the committed batches
    // (Phase 1 + Phase 3); the faulted Phase 2 contributed nothing.
    assert_eq!(
        count_calls(&backend),
        (ok1 + ok3) * CHAOS_BATCH,
        "row count must equal exactly the committed batches — no silent loss"
    );
}

/// A DuckDB FATAL/invalidation arriving mid-load: writes during the window fail
/// cleanly, `reopen_all_connections` recovers, and no committed row is lost.
#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duckdb_invalidate_mid_load_recovers_with_no_silent_loss() {
    chaos_under_load(crate::fault_injection::FaultPoint::DuckDbInvalidate).await;
}

/// ENOSPC/`DiskFull` arriving mid-load. This is the first test to ARM the
/// `DiskFull` fault — it is wired into every write path but was previously
/// defined-but-never-triggered. Writes fail gracefully and recovery is clean.
#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disk_full_mid_load_is_graceful_and_recovers() {
    chaos_under_load(crate::fault_injection::FaultPoint::DiskFull).await;
}
