//! Query-construction tests for the aglake backend against the shared search
//! mock (`crate::search_mock`).
//!
//! The live `it.rs` suite decodes real events but self-skips without an
//! aglaked daemon. These tests drive every list / lookup / aggregate method
//! through the real `SearchClient` and assert the SPL that reaches the wire —
//! which is where the read-path invariants live (a page count companion, a
//! deterministic sort tie-break, index/time scoping). Empty/one-row mocked
//! results keep the row-mapping code on the path too.
#![cfg(test)]

use h_storage::query::{
    AgentActivityQuery, AgentSummaryQuery, DimensionFilter, DistinctAgentKindsQuery,
    FinishReasonsQuery, HttpExchangesQuery, MetricsModelsQuery, MetricsSummaryQuery,
    MetricsTimeseriesQuery, ServicesQuery, ServicesTopologyQuery, SessionListQuery, SpansQuery,
    TimeRange, TracesQuery,
};

use crate::search_mock::{test_backend, MockAglake};

fn tr() -> TimeRange {
    TimeRange {
        start_us: 1_000_000,
        end_us: 2_000_000,
    }
}

fn traces_query() -> TracesQuery {
    TracesQuery {
        time_range: tr(),
        filter: DimensionFilter::default(),
        client_ips: vec![],
        server_ports: vec![],
        statuses: vec![],
        agent_kinds: vec![],
        sort_by: "start_time".to_string(),
        sort_order: "desc".to_string(),
        page: 1,
        page_size: 10,
        include_proxy_hops: false,
    }
}

fn spans_query() -> SpansQuery {
    SpansQuery {
        time_range: tr(),
        filter: DimensionFilter::default(),
        status_codes: vec![],
        finish_reasons: vec![],
        client_ips: vec![],
        server_ports: vec![],
        request_path_contains: None,
        is_stream: None,
        sort_by: "request_time".to_string(),
        sort_order: "desc".to_string(),
        page: 1,
        page_size: 10,
    }
}

fn exchanges_query() -> HttpExchangesQuery {
    HttpExchangesQuery {
        time_range: tr(),
        server_ips: vec![],
        client_ips: vec![],
        methods: vec![],
        status_codes: vec![],
        uri_contains: None,
        is_sse: None,
        sort_by: "request_time".to_string(),
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

#[tokio::test]
async fn distincts_and_agent_rollups_build_stats_queries() {
    let mock = MockAglake::start();
    let b = test_backend(&mock);

    let wire = b.query_distinct_wire_apis().await.unwrap();
    assert_eq!(wire, vec!["alpha".to_string()]);
    let _ = b.query_distinct_models().await.unwrap();
    let _ = b.query_distinct_server_ips().await.unwrap();
    let fr = b.query_distinct_finish_reasons().await.unwrap();
    assert_eq!(fr.len(), 1);
    let kinds = b
        .query_distinct_agent_kinds(&DistinctAgentKindsQuery {
            time_range: tr(),
            filter: DimensionFilter::default(),
            include_proxy_hops: false,
        })
        .await
        .unwrap();
    assert_eq!(kinds, vec!["alpha".to_string()]);
    let summary = b
        .query_agent_summary(&AgentSummaryQuery { time_range: tr() })
        .await
        .unwrap();
    assert_eq!(summary.len(), 1);
    let activity = b
        .query_agent_activity(&AgentActivityQuery {
            time_range: tr(),
            bucket_seconds: None,
        })
        .await
        .unwrap();
    assert_eq!(activity.len(), 1);

    assert!(mock.saw("stats count by"), "{:?}", mock.queries());
    assert!(mock.saw("by wire_api, finish_reason"));
    assert!(mock.saw("avg_duration_ms"));
    assert!(mock.saw("bin _time span="));
}

#[tokio::test]
async fn paginated_lists_issue_a_count_companion_and_sorted_page() {
    let mock = MockAglake::start();
    let b = test_backend(&mock);

    let _ = b.query_http_exchanges(&exchanges_query()).await.unwrap();
    let _ = b.query_spans(&spans_query()).await.unwrap();
    let _ = b.query_sessions(&sessions_query()).await.unwrap();
    let _ = b.query_traces(&traces_query()).await.unwrap();

    // Every page runs a count query and a sorted rows query.
    assert!(mock.saw("stats count as n"), "{:?}", mock.queries());
    assert!(mock.saw("| sort"), "{:?}", mock.queries());
    // Offset pagination is bounded by max_page_offset (not a silent slow scan).
    assert!(mock.saw("streamstats count as _rn"));
    // The exchange page carries its deterministic tie-break.
    assert!(mock.saw("str(id)"));

    // No unbounded search leaked out of a windowed list query.
    for q in mock.queries() {
        assert!(!q.contains("earliest") && !q.contains("latest"), "{q}");
    }
}

#[tokio::test]
async fn point_lookups_exercise_the_raw_fetch_path() {
    let mock = MockAglake::start();
    let b = test_backend(&mock);

    // Empty mocked results → None / empty, but the query is built and issued.
    assert!(b.query_http_exchange_by_id("nope").await.unwrap().is_none());
    assert!(b.query_span_by_id("nope").await.unwrap().is_none());
    assert!(b.query_trace_by_id("nope").await.unwrap().is_none());
    let _ = b.query_trace_spans("nope", true).await.unwrap();
    let _ = b.query_session_by_id("s", "x").await.unwrap();

    // Reads fetch whole events and decode in Rust, never assembled fields.
    assert!(mock.saw("| table _raw"), "{:?}", mock.queries());
}

#[tokio::test]
async fn metrics_queries_build() {
    let mock = MockAglake::start();
    let b = test_backend(&mock);

    let _ = b
        .query_metrics_timeseries(&MetricsTimeseriesQuery {
            time_range: tr(),
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["call_count".to_string()],
            group_by: None,
        })
        .await
        .unwrap();
    let _ = b
        .query_metrics_summary(&MetricsSummaryQuery {
            time_range: tr(),
            filter: DimensionFilter::default(),
        })
        .await
        .unwrap();
    let _ = b
        .query_metrics_models(&MetricsModelsQuery {
            time_range: tr(),
            filter: DimensionFilter::default(),
            sort_by: "call_count".to_string(),
            sort_order: "desc".to_string(),
            limit: 10,
        })
        .await
        .unwrap();
    let _ = b
        .query_finish_reasons(&FinishReasonsQuery {
            time_range: tr(),
            granularity: "1m".to_string(),
            wire_apis: vec![],
            models: vec![],
            server_ips: vec![],
        })
        .await
        .unwrap();

    assert!(!mock.queries().is_empty());
}

#[tokio::test]
async fn services_queries_build() {
    let mock = MockAglake::start();
    let b = test_backend(&mock);

    let _ = b
        .query_services(&ServicesQuery {
            time_range: tr(),
            sort_by: "call_count".to_string(),
            sort_order: "desc".to_string(),
            limit: 10,
        })
        .await
        .unwrap();
    let _ = b
        .query_services_topology(&ServicesTopologyQuery { time_range: tr() })
        .await
        .unwrap();

    assert!(mock.saw("stats"), "{:?}", mock.queries());
}
