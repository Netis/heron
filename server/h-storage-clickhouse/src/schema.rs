//! ClickHouse DDL + `init()` bootstrap. Mirrors the DuckDB schema column-for-
//! column (same names, same order) so the two backends stay diffable and the
//! shared `h_storage::query` row shapes map identically.
//!
//! Engine choices (see `docs/design/07-schema.md`):
//!   * `llm_calls` / `http_exchanges` — append-only `MergeTree`,
//!     `ORDER BY (request_time, id)`. `id` is unique per row; reads that need a
//!     single row already `LIMIT 1`.
//!   * `llm_metrics` / `llm_finish_metrics` — append-only `MergeTree`; rollup
//!     rows are SUM/MAX-aggregated at read, one row per bucket from the
//!     aggregator, so no dedup is needed.
//!   * `agent_turns` — `ReplacingMergeTree(_version)`: it is the only mutated
//!     table (`update_turn_metadata`), so reads use `FINAL` and updates
//!     re-insert the whole row with a higher `_version`.
//!
//! Timestamps are `DateTime64(6, 'UTC')`; the `clickhouse` crate maps a Rust
//! `i64` field directly to the underlying microsecond ticks, matching the
//! domain models (`LlmCall.request_time: i64` micros) with no chrono/time dep.
//!
//! Retention is NOT declarative TTL here — it runs through `apply_retention`
//! (lightweight `DELETE`) exactly like the DuckDB backend, driven by the shared
//! `spawn_retention_task`. `init()` therefore takes no config.

use tracing::info;

use h_common::error::Result;

use crate::client::ch_err;
use crate::ClickHouseBackend;

const CREATE_LLM_CALLS: &str = "
CREATE TABLE IF NOT EXISTS llm_calls (
    id                String,
    source_id         String DEFAULT '',
    client_ip         String,
    client_port       UInt16,
    server_ip         String,
    server_port       UInt16,
    request_time      DateTime64(6, 'UTC'),
    response_time     Nullable(DateTime64(6, 'UTC')),
    complete_time     Nullable(DateTime64(6, 'UTC')),
    wire_api          String,
    model             String,
    api_type          String,
    is_stream         Bool,
    request_path      String,
    status_code       Nullable(UInt16),
    finish_reason     Nullable(String),
    input_tokens      Nullable(UInt32),
    output_tokens     Nullable(UInt32),
    total_tokens      Nullable(UInt32),
    cache_read_input_tokens     Nullable(UInt32),
    cache_creation_input_tokens Nullable(UInt32),
    ttft_ms           Nullable(Float64),
    e2e_latency_ms    Nullable(Float64),
    request_body      Nullable(String),
    response_body     Nullable(String),
    response_id       Nullable(String),
    request_headers   String,
    response_headers  String,
    is_agent_request  Bool DEFAULT false,
    tool_surface      Nullable(String),
    agent_topology    Nullable(String),
    tool_call_count   UInt32 DEFAULT 0,
    tool_names_json   Nullable(String),
    body_bytes_dropped UInt64 DEFAULT 0
) ENGINE = MergeTree ORDER BY (request_time, id)
";

const CREATE_LLM_METRICS: &str = "
CREATE TABLE IF NOT EXISTS llm_metrics (
    timestamp           DateTime64(6, 'UTC'),
    source_id           String,
    granularity         String,
    wire_api            String,
    model               String,
    server_ip           String,
    call_count          UInt64,
    stream_count        UInt64,
    non_stream_count    UInt64,
    active_calls_sum          UInt64,
    active_calls_sample_count UInt64,
    active_calls_max          UInt32,
    total_input_tokens  UInt64,
    input_token_count   UInt64,
    total_output_tokens UInt64,
    output_token_count  UInt64,
    total_cache_read_input_tokens     UInt64,
    total_cache_creation_input_tokens UInt64,
    error_count         UInt64,
    error_4xx_count     UInt64,
    error_429_count     UInt64,
    error_5xx_count     UInt64,
    ttft_sum            Float64,
    ttft_count          UInt64,
    ttft_p50            Nullable(Float64),
    ttft_p95            Nullable(Float64),
    ttft_p99            Nullable(Float64),
    ttft_stream_sum     Float64 DEFAULT 0,
    ttft_stream_count   UInt64 DEFAULT 0,
    ttft_stream_p50     Nullable(Float64),
    ttft_stream_p95     Nullable(Float64),
    ttft_stream_p99     Nullable(Float64),
    ttft_nonstream_sum   Float64 DEFAULT 0,
    ttft_nonstream_count UInt64 DEFAULT 0,
    ttft_nonstream_p50   Nullable(Float64),
    ttft_nonstream_p95   Nullable(Float64),
    ttft_nonstream_p99   Nullable(Float64),
    e2e_sum             Float64,
    e2e_count           UInt64,
    e2e_p50             Nullable(Float64),
    e2e_p95             Nullable(Float64),
    e2e_p99             Nullable(Float64),
    tpot_sum            Float64,
    tpot_count          UInt64,
    tpot_p50            Nullable(Float64),
    tpot_p95            Nullable(Float64),
    tpot_p99            Nullable(Float64),
    tool_surface        Nullable(String)
) ENGINE = MergeTree ORDER BY (granularity, timestamp, wire_api, model, server_ip)
";

const CREATE_LLM_FINISH_METRICS: &str = "
CREATE TABLE IF NOT EXISTS llm_finish_metrics (
    timestamp     DateTime64(6, 'UTC'),
    source_id     String,
    granularity   String,
    wire_api      String,
    model         String,
    server_ip     String,
    finish_reason String,
    count         UInt64
) ENGINE = MergeTree
ORDER BY (granularity, timestamp, finish_reason, wire_api, model, server_ip)
";

const CREATE_AGENT_TURNS: &str = "
CREATE TABLE IF NOT EXISTS agent_turns (
    turn_id                   String,
    source_id                 String DEFAULT '',
    session_id                String,
    wire_api                  String,
    agent_kind                String,
    client_ip                 String,
    server_ip                 String,
    start_time                DateTime64(6, 'UTC'),
    end_time                  DateTime64(6, 'UTC'),
    duration_ms               UInt64,
    call_count                UInt32,
    models_used               Nullable(String),
    subagents_used            Nullable(String),
    total_input_tokens        UInt64,
    total_output_tokens       UInt64,
    total_cache_read_input_tokens     UInt64,
    total_cache_creation_input_tokens UInt64,
    total_cost_usd            Nullable(Float64),
    status                    String,
    final_finish_reason       Nullable(String),
    user_input_preview        Nullable(String),
    user_call_id              Nullable(String),
    final_answer_preview      Nullable(String),
    final_call_id             Nullable(String),
    call_ids                  String,
    metadata                  Nullable(String),
    tool_surfaces_json        Nullable(String),
    tool_call_total           UInt32 DEFAULT 0,
    agent_topology            Nullable(String),
    suspicious_skills_json    Nullable(String),
    _version                  UInt64
) ENGINE = ReplacingMergeTree(_version) ORDER BY turn_id
";

const CREATE_HTTP_EXCHANGES: &str = "
CREATE TABLE IF NOT EXISTS http_exchanges (
    id                        String,
    source_id                 String DEFAULT '',
    client_ip                 String,
    client_port               UInt16,
    server_ip                 String,
    server_port               UInt16,
    method                    String,
    uri                       String,
    request_headers           String,
    request_body              Nullable(String),
    status                    Nullable(UInt16),
    response_headers          String,
    response_body             Nullable(String),
    is_sse                    Bool,
    sse_event_count           UInt32 DEFAULT 0,
    sse_data_bytes            UInt64 DEFAULT 0,
    request_time              DateTime64(6, 'UTC'),
    response_first_byte_time  Nullable(DateTime64(6, 'UTC')),
    response_complete_time    Nullable(DateTime64(6, 'UTC'))
) ENGINE = MergeTree ORDER BY (request_time, id)
";

const CREATE_TABLES: &[&str] = &[
    CREATE_LLM_CALLS,
    CREATE_LLM_METRICS,
    CREATE_LLM_FINISH_METRICS,
    CREATE_AGENT_TURNS,
    CREATE_HTTP_EXCHANGES,
];

pub(crate) async fn init(backend: &ClickHouseBackend) -> Result<()> {
    // Step 1: create the database via a server-scoped client (a db-scoped
    // connection errors UNKNOWN_DATABASE until the database exists).
    let create_db = format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        quote_ident(&backend.database)
    );
    backend
        .admin_client()
        .query(&create_db)
        .execute()
        .await
        .map_err(|e| ch_err("create database", e))?;

    // Step 2: create every table on the db-scoped client. Idempotent.
    for ddl in CREATE_TABLES {
        backend.exec(ddl).await?;
    }

    info!(database = %backend.database, "clickhouse storage tables initialized");
    Ok(())
}

/// Backtick-quote a ClickHouse identifier, doubling embedded backticks.
/// Database names come from config (operator-controlled), but quoting keeps
/// names with hyphens / reserved words valid.
fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}
