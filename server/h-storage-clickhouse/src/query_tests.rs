//! Query-construction tests for the ClickHouse backend, against the shared
//! scripted HTTP mock (`crate::test_mock`).
//!
//! The live `it.rs` suite proves row round-trips but self-skips without a
//! server. These tests prove the **SQL the backend builds** for every list /
//! lookup query, which is where the project's read-path invariants live:
//!
//!   * **No `JOIN`** anywhere (CLAUDE.md read-path rule) — a regression to a JOIN is caught here,
//!     on every list query, without a database.
//!   * **Pagination has a total order** — every paginated `ORDER BY` must end with the row's id,
//!     else `LIMIT/OFFSET` can duplicate/drop rows across pages (the 26-span incident that
//!     motivated the rule).
//!   * **User input is escaped** for ClickHouse literals (backslash-aware) so a quote in a filter
//!     cannot break out.
//!   * **Time filters** use `fromUnixTimestamp64Micro(...)` so the MergeTree timestamp index stays
//!     usable.
//!
//! The mock answers count probes (u64 RowBinary) and returns empty result sets
//! otherwise, so `fetch_all`/`fetch_optional` paths run end to end.
#![cfg(test)]

use h_storage::query::{
    AgentActivityQuery, AgentSummaryQuery, DimensionFilter, DistinctAgentKindsQuery,
    FinishReasonsQuery, HttpExchangesQuery, MetricsModelsQuery, MetricsTimeseriesQuery,
    ServicesQuery, ServicesTopologyQuery, SessionListQuery, SessionTracesQuery, SpansQuery,
    TimeRange, TracesQuery,
};

use crate::test_mock::{test_backend, Mock};
use crate::ClickHouseBackend;

fn tr() -> TimeRange {
    TimeRange {
        start_us: 1_000_000,
        end_us: 2_000_000,
    }
}

fn traces_query(sort_by: &str) -> TracesQuery {
    TracesQuery {
        time_range: tr(),
        filter: DimensionFilter::default(),
        client_ips: vec![],
        server_ports: vec![],
        statuses: vec![],
        agent_kinds: vec![],
        sort_by: sort_by.to_string(),
        sort_order: "desc".to_string(),
        page: 2,
        page_size: 25,
        include_proxy_hops: false,
    }
}

fn spans_query(sort_by: &str) -> SpansQuery {
    SpansQuery {
        time_range: tr(),
        filter: DimensionFilter::default(),
        status_codes: vec![],
        finish_reasons: vec![],
        client_ips: vec![],
        server_ports: vec![],
        request_path_contains: None,
        is_stream: None,
        sort_by: sort_by.to_string(),
        sort_order: "desc".to_string(),
        page: 1,
        page_size: 10,
    }
}

fn exchanges_query(sort_by: &str) -> HttpExchangesQuery {
    HttpExchangesQuery {
        time_range: tr(),
        server_ips: vec![],
        client_ips: vec![],
        methods: vec![],
        status_codes: vec![],
        uri_contains: None,
        is_sse: None,
        sort_by: sort_by.to_string(),
        sort_order: "desc".to_string(),
        page: 1,
        page_size: 10,
    }
}

fn sessions_query() -> SessionListQuery {
    SessionListQuery {
        time_range: tr(),
        source_id: None,
        agent_kinds: vec![],
        cursor: None,
        page_size: 10,
    }
}

fn session_traces_query() -> SessionTracesQuery {
    SessionTracesQuery {
        source_id: "s".to_string(),
        session_id: "x".to_string(),
        cursor: None,
        page_size: 10,
    }
}

fn assert_no_join(sql: &str) {
    assert!(
        !sql.to_ascii_uppercase().contains(" JOIN "),
        "read-path JOIN detected:\n{sql}"
    );
}

#[test]
fn new_builds_scoped_and_admin_clients() {
    use h_common::config::ClickHouseConfig;
    let cfg = ClickHouseConfig {
        url: "http://127.0.0.1:1".to_string(),
        database: "heron".to_string(),
        user: "u".to_string(),
        password: "p".to_string(),
        optimize_on_sweep: true,
    };
    let b = ClickHouseBackend::new(&cfg).expect("build backend");
    assert!(b.optimize_on_sweep);
    // The unscoped client is what `init()` uses to CREATE DATABASE.
    let _ = b.admin_client();
}

#[tokio::test]
async fn init_issues_ddl_for_a_fresh_database() {
    let mock = Mock::start();
    let b = test_backend(&mock, false);
    h_storage::StorageBackend::init(&b).await.unwrap();

    let sql = mock.sql();
    assert!(
        sql.contains("CREATE DATABASE IF NOT EXISTS `heron`"),
        "{sql}"
    );
    for table in [
        "spans",
        "traces",
        "llm_metrics",
        "llm_finish_metrics",
        "http_exchanges",
    ] {
        assert!(
            sql.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "missing DDL for {table}:\n{sql}"
        );
    }
}

#[tokio::test]
async fn traces_paginates_with_id_tiebreak() {
    let mock = Mock::start();
    let page = test_backend(&mock, false)
        .query_traces(&traces_query("start_time"))
        .await
        .unwrap();
    assert_eq!(page.total, 0);
    assert!(page.items.is_empty());

    let sql = mock.sql();
    // Pagination total order: sort key then the id tie-break.
    assert!(
        sql.contains("ORDER BY start_time DESC, turn_id ASC"),
        "{sql}"
    );
    // page 2, page_size 25 → OFFSET 25.
    assert!(sql.contains("LIMIT 25 OFFSET 25"), "{sql}");
    // Time bounds keep the MergeTree index usable.
    assert!(sql.contains("fromUnixTimestamp64Micro(1000000)"), "{sql}");
    assert!(sql.contains("fromUnixTimestamp64Micro(2000000)"), "{sql}");
    assert_no_join(&sql);
}

#[tokio::test]
async fn spans_paginates_with_id_tiebreak() {
    let mock = Mock::start();
    let page = test_backend(&mock, false)
        .query_spans(&spans_query("request_time"))
        .await
        .unwrap();
    assert!(page.items.is_empty());
    let sql = mock.sql();
    assert!(sql.contains("ORDER BY request_time DESC, id ASC"), "{sql}");
    assert_no_join(&sql);
}

#[tokio::test]
async fn http_exchanges_paginate_with_id_tiebreak() {
    let mock = Mock::start();
    let page = test_backend(&mock, false)
        .query_http_exchanges(&exchanges_query("request_time"))
        .await
        .unwrap();
    assert!(page.items.is_empty());
    let sql = mock.sql();
    assert!(sql.contains(", id ASC"), "{sql}");
    assert_no_join(&sql);
}

#[tokio::test]
async fn session_traces_paginate_with_turn_id_tiebreak() {
    let mock = Mock::start();
    let page = test_backend(&mock, false)
        .query_session_traces(&session_traces_query())
        .await
        .unwrap();
    assert!(page.items.is_empty());
    let sql = mock.sql();
    assert!(
        sql.contains("ORDER BY start_time DESC, turn_id DESC"),
        "{sql}"
    );
    assert_no_join(&sql);
}

#[tokio::test]
async fn filter_values_are_clickhouse_escaped() {
    let mock = Mock::start();
    let mut q = traces_query("start_time");
    q.filter.wire_apis = vec!["a'b".to_string()];
    q.filter.models = vec!["m'--".to_string()];
    q.filter.server_ips = vec!["1.1.1.1".to_string()];
    q.client_ips = vec!["c'd".to_string()];
    test_backend(&mock, false).query_traces(&q).await.unwrap();

    let sql = mock.sql();
    assert!(sql.contains("wire_api IN ('a''b')"), "{sql}");
    assert!(sql.contains("'m''--'"), "{sql}");
    assert!(sql.contains("client_ip IN ('c''d')"), "{sql}");
    // The unescaped form must never appear (that is the injection).
    assert!(!sql.contains("'a'b'"), "{sql}");
    assert_no_join(&sql);
}

#[tokio::test]
async fn unknown_sort_field_is_rejected_before_any_query() {
    let mock = Mock::start();
    let backend = test_backend(&mock, false);

    for bad in ["nope", "start_time; DROP TABLE spans"] {
        assert!(
            backend.query_traces(&traces_query(bad)).await.is_err(),
            "traces accepted sort_by={bad:?}"
        );
        assert!(backend.query_spans(&spans_query(bad)).await.is_err());
        assert!(backend
            .query_http_exchanges(&exchanges_query(bad))
            .await
            .is_err());
    }
    assert!(
        mock.statements().is_empty(),
        "a rejected sort field must not reach the server: {:?}",
        mock.statements()
    );
}

#[tokio::test]
async fn id_lookups_escape_the_id_literal() {
    let mock = Mock::start();
    let backend = test_backend(&mock, false);
    let _ = backend.query_trace_by_id("t'--").await.unwrap();
    let _ = backend.query_span_by_id("i'--").await.unwrap();
    let _ = backend.query_http_exchange_by_id("e'--").await.unwrap();

    let sql = mock.sql();
    assert!(sql.contains("turn_id = 't''--'"), "{sql}");
    assert!(sql.contains("id = 'i''--'"), "{sql}");
    assert!(sql.contains("id = 'e''--'"), "{sql}");
    assert_no_join(&sql);
}

#[tokio::test]
async fn no_join_and_no_statement_separator_across_all_list_queries() {
    let mock = Mock::start();
    let backend = test_backend(&mock, false);

    let _ = backend.query_traces(&traces_query("start_time")).await;
    let _ = backend.query_spans(&spans_query("request_time")).await;
    let _ = backend
        .query_http_exchanges(&exchanges_query("request_time"))
        .await;
    let _ = backend.query_sessions(&sessions_query()).await;
    let _ = backend.query_session_traces(&session_traces_query()).await;
    let _ = backend.query_distinct_wire_apis().await;
    let _ = backend.query_distinct_models().await;
    let _ = backend.query_distinct_server_ips().await;
    let _ = backend.query_distinct_finish_reasons().await;
    let _ = backend.query_pair_candidates(1_000_000, 2_000_000).await;
    let _ = backend
        .query_distinct_agent_kinds(&DistinctAgentKindsQuery {
            time_range: tr(),
            filter: DimensionFilter::default(),
            include_proxy_hops: true,
        })
        .await;
    let _ = backend
        .query_metrics_timeseries(&MetricsTimeseriesQuery {
            time_range: tr(),
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["call_count".to_string()],
            group_by: None,
        })
        .await;
    let _ = backend
        .query_metrics_models(&MetricsModelsQuery {
            time_range: tr(),
            filter: DimensionFilter::default(),
            sort_by: "call_count".to_string(),
            sort_order: "desc".to_string(),
            limit: 10,
        })
        .await;
    let _ = backend
        .query_finish_reasons(&FinishReasonsQuery {
            time_range: tr(),
            granularity: "1m".to_string(),
            wire_apis: vec![],
            models: vec![],
            server_ips: vec![],
        })
        .await;
    let _ = backend
        .query_agent_summary(&AgentSummaryQuery { time_range: tr() })
        .await;
    let _ = backend
        .query_agent_activity(&AgentActivityQuery {
            time_range: tr(),
            bucket_seconds: None,
        })
        .await;
    let _ = backend
        .query_services(&ServicesQuery {
            time_range: tr(),
            sort_by: "call_count".to_string(),
            sort_order: "desc".to_string(),
            limit: 10,
        })
        .await;
    let _ = backend
        .query_services_topology(&ServicesTopologyQuery { time_range: tr() })
        .await;
    let _ = backend.query_trace_by_id("t").await;
    let _ = backend.query_span_by_id("s").await;
    let _ = backend.query_http_exchange_by_id("e").await;
    let _ = backend.query_trace_spans("t", true).await;
    let _ = backend.query_spans_by_ids(&["a".to_string()], true).await;

    let sql = mock.sql();
    assert_no_join(&sql);
    // Interpolated user data is escaped, so no statement separator can appear.
    assert!(!sql.contains(';'), "statement separator in:\n{sql}");
    // Advisory: the battery must actually have issued many statements.
    assert!(
        mock.statements().len() >= 10,
        "{} statements",
        mock.statements().len()
    );
}

#[tokio::test]
async fn write_paths_issue_rowbinary_inserts() {
    use h_storage::StorageBackend;

    use crate::it::fixtures::{
        sample_call, sample_exchange, sample_finish_metric, sample_metric, sample_turn,
    };

    let mock = Mock::start();
    let b = test_backend(&mock, false);
    let ts = 1_700_000_000_000_000i64;

    StorageBackend::write_spans(&b, vec![sample_call("c1", ts)])
        .await
        .unwrap();
    StorageBackend::write_metrics(&b, vec![sample_metric("1m", ts)])
        .await
        .unwrap();
    StorageBackend::write_finish_metrics(&b, vec![sample_finish_metric("1m", ts, "stop")])
        .await
        .unwrap();
    StorageBackend::write_traces(&b, vec![sample_turn("t1", "s1", ts, vec!["c1"])])
        .await
        .unwrap();
    StorageBackend::write_exchanges(&b, vec![sample_exchange("e1", ts)])
        .await
        .unwrap();

    let sql = mock.sql();
    assert!(sql.contains("INSERT INTO"), "no insert issued:\n{sql}");
    for table in [
        "spans",
        "llm_metrics",
        "llm_finish_metrics",
        "traces",
        "http_exchanges",
    ] {
        assert!(sql.contains(table), "no insert for {table}:\n{sql}");
    }
}
