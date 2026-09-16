//! Schema-migration integration tests.
//!
//! Each test synthesizes the on-disk shape of an older `heron`
//! release directly via DDL — no historical binary build required — then
//! runs the current `DuckDbBackend::init()` against it and asserts that:
//!
//!   1. init succeeds (schema reconciles cleanly)
//!   2. the expected migrations actually fired (columns added/dropped,
//!      legacy values rewritten)
//!   3. canonical read paths still work
//!
//! This locks in the existing migration code in `schema.rs` against the
//! "PR#48 class" — silent regressions where a future refactor breaks the
//! upgrade path without breaking any fresh-install test.
//!
//! Binary `.duckdb` fixtures (per release tag) are out of scope: they
//! require building each tagged binary, take meaningful disk, and add
//! drift risk between fixture and reality. The synthesized approach
//! covers the same migration code paths deterministically. See
//! `testdata/golden-dbs/README.md` for the binary-fixture flow if/when
//! per-tag fixtures become necessary.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use duckdb::Connection;
use tempfile::TempDir;

use h_common::process::ProcessInfo;
use h_llm::model::{ApiType, LlmCall};
use h_storage::query::{DimensionFilter, TimeRange, TracesQuery};
use h_storage::StorageBackend;
use h_storage_duckdb::DuckDbBackend;

/// Minimal `LlmCall` for write-path round-trip assertions. `tool_call_count`
/// and `body_bytes_dropped` are set to distinct sentinels so a positional
/// appender misalignment (writing one tail column into another) is caught.
fn sample_call(id: &str, tool_call_count: u32, body_bytes_dropped: u64) -> LlmCall {
    LlmCall {
        source_id: "test".into(),
        id: id.into(),
        wire_api: "openai-chat",
        model: "gpt-test".into(),
        api_type: ApiType::Chat,
        request_time: 1_000_000,
        response_time: Some(1_000_500),
        complete_time: Some(1_001_000),
        request_path: "/v1/chat/completions".into(),
        is_stream: false,
        request_body: Some("{}".into()),
        status_code: Some(200),
        finish_reason: Some("stop".into()),
        response_body: Some("{}".into()),
        input_tokens: Some(10),
        output_tokens: Some(20),
        total_tokens: Some(30),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        ttft_ms: Some(5.0),
        e2e_latency_ms: Some(10.0),
        client_ip: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        client_port: 1234,
        server_ip: IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2)),
        server_port: 443,
        response_id: Some("resp-1".into()),
        request_headers: vec![],
        response_headers: vec![],
        is_agent_request: false,
        tool_surface: None,
        agent_topology: None,
        tool_call_count,
        tool_names: vec![],
        body_bytes_dropped,
        attribution: h_common::attribution::AttributionInfo::ambiguous(),
        process: None,
    }
}

/// Schema shape immediately before Phase 5 added the agent-classification
/// columns to `llm_calls`. Used to drive the
/// `phase5_adds_agent_columns_to_llm_calls` test.
const LEGACY_LLM_CALLS_PRE_PHASE5: &str = "
CREATE TABLE llm_calls (
    id                VARCHAR NOT NULL PRIMARY KEY,
    source_id         VARCHAR NOT NULL DEFAULT '',
    client_ip         VARCHAR NOT NULL,
    client_port       USMALLINT NOT NULL,
    server_ip         VARCHAR NOT NULL,
    server_port       USMALLINT NOT NULL,
    request_time      TIMESTAMP NOT NULL,
    response_time     TIMESTAMP,
    complete_time     TIMESTAMP,
    wire_api          VARCHAR NOT NULL,
    model             VARCHAR NOT NULL,
    api_type          VARCHAR NOT NULL,
    is_stream         BOOLEAN NOT NULL,
    request_path      VARCHAR NOT NULL,
    status_code       USMALLINT,
    finish_reason     VARCHAR,
    input_tokens      UINTEGER,
    output_tokens     UINTEGER,
    total_tokens      UINTEGER,
    cache_read_input_tokens   UINTEGER,
    cache_creation_input_tokens UINTEGER,
    ttft_ms           DOUBLE,
    e2e_latency_ms    DOUBLE,
    request_body      VARCHAR,
    response_body     VARCHAR,
    response_id       VARCHAR,
    request_headers   VARCHAR,
    response_headers  VARCHAR
);
";

/// Schema shape immediately before Phase 6 added `body_bytes_dropped` to
/// `llm_calls` — i.e. the full v0.4.0 layout (agent columns present) but
/// without the body-cap counter. Drives the
/// `phase6_adds_body_bytes_dropped_*` test.
const LEGACY_LLM_CALLS_PRE_PHASE6: &str = "
CREATE TABLE llm_calls (
    id                VARCHAR NOT NULL PRIMARY KEY,
    source_id         VARCHAR NOT NULL DEFAULT '',
    client_ip         VARCHAR NOT NULL,
    client_port       USMALLINT NOT NULL,
    server_ip         VARCHAR NOT NULL,
    server_port       USMALLINT NOT NULL,
    request_time      TIMESTAMP NOT NULL,
    response_time     TIMESTAMP,
    complete_time     TIMESTAMP,
    wire_api          VARCHAR NOT NULL,
    model             VARCHAR NOT NULL,
    api_type          VARCHAR NOT NULL,
    is_stream         BOOLEAN NOT NULL,
    request_path      VARCHAR NOT NULL,
    status_code       USMALLINT,
    finish_reason     VARCHAR,
    input_tokens      UINTEGER,
    output_tokens     UINTEGER,
    total_tokens      UINTEGER,
    cache_read_input_tokens   UINTEGER,
    cache_creation_input_tokens UINTEGER,
    ttft_ms           DOUBLE,
    e2e_latency_ms    DOUBLE,
    request_body      VARCHAR,
    response_body     VARCHAR,
    response_id       VARCHAR,
    request_headers   VARCHAR,
    response_headers  VARCHAR,
    is_agent_request  BOOLEAN  NOT NULL DEFAULT FALSE,
    tool_surface      VARCHAR,
    agent_topology    VARCHAR,
    tool_call_count   UINTEGER NOT NULL DEFAULT 0,
    tool_names_json   VARCHAR
);
";

/// Schema shape immediately before Phase 7 added the `process_*` attribution
/// columns to `llm_calls` — the full post-Phase-6 layout (body-cap counter
/// present) but without process pid/comm/exe. Drives the
/// `phase7_adds_process_columns_*` test.
const LEGACY_LLM_CALLS_PRE_PHASE7: &str = "
CREATE TABLE llm_calls (
    id                VARCHAR NOT NULL PRIMARY KEY,
    source_id         VARCHAR NOT NULL DEFAULT '',
    client_ip         VARCHAR NOT NULL,
    client_port       USMALLINT NOT NULL,
    server_ip         VARCHAR NOT NULL,
    server_port       USMALLINT NOT NULL,
    request_time      TIMESTAMP NOT NULL,
    response_time     TIMESTAMP,
    complete_time     TIMESTAMP,
    wire_api          VARCHAR NOT NULL,
    model             VARCHAR NOT NULL,
    api_type          VARCHAR NOT NULL,
    is_stream         BOOLEAN NOT NULL,
    request_path      VARCHAR NOT NULL,
    status_code       USMALLINT,
    finish_reason     VARCHAR,
    input_tokens      UINTEGER,
    output_tokens     UINTEGER,
    total_tokens      UINTEGER,
    cache_read_input_tokens   UINTEGER,
    cache_creation_input_tokens UINTEGER,
    ttft_ms           DOUBLE,
    e2e_latency_ms    DOUBLE,
    request_body      VARCHAR,
    response_body     VARCHAR,
    response_id       VARCHAR,
    request_headers   VARCHAR,
    response_headers  VARCHAR,
    is_agent_request  BOOLEAN  NOT NULL DEFAULT FALSE,
    tool_surface      VARCHAR,
    agent_topology    VARCHAR,
    tool_call_count   UINTEGER NOT NULL DEFAULT 0,
    tool_names_json   VARCHAR,
    body_bytes_dropped UBIGINT NOT NULL DEFAULT 0
);
";

/// `llm_metrics` shape that includes the Phase-4-dropped finish_*_count
/// columns. The init() migration must drop them.
const LEGACY_LLM_METRICS_WITH_FINISH_COUNTS: &str = "
CREATE TABLE llm_metrics (
    timestamp           TIMESTAMP NOT NULL,
    source_id           VARCHAR NOT NULL,
    granularity         VARCHAR NOT NULL,
    wire_api            VARCHAR NOT NULL,
    model               VARCHAR NOT NULL,
    server_ip           VARCHAR NOT NULL,
    finish_complete_count    UBIGINT NOT NULL DEFAULT 0,
    finish_length_count      UBIGINT NOT NULL DEFAULT 0,
    finish_tool_use_count    UBIGINT NOT NULL DEFAULT 0,
    finish_error_count       UBIGINT NOT NULL DEFAULT 0,
    finish_cancelled_count   UBIGINT NOT NULL DEFAULT 0
);
";

/// `agent_turns` shape with Phase-3 legacy status values still in rows.
const LEGACY_AGENT_TURNS_WITH_OLD_STATUS: &str = "
CREATE TABLE agent_turns (
    source_id          VARCHAR NOT NULL DEFAULT '',
    turn_id            VARCHAR NOT NULL PRIMARY KEY,
    session_id         VARCHAR NOT NULL,
    wire_api           VARCHAR NOT NULL,
    agent_kind         VARCHAR NOT NULL,
    client_ip          VARCHAR NOT NULL,
    server_ip          VARCHAR NOT NULL,
    start_time         TIMESTAMP NOT NULL,
    end_time           TIMESTAMP NOT NULL,
    duration_ms        UBIGINT NOT NULL,
    call_count         UBIGINT NOT NULL,
    models_used        VARCHAR,
    subagents_used     VARCHAR,
    total_input_tokens UBIGINT NOT NULL,
    total_output_tokens UBIGINT NOT NULL,
    total_cache_read_input_tokens   UBIGINT NOT NULL,
    total_cache_creation_input_tokens UBIGINT NOT NULL,
    total_cost_usd     DOUBLE,
    status             VARCHAR NOT NULL,
    final_finish_reason VARCHAR,
    user_input_preview VARCHAR,
    user_call_id       VARCHAR,
    final_answer_preview VARCHAR,
    final_call_id      VARCHAR,
    call_ids           VARCHAR,
    metadata           VARCHAR
);
";

fn synth_db(dir: &TempDir, ddl: &[&str]) -> std::path::PathBuf {
    let path = dir.path().join("legacy.duckdb");
    let conn = Connection::open(&path).expect("open synth db");
    for sql in ddl {
        conn.execute_batch(sql)
            .unwrap_or_else(|e| panic!("synth DDL failed: {e}\nsql: {sql}"));
    }
    drop(conn);
    path
}

fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT column_name FROM duckdb_columns() \
             WHERE table_name = '{table}' \
             ORDER BY column_index"
        ))
        .expect("prepare duckdb_columns");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query duckdb_columns");
    rows.filter_map(|r| r.ok()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase5_adds_nullable_agent_columns_to_llm_calls_on_legacy_db() {
    let tmp = TempDir::new().unwrap();
    let path = synth_db(&tmp, &[LEGACY_LLM_CALLS_PRE_PHASE5]);

    // Pre-condition: agent fields are absent.
    {
        let conn = Connection::open(&path).unwrap();
        let cols = column_names(&conn, "llm_calls");
        assert!(
            !cols.iter().any(|c| c == "tool_surface"),
            "synth pre-condition: agent fields must be absent in legacy DB, got cols: {cols:?}"
        );
    }

    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend.init().await.expect("init must reconcile legacy schema");

    let conn = Connection::open(&path).unwrap();
    // Post-init the table has been renamed `llm_calls` -> `spans` (Phase 8).
    let cols = column_names(&conn, "spans");
    // The three nullable Phase-5 columns can always be added via
    // `ALTER ADD COLUMN IF NOT EXISTS`. See FIXME below for the NOT NULL
    // columns DuckDB refuses to add to a populated/aged table.
    for expected in ["tool_surface", "agent_topology", "tool_names_json"] {
        assert!(
            cols.iter().any(|c| c == expected),
            "post-migration spans must contain {expected}, got: {cols:?}"
        );
    }
}

// Regression test for the Phase-5 NOT NULL ADD COLUMN gap: DuckDB
// (verified on 1.10501.0) refuses `ALTER TABLE ... ADD COLUMN ...
// NOT NULL DEFAULT ...`, so the original migration silently left
// `is_agent_request` / `tool_call_count` absent on upgraded DBs,
// breaking every subsequent INSERT. `schema::init` now adds these
// nullable-with-default (NOT NULL omitted on the ALTER path only), so
// the columns land on a legacy DB. This test asserts they do — keep it
// passing so the migration can't silently regress again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase5_adds_agent_not_null_columns_to_llm_calls_on_legacy_db() {
    let tmp = TempDir::new().unwrap();
    let path = synth_db(&tmp, &[LEGACY_LLM_CALLS_PRE_PHASE5]);

    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend.init().await.expect("init must reconcile legacy schema");

    let conn = Connection::open(&path).unwrap();
    let cols = column_names(&conn, "spans");
    for expected in ["is_agent_request", "tool_call_count"] {
        assert!(
            cols.iter().any(|c| c == expected),
            "post-migration spans must contain {expected}, got: {cols:?}"
        );
    }
}

// Phase-6 migration: `body_bytes_dropped` must be added to a legacy
// (pre-Phase-6) `llm_calls`, AND the positional appender must keep writing
// the right columns afterward. The ALTER appends the new column at the table
// tail; the CREATE TABLE also places it last, so a fresh-vs-migrated DB share
// one column order and the positional `append_row` stays aligned. This test
// proves alignment end-to-end by writing distinct sentinels into the two
// trailing NOT-NULL-with-default columns (`tool_call_count`,
// `body_bytes_dropped`) and reading both back — exactly the "PR#48 class"
// regression that a same-name-only assertion would miss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase6_adds_body_bytes_dropped_and_appender_stays_aligned_on_legacy_db() {
    let tmp = TempDir::new().unwrap();
    let path = synth_db(&tmp, &[LEGACY_LLM_CALLS_PRE_PHASE6]);

    // Pre-condition: the body-cap counter is absent on the legacy DB.
    {
        let conn = Connection::open(&path).unwrap();
        let cols = column_names(&conn, "llm_calls");
        assert!(
            !cols.iter().any(|c| c == "body_bytes_dropped"),
            "synth pre-condition: body_bytes_dropped must be absent in pre-Phase-6 DB, got: {cols:?}"
        );
    }

    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend.init().await.expect("init must reconcile legacy schema");

    // Column landed.
    {
        let conn = Connection::open(&path).unwrap();
        let cols = column_names(&conn, "spans");
        assert!(
            cols.iter().any(|c| c == "body_bytes_dropped"),
            "post-migration spans must contain body_bytes_dropped, got: {cols:?}"
        );
    }

    // Write a call through the real appender into the just-migrated table and
    // read both trailing columns back. Distinct sentinels (7 vs 4242) catch a
    // positional swap.
    backend
        .write_spans(vec![sample_call("call-phase6", 7, 4242)])
        .await
        .expect("write_spans must succeed against a migrated table");

    // Close the backend (checkpoints DuckDB to the file) before reading via a
    // fresh connection — a second live connection sees the pre-write snapshot.
    drop(backend);

    let conn = Connection::open(&path).unwrap();
    let (tool_call_count, body_bytes_dropped): (u32, u64) = conn
        .query_row(
            "SELECT tool_call_count, body_bytes_dropped FROM spans WHERE id = 'call-phase6'",
            [],
            |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u64>(1)?)),
        )
        .expect("row must be present after write_spans");
    assert_eq!(
        tool_call_count, 7,
        "tool_call_count must round-trip (appender column alignment)"
    );
    assert_eq!(
        body_bytes_dropped, 4242,
        "body_bytes_dropped must round-trip into the migrated column (appender alignment)"
    );
}

// Phase-7 migration: the `process_*` attribution columns must be added to a
// legacy (pre-Phase-7) `llm_calls`, AND the positional appender must keep
// writing the right columns afterward. Same alignment contract as Phase 6:
// the three columns append at the table tail in both CREATE and ALTER, so a
// fresh-vs-migrated DB share one column order. This writes a call carrying
// real process attribution and reads pid/comm/exe back to prove the eBPF
// attribution round-trips through the migrated columns intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase7_adds_process_columns_and_appender_stays_aligned_on_legacy_db() {
    let tmp = TempDir::new().unwrap();
    let path = synth_db(&tmp, &[LEGACY_LLM_CALLS_PRE_PHASE7]);

    // Pre-condition: the process columns are absent on the legacy DB.
    {
        let conn = Connection::open(&path).unwrap();
        let cols = column_names(&conn, "llm_calls");
        assert!(
            !cols.iter().any(|c| c == "process_pid"),
            "synth pre-condition: process_pid must be absent in pre-Phase-7 DB, got: {cols:?}"
        );
    }

    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend.init().await.expect("init must reconcile legacy schema");

    // Columns landed.
    {
        let conn = Connection::open(&path).unwrap();
        let cols = column_names(&conn, "spans");
        for col in ["process_pid", "process_comm", "process_exe"] {
            assert!(
                cols.iter().any(|c| c == col),
                "post-migration spans must contain {col}, got: {cols:?}"
            );
        }
    }

    // Write a call carrying eBPF process attribution through the real appender
    // into the just-migrated table. The trailing sentinel body_bytes_dropped
    // (4242) plus the process columns catch a positional swap among tail cols.
    let mut call = sample_call("call-phase7", 7, 4242);
    call.process = Some(ProcessInfo {
        pid: 31337,
        comm: "python3".into(),
        exe: Some("/usr/bin/python3.12".into()),
    });
    backend
        .write_spans(vec![call])
        .await
        .expect("write_spans must succeed against a migrated table");

    drop(backend);

    let conn = Connection::open(&path).unwrap();
    let (pid, comm, exe, body_bytes_dropped): (Option<u32>, Option<String>, Option<String>, u64) =
        conn.query_row(
            "SELECT process_pid, process_comm, process_exe, body_bytes_dropped \
             FROM spans WHERE id = 'call-phase7'",
            [],
            |r| {
                Ok((
                    r.get::<_, Option<u32>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, u64>(3)?,
                ))
            },
        )
        .expect("row must be present after write_spans");
    assert_eq!(pid, Some(31337), "process_pid must round-trip");
    assert_eq!(comm.as_deref(), Some("python3"), "process_comm must round-trip");
    assert_eq!(
        exe.as_deref(),
        Some("/usr/bin/python3.12"),
        "process_exe must round-trip"
    );
    assert_eq!(
        body_bytes_dropped, 4242,
        "body_bytes_dropped must stay aligned after the process columns append"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase4_drops_finish_count_columns_from_llm_metrics() {
    let tmp = TempDir::new().unwrap();
    let path = synth_db(&tmp, &[LEGACY_LLM_METRICS_WITH_FINISH_COUNTS]);

    // Pre-condition.
    {
        let conn = Connection::open(&path).unwrap();
        let cols = column_names(&conn, "llm_metrics");
        assert!(
            cols.iter().any(|c| c == "finish_complete_count"),
            "synth pre-condition: finish_*_count columns must be present"
        );
    }

    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend.init().await.expect("init must reconcile legacy schema");

    let conn = Connection::open(&path).unwrap();
    let cols = column_names(&conn, "llm_metrics");
    for dropped in [
        "finish_complete_count",
        "finish_length_count",
        "finish_tool_use_count",
        "finish_error_count",
        "finish_cancelled_count",
    ] {
        assert!(
            !cols.iter().any(|c| c == dropped),
            "post-migration llm_metrics must NOT contain {dropped}, got: {cols:?}"
        );
    }
    // The Phase-4 migration is strictly DROP COLUMN — it does not
    // backfill the canonical data columns that v0.3.0 expects (call_count,
    // ttft_*, etc.). A real legacy DB would have those from the start.
    // We deliberately do NOT smoke-test summary queries here because
    // the synth DDL above intentionally exercises just the drop path.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase3_rewrites_legacy_agent_turn_status_values() {
    let tmp = TempDir::new().unwrap();
    let path = synth_db(&tmp, &[LEGACY_AGENT_TURNS_WITH_OLD_STATUS]);

    // Seed legacy status values directly via raw SQL so the test does
    // not depend on any current Rust struct shape.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO agent_turns \
             (turn_id, session_id, wire_api, agent_kind, client_ip, server_ip, \
              start_time, end_time, duration_ms, call_count, \
              total_input_tokens, total_output_tokens, \
              total_cache_read_input_tokens, total_cache_creation_input_tokens, \
              status) VALUES \
             ('t-len',    's', 'openai-chat', 'test', '1.1.1.1', '2.2.2.2', NOW(), NOW(), 0, 0, 0, 0, 0, 0, 'length'),\
             ('t-fail',   's', 'openai-chat', 'test', '1.1.1.1', '2.2.2.2', NOW(), NOW(), 0, 0, 0, 0, 0, 0, 'failed'),\
             ('t-cancel', 's', 'openai-chat', 'test', '1.1.1.1', '2.2.2.2', NOW(), NOW(), 0, 0, 0, 0, 0, 0, 'cancelled'),\
             ('t-ok',     's', 'openai-chat', 'test', '1.1.1.1', '2.2.2.2', NOW(), NOW(), 0, 0, 0, 0, 0, 0, 'complete');"
        ).expect("seed legacy status rows");
    }

    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend.init().await.expect("init must reconcile legacy schema");

    // Inspect the raw status column post-migration.
    let conn = Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare("SELECT turn_id, status FROM traces ORDER BY turn_id")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    let map: std::collections::HashMap<_, _> = rows.into_iter().collect();
    assert_eq!(
        map.get("t-len").map(String::as_str),
        Some("complete"),
        "phase3: 'length' must rewrite to 'complete' (max_tokens is a wire terminal)"
    );
    assert_eq!(
        map.get("t-fail").map(String::as_str),
        Some("incomplete"),
        "phase3: 'failed' must rewrite to 'incomplete'"
    );
    assert_eq!(
        map.get("t-cancel").map(String::as_str),
        Some("incomplete"),
        "phase3: 'cancelled' must rewrite to 'incomplete'"
    );
    assert_eq!(
        map.get("t-ok").map(String::as_str),
        Some("complete"),
        "phase3: pre-existing canonical values must stay untouched"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_is_idempotent_against_canonical_schema() {
    // The "no-op" case: a freshly-init'd DB must accept a second init()
    // without error or drift. Catches accidental migration code that
    // mutates state on every boot (slow startup, lock contention).
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fresh.duckdb");
    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend.init().await.unwrap();

    let cols_before = {
        let conn = Connection::open(&path).unwrap();
        column_names(&conn, "spans")
    };

    backend.init().await.expect("re-init must succeed");

    let cols_after = {
        let conn = Connection::open(&path).unwrap();
        column_names(&conn, "spans")
    };
    assert_eq!(
        cols_before, cols_after,
        "second init() must not drift the spans column set"
    );

    // And the read path must still work.
    let _ = backend
        .query_traces(&TracesQuery {
            time_range: TimeRange {
                start_us: 0,
                end_us: i64::MAX,
            },
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
        .expect("query_traces must work after double-init");
}

/// Phase-8 OTel rename: a complete pre-rename database (legacy `llm_calls`
/// plus `agent_turns` carrying `call_ids`) must migrate in place to `spans`
/// and `traces` with `span_ids`, the new `kind` column backfilled to 'llm'
/// on existing rows, and zero row loss. This is the core guard for the
/// rename — a same-name-only check would miss content/row-loss regressions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase8_otel_rename_migrates_tables_columns_and_backfills_kind() {
    let tmp = TempDir::new().unwrap();
    let path = synth_db(
        &tmp,
        &[LEGACY_LLM_CALLS_PRE_PHASE7, LEGACY_AGENT_TURNS_WITH_OLD_STATUS],
    );

    // Seed one row in each legacy table. The turn carries a known span list so
    // we can assert the JSON survives the column rename byte-for-byte.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO llm_calls \
             (id, client_ip, client_port, server_ip, server_port, request_time, \
              wire_api, model, api_type, is_stream, request_path) VALUES \
             ('call-1', '1.1.1.1', 1, '2.2.2.2', 443, NOW(), \
              'openai-chat', 'gpt-test', 'chat', false, '/v1/chat/completions');",
        )
        .expect("seed legacy llm_calls row");
        conn.execute_batch(
            "INSERT INTO agent_turns \
             (turn_id, session_id, wire_api, agent_kind, client_ip, server_ip, \
              start_time, end_time, duration_ms, call_count, \
              total_input_tokens, total_output_tokens, \
              total_cache_read_input_tokens, total_cache_creation_input_tokens, \
              status, call_ids) VALUES \
             ('turn-1', 's', 'openai-chat', 'test', '1.1.1.1', '2.2.2.2', \
              NOW(), NOW(), 0, 1, 0, 0, 0, 0, 'complete', '[\"call-1\",\"call-2\"]');",
        )
        .expect("seed legacy agent_turns row");
    }

    let backend = Arc::new(
        DuckDbBackend::open_with_pool(path.to_str().unwrap(), 2).expect("open backend"),
    );
    backend
        .init()
        .await
        .expect("init must reconcile + rename legacy schema");
    drop(backend);

    let conn = Connection::open(&path).unwrap();

    // (1) Tables renamed: new names present, old names gone.
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT table_name FROM duckdb_tables() ORDER BY table_name")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert!(tables.iter().any(|t| t == "spans"), "spans table must exist, got: {tables:?}");
    assert!(tables.iter().any(|t| t == "traces"), "traces table must exist, got: {tables:?}");
    assert!(!tables.iter().any(|t| t == "llm_calls"), "old llm_calls must be gone, got: {tables:?}");
    assert!(!tables.iter().any(|t| t == "agent_turns"), "old agent_turns must be gone, got: {tables:?}");

    // (2) Column renamed call_ids -> span_ids, content preserved byte-for-byte.
    let trace_cols = column_names(&conn, "traces");
    assert!(trace_cols.iter().any(|c| c == "span_ids"), "traces must have span_ids, got: {trace_cols:?}");
    assert!(!trace_cols.iter().any(|c| c == "call_ids"), "traces must not have call_ids, got: {trace_cols:?}");
    let span_ids: String = conn
        .query_row("SELECT span_ids FROM traces WHERE turn_id = 'turn-1'", [], |r| r.get(0))
        .expect("migrated turn row must be present");
    assert_eq!(
        span_ids, "[\"call-1\",\"call-2\"]",
        "span_ids JSON must survive the rename byte-for-byte"
    );

    // (3) `kind` column added and backfilled to 'llm' on the pre-existing row.
    let span_cols = column_names(&conn, "spans");
    assert!(span_cols.iter().any(|c| c == "kind"), "spans must have kind, got: {span_cols:?}");
    assert!(span_cols.iter().any(|c| c == "attribution_label"), "spans must have attribution_label, got: {span_cols:?}");
    assert!(span_cols.iter().any(|c| c == "attribution_source"), "spans must have attribution_source, got: {span_cols:?}");
    assert!(span_cols.iter().any(|c| c == "attribution_confidence"), "spans must have attribution_confidence, got: {span_cols:?}");
    let kind: String = conn
        .query_row("SELECT kind FROM spans WHERE id = 'call-1'", [], |r| r.get(0))
        .expect("migrated span row must be present");
    assert_eq!(kind, "llm", "kind must backfill to 'llm' on existing rows");

    // (4) No row loss across the rename.
    let span_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM spans", [], |r| r.get(0))
        .unwrap();
    let trace_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(span_rows, 1, "the seeded span row must survive the rename");
    assert_eq!(trace_rows, 1, "the seeded trace row must survive the rename");
}
