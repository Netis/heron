//! DDL constants for the DuckDB-backed schema, plus the `init()` bootstrap
//! that creates every table and runs forward-compatible migrations.

use tracing::info;
use ts_common::error::{AppError, Result};

use crate::DuckDbBackend;

const CREATE_LLM_CALLS: &str = "
CREATE TABLE IF NOT EXISTS llm_calls (
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

const CREATE_LLM_METRICS: &str = "
CREATE TABLE IF NOT EXISTS llm_metrics (
    timestamp           TIMESTAMP NOT NULL,
    source_id           VARCHAR NOT NULL,
    granularity         VARCHAR NOT NULL,
    wire_api            VARCHAR NOT NULL,
    model               VARCHAR NOT NULL,
    server_ip           VARCHAR NOT NULL,
    call_count       UBIGINT NOT NULL,
    stream_count        UBIGINT NOT NULL,
    non_stream_count    UBIGINT NOT NULL,
    active_calls_sum          UBIGINT NOT NULL,
    active_calls_sample_count UBIGINT NOT NULL,
    active_calls_max          UINTEGER NOT NULL,
    total_input_tokens  UBIGINT NOT NULL,
    input_token_count   UBIGINT NOT NULL,
    total_output_tokens UBIGINT NOT NULL,
    output_token_count  UBIGINT NOT NULL,
    total_cache_read_input_tokens    UBIGINT NOT NULL,
    total_cache_creation_input_tokens UBIGINT NOT NULL,
    error_count         UBIGINT NOT NULL,
    error_4xx_count     UBIGINT NOT NULL,
    error_429_count     UBIGINT NOT NULL,
    error_5xx_count     UBIGINT NOT NULL,
    ttft_sum            DOUBLE NOT NULL,
    ttft_count          UBIGINT NOT NULL,
    ttft_p50            DOUBLE,
    ttft_p95            DOUBLE,
    ttft_p99            DOUBLE,
    ttft_stream_sum     DOUBLE NOT NULL DEFAULT 0,
    ttft_stream_count   UBIGINT NOT NULL DEFAULT 0,
    ttft_stream_p50     DOUBLE,
    ttft_stream_p95     DOUBLE,
    ttft_stream_p99     DOUBLE,
    ttft_nonstream_sum   DOUBLE NOT NULL DEFAULT 0,
    ttft_nonstream_count UBIGINT NOT NULL DEFAULT 0,
    ttft_nonstream_p50   DOUBLE,
    ttft_nonstream_p95   DOUBLE,
    ttft_nonstream_p99   DOUBLE,
    e2e_sum             DOUBLE NOT NULL,
    e2e_count           UBIGINT NOT NULL,
    e2e_p50             DOUBLE,
    e2e_p95             DOUBLE,
    e2e_p99             DOUBLE,
    tpot_sum            DOUBLE NOT NULL,
    tpot_count          UBIGINT NOT NULL,
    tpot_p50            DOUBLE,
    tpot_p95            DOUBLE,
    tpot_p99            DOUBLE
);
";

const CREATE_LLM_FINISH_METRICS: &str = "
CREATE TABLE IF NOT EXISTS llm_finish_metrics (
    timestamp     TIMESTAMP NOT NULL,
    source_id     VARCHAR NOT NULL,
    granularity   VARCHAR NOT NULL,
    wire_api      VARCHAR NOT NULL,
    model         VARCHAR NOT NULL,
    server_ip     VARCHAR NOT NULL,
    finish_reason VARCHAR NOT NULL,
    count         UBIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_llm_finish_metrics_ts
    ON llm_finish_metrics (timestamp, granularity);
";

const CREATE_LLM_TURNS: &str = "
CREATE TABLE IF NOT EXISTS agent_turns (
    turn_id                   VARCHAR NOT NULL PRIMARY KEY,
    source_id                 VARCHAR NOT NULL DEFAULT '',
    session_id                VARCHAR NOT NULL,
    wire_api                  VARCHAR NOT NULL,
    agent_kind               VARCHAR NOT NULL,
    client_ip                 VARCHAR NOT NULL,
    server_ip                 VARCHAR NOT NULL,
    start_time                TIMESTAMP NOT NULL,
    end_time                  TIMESTAMP NOT NULL,
    duration_ms               UBIGINT NOT NULL,
    call_count                UINTEGER NOT NULL,
    models_used               VARCHAR,
    subagents_used            VARCHAR,
    total_input_tokens        UBIGINT NOT NULL,
    total_output_tokens       UBIGINT NOT NULL,
    total_cache_read_input_tokens UBIGINT NOT NULL,
    total_cache_creation_input_tokens UBIGINT NOT NULL,
    total_cost_usd            DOUBLE,
    status                    VARCHAR NOT NULL,
    final_finish_reason       VARCHAR,
    user_input_preview        VARCHAR,
    user_call_id              VARCHAR,
    final_answer_preview      VARCHAR,
    final_call_id             VARCHAR,
    call_ids                  JSON NOT NULL,
    metadata                  VARCHAR
);
";

const CREATE_HTTP_EXCHANGES: &str = "
CREATE TABLE IF NOT EXISTS http_exchanges (
    id                        VARCHAR NOT NULL PRIMARY KEY,
    source_id                 VARCHAR NOT NULL DEFAULT '',
    client_ip                 VARCHAR NOT NULL,
    client_port               USMALLINT NOT NULL,
    server_ip                 VARCHAR NOT NULL,
    server_port               USMALLINT NOT NULL,
    method                    VARCHAR NOT NULL,
    uri                       VARCHAR NOT NULL,
    request_headers           VARCHAR NOT NULL,
    request_body              BLOB,
    status                    USMALLINT,
    response_headers          VARCHAR NOT NULL,
    response_body             BLOB,
    is_sse                    BOOLEAN NOT NULL,
    sse_event_count           UINTEGER NOT NULL DEFAULT 0,
    sse_data_bytes            UBIGINT NOT NULL DEFAULT 0,
    request_time              TIMESTAMP NOT NULL,
    response_first_byte_time  TIMESTAMP,
    response_complete_time    TIMESTAMP
);
";

pub(crate) async fn init(backend: &DuckDbBackend) -> Result<()> {
    // Any writer works — they share the same DuckDB instance. Using the
    // calls writer keeps init deterministic.
    let conn = backend.write_calls_conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
        conn.execute_batch(CREATE_LLM_CALLS)
            .map_err(|e| AppError::Storage(format!("failed to create llm_calls: {e}")))?;
        conn.execute_batch(CREATE_LLM_METRICS)
            .map_err(|e| AppError::Storage(format!("failed to create llm_metrics: {e}")))?;
        conn.execute_batch(CREATE_LLM_FINISH_METRICS).map_err(|e| {
            AppError::Storage(format!("failed to create llm_finish_metrics: {e}"))
        })?;
        conn.execute_batch(CREATE_LLM_TURNS)
            .map_err(|e| AppError::Storage(format!("failed to create agent_turns: {e}")))?;
        conn.execute_batch(CREATE_HTTP_EXCHANGES)
            .map_err(|e| AppError::Storage(format!("failed to create http_exchanges: {e}")))?;

        // Phase 4 migration: drop the legacy finish_*_count columns from
        // llm_metrics on databases created before this change. DuckDB
        // accepts `DROP COLUMN IF EXISTS` (added in 0.7.0), so each ALTER
        // is a no-op on a fresh schema. Run each statement on its own so
        // a failure on one column does not abort the rest, and log the
        // outcome instead of swallowing it silently.
        //
        // Also: ttft_stream_* / ttft_nonstream_* columns added later.
        // `ADD COLUMN IF NOT EXISTS` is also a no-op on fresh schemas.
        for stmt in [
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_complete_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_length_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_tool_use_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_error_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_cancelled_count;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_stream_sum DOUBLE NOT NULL DEFAULT 0;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_stream_count UBIGINT NOT NULL DEFAULT 0;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_stream_p50 DOUBLE;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_stream_p95 DOUBLE;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_stream_p99 DOUBLE;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_nonstream_sum DOUBLE NOT NULL DEFAULT 0;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_nonstream_count UBIGINT NOT NULL DEFAULT 0;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_nonstream_p50 DOUBLE;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_nonstream_p95 DOUBLE;",
            "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS ttft_nonstream_p99 DOUBLE;",
        ] {
            match conn.execute_batch(stmt) {
                Ok(()) => tracing::debug!(
                    sql = stmt,
                    "phase4 migration: llm_metrics finish_* column dropped (or absent)"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    sql = stmt,
                    "phase4 migration: drop finish_* column failed (non-fatal — fresh DB or unsupported DuckDB version)"
                ),
            }
        }

        // Phase 3 collapsed TurnStatus to Complete | Incomplete. Migrate
        // legacy values:
        //   'length'             -> 'complete'   (max_tokens IS a wire terminal)
        //   'failed'/'cancelled' -> 'incomplete' (no wire terminal landed)
        // Rich provider state (e.g. 'max_tokens', 'refusal') already
        // lives in agent_turns.final_finish_reason, so no information
        // loss beyond the status-axis collapse.
        match conn.execute_batch(
            "UPDATE agent_turns SET status='complete'   WHERE status = 'length';\n\
             UPDATE agent_turns SET status='incomplete' WHERE status IN ('failed', 'cancelled');",
        ) {
            Ok(()) => tracing::debug!(
                "phase3 migration: agent_turns.status legacy values rewritten (or absent)"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "phase3 migration: agent_turns.status rewrite failed (non-fatal — fresh DB or unsupported DuckDB version)"
            ),
        }

        info!("storage tables initialized");
        Ok(())
    })
    .await
    .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use ts_storage::StorageBackend;

    fn in_memory() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
    }

    #[tokio::test]
    async fn test_init_creates_tables() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM llm_calls").unwrap();
        let count: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);

        let mut stmt = conn.prepare("SELECT COUNT(*) FROM llm_metrics").unwrap();
        let count: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_init_is_idempotent() {
        let backend = in_memory();
        backend.init().await.unwrap();
        backend.init().await.unwrap();
    }

}
