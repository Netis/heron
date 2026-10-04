//! Retention sweeper tests against the shared scripted ClickHouse HTTP mock
//! (`crate::test_mock`).
//!
//! Closes the highest-risk Tier 0 gap — the retention DELETE path
//! (docs/design/11-verification.md, `CLICKHOUSE-DELETE-001`) — without a live
//! server. What this proves: which tables are swept, with which predicates,
//! that the per-granularity label is escaped, that `OPTIMIZE FINAL` is issued
//! exactly when configured, that the report carries the pre-delete counts, and
//! that a server error propagates as `Err` rather than a silent zero-row
//! report.
//!
//! It does NOT prove ClickHouse's own delete semantics (the live `it.rs` suite
//! does, when a server is present).
#![cfg(test)]

use std::time::{Duration, SystemTime};

use h_storage::retention::RetentionPolicy;

use crate::test_mock::{test_backend, Mock};

fn micros(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

#[tokio::test]
// @scenario CLICKHOUSE-DELETE-001 integration
async fn sweeps_each_table_and_reports_predelete_counts() {
    let mock = Mock::start();
    mock.set_count("spans", 3);
    mock.set_count("http_exchanges", 4);
    mock.set_count("traces", 5);
    mock.set_count("llm_metrics", 6);
    mock.set_count("llm_finish_metrics", 999);

    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let policy = RetentionPolicy {
        spans_before: Some(t),
        traces_before: Some(t),
        http_exchanges_before: Some(t),
        metrics_before: vec![("1m".to_string(), t), ("5m".to_string(), t)],
    };

    let report = test_backend(&mock, false)
        .apply_retention(policy)
        .await
        .unwrap();

    assert_eq!(report.spans_deleted, 3);
    assert_eq!(report.http_exchanges_deleted, 4);
    assert_eq!(report.traces_deleted, 5);
    assert_eq!(report.metrics_deleted.get("1m"), Some(&6));
    assert_eq!(report.metrics_deleted.get("5m"), Some(&6));
    // llm_finish_metrics count is deliberately not surfaced (matches DuckDB).
    assert_eq!(report.total(), 3 + 4 + 5 + 6 + 6);

    let us = micros(t);
    let joined = mock.sql();
    assert!(
        joined.contains(&format!(
            "DELETE FROM spans WHERE request_time < fromUnixTimestamp64Micro({us})"
        )),
        "{joined}"
    );
    assert!(
        joined.contains(&format!(
            "DELETE FROM http_exchanges WHERE request_time < fromUnixTimestamp64Micro({us})"
        )),
        "{joined}"
    );
    assert!(
        joined.contains(&format!(
            "DELETE FROM traces WHERE end_time < fromUnixTimestamp64Micro({us})"
        )),
        "{joined}"
    );
    assert!(
        joined.contains(&format!(
            "DELETE FROM llm_metrics WHERE granularity = '1m' AND timestamp < \
             fromUnixTimestamp64Micro({us})"
        )),
        "{joined}"
    );
    // Both metric tables are swept in lock-step.
    assert!(
        joined.contains("DELETE FROM llm_finish_metrics WHERE granularity = '1m'"),
        "{joined}"
    );
    // No OPTIMIZE when the flag is off.
    assert!(!joined.contains("OPTIMIZE"), "{joined}");
}

#[tokio::test]
async fn optimize_final_runs_only_for_swept_tables_when_enabled() {
    let mock = Mock::start();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let policy = RetentionPolicy {
        spans_before: Some(t),
        traces_before: None,
        http_exchanges_before: None,
        metrics_before: vec![("1m".to_string(), t)],
    };

    let _ = test_backend(&mock, true)
        .apply_retention(policy)
        .await
        .unwrap();

    let joined = mock.sql();
    assert!(joined.contains("OPTIMIZE TABLE spans FINAL"), "{joined}");
    assert!(
        joined.contains("OPTIMIZE TABLE llm_metrics FINAL"),
        "{joined}"
    );
    assert!(
        joined.contains("OPTIMIZE TABLE llm_finish_metrics FINAL"),
        "{joined}"
    );
    // A table with no cutoff must not be optimized.
    assert!(!joined.contains("OPTIMIZE TABLE traces"), "{joined}");
    assert!(
        !joined.contains("OPTIMIZE TABLE http_exchanges"),
        "{joined}"
    );
}

#[tokio::test]
async fn empty_policy_makes_no_requests() {
    let mock = Mock::start();
    let report = test_backend(&mock, false)
        .apply_retention(RetentionPolicy::default())
        .await
        .unwrap();
    assert_eq!(report.total(), 0);
    assert!(mock.statements().is_empty(), "{:?}", mock.statements());
}

#[tokio::test]
async fn granularity_label_is_escaped_in_the_predicate() {
    let mock = Mock::start();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let policy = RetentionPolicy {
        metrics_before: vec![("o'brien".to_string(), t)],
        ..Default::default()
    };
    let _ = test_backend(&mock, false)
        .apply_retention(policy)
        .await
        .unwrap();
    let joined = mock.sql();
    assert!(joined.contains("granularity = 'o''brien'"), "{joined}");
    assert!(!joined.contains("granularity = 'o'brien'"), "{joined}");
}

#[tokio::test]
async fn count_failure_propagates_as_error() {
    let mock = Mock::start();
    mock.fail_on("SELECT count()");
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let policy = RetentionPolicy {
        spans_before: Some(t),
        ..Default::default()
    };

    let err = test_backend(&mock, false).apply_retention(policy).await;
    assert!(
        err.is_err(),
        "count failure must not become a zero-row report"
    );
}
