//! DDL constants for the DuckDB-backed schema, plus the `init()` bootstrap
//! that creates every table and runs forward-compatible migrations.

use tracing::info;
use h_common::error::{AppError, Result};

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
    response_headers  VARCHAR,
    is_agent_request  BOOLEAN  NOT NULL DEFAULT FALSE,
    tool_surface      VARCHAR,
    agent_topology    VARCHAR,
    tool_call_count   UINTEGER NOT NULL DEFAULT 0,
    tool_names_json   VARCHAR,
    body_bytes_dropped UBIGINT NOT NULL DEFAULT 0
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
    tpot_p99            DOUBLE,
    tool_surface        VARCHAR
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
    metadata                  VARCHAR,
    tool_surfaces_json        VARCHAR,
    tool_call_total           UINTEGER NOT NULL DEFAULT 0,
    agent_topology            VARCHAR,
    suspicious_skills_json    VARCHAR
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
        for stmt in [
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_complete_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_length_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_tool_use_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_error_count;",
            "ALTER TABLE llm_metrics DROP COLUMN IF EXISTS finish_cancelled_count;",
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

        // ttft_stream_* / ttft_nonstream_* migration: the appender writes
        // values by POSITION, not name. `ALTER TABLE ADD COLUMN` always
        // appends to the end; that puts the new columns AFTER e2e_* and
        // tpot_*, while the Rust struct (and CREATE TABLE) places them
        // immediately after `ttft_p99`. So positional writes from new
        // code into an ALTER-migrated table corrupt e2e + tpot columns.
        //
        // Detection: look at column order via duckdb_columns(). If
        // `ttft_stream_sum` does not sit directly after `ttft_p99`, the
        // table is in the broken legacy layout — rebuild it with the
        // canonical order. Old rollup data is lost (llm_calls is intact,
        // and the rollup repopulates from new traffic); the alternative
        // is a stat-by-stat preserve-and-rewrite which is more invasive
        // and not worth the few hours of corrupted rollups it would save.
        if needs_canonical_rebuild(&conn) {
            tracing::warn!(
                "llm_metrics columns out of canonical order \
                 (ALTER ADD COLUMN appended at end); rebuilding table"
            );
            for stmt in [
                "DROP TABLE IF EXISTS llm_metrics;",
                CREATE_LLM_METRICS,
            ] {
                conn.execute_batch(stmt).map_err(|e| {
                    AppError::Storage(format!(
                        "llm_metrics canonical rebuild failed: {e} (sql: {stmt})"
                    ))
                })?;
            }
        }

        // Back-fill rollups from the still-present llm_calls table if
        // llm_metrics is empty but llm_calls has rows. Catches: (1)
        // operators who restored a calls.duckdb without the rollup, and
        // (2) the post-rebuild state above where we just dropped the
        // table. TPOT is reconstructed per-call (complete_time -
        // response_time) / output_tokens; active_calls_* can't be
        // reconstructed (live concurrency sampling) and stays 0.
        if rollup_empty_but_calls_present(&conn) {
            tracing::info!(
                "llm_metrics is empty but llm_calls has rows — back-filling rollup history"
            );
            for granularity in [
                ("10s", "INTERVAL '10 seconds'"),
                ("1m", "INTERVAL '1 minute'"),
                ("5m", "INTERVAL '5 minutes'"),
                ("1h", "INTERVAL '1 hour'"),
            ] {
                let sql = backfill_sql(granularity.0, granularity.1);
                match conn.execute_batch(&sql) {
                    Ok(()) => tracing::info!(
                        granularity = granularity.0,
                        "llm_metrics back-fill complete"
                    ),
                    Err(e) => tracing::error!(
                        error = %e,
                        granularity = granularity.0,
                        "llm_metrics back-fill failed (non-fatal — new traffic still repopulates)"
                    ),
                }
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

        // Phase 5 migration: add agent-classification columns to llm_calls.
        // Each statement is run independently so a failure on one column
        // does not abort the rest; failures are logged (not propagated) so
        // the app can start even against an older DuckDB that doesn't support
        // `ADD COLUMN IF NOT EXISTS`.
        //
        // NOTE: the agent columns are added WITHOUT `NOT NULL` here, even
        // though the canonical CREATE TABLE declares them NOT NULL. DuckDB
        // refuses `ALTER TABLE ... ADD COLUMN ... NOT NULL DEFAULT ...`
        // (verified on 1.10501.0), so a NOT NULL form silently fails and
        // leaves the column absent — which then breaks every subsequent
        // INSERT on an upgraded DB. Adding them nullable-with-default
        // succeeds, is a metadata-only operation (instant even on a
        // multi-GB table), and is behaviorally equivalent: the writer
        // always supplies a value, and these columns sit at the end of
        // the table in both CREATE and ALTER so the appender's positional
        // writes stay aligned. Fresh installs still get NOT NULL via
        // CREATE TABLE.
        let agent_columns = [
            "ALTER TABLE llm_calls ADD COLUMN IF NOT EXISTS is_agent_request BOOLEAN DEFAULT FALSE;",
            "ALTER TABLE llm_calls ADD COLUMN IF NOT EXISTS tool_surface VARCHAR;",
            "ALTER TABLE llm_calls ADD COLUMN IF NOT EXISTS agent_topology VARCHAR;",
            "ALTER TABLE llm_calls ADD COLUMN IF NOT EXISTS tool_call_count UINTEGER DEFAULT 0;",
            "ALTER TABLE llm_calls ADD COLUMN IF NOT EXISTS tool_names_json VARCHAR;",
        ];
        for stmt in agent_columns {
            match conn.execute_batch(stmt) {
                Ok(()) => tracing::debug!(
                    sql = stmt,
                    "phase5 migration: llm_calls agent column added (or already present)"
                ),
                Err(e) => tracing::info!(
                    "phase5 migration: llm_calls agent columns add skipped: {e} (sql: {stmt})"
                ),
            }
        }

        // Phase 6 migration: add `body_bytes_dropped` to llm_calls — the
        // count of request+response body bytes elided by the stored-body cap
        // (`BodyCapConfig`). Added WITHOUT `NOT NULL` for the exact reason
        // documented on the Phase 5 block above (DuckDB rejects
        // `ALTER ... ADD COLUMN ... NOT NULL DEFAULT`, verified on 1.10501.0).
        // Nullable-with-default is metadata-only and behaviorally equivalent:
        // the writer always supplies a value, and the column sits at the tail
        // of the table in both CREATE and ALTER so the appender's positional
        // writes stay aligned. Fresh installs get NOT NULL via CREATE TABLE.
        match conn.execute_batch(
            "ALTER TABLE llm_calls ADD COLUMN IF NOT EXISTS body_bytes_dropped UBIGINT DEFAULT 0;",
        ) {
            Ok(()) => tracing::debug!(
                "phase6 migration: llm_calls.body_bytes_dropped added (or already present)"
            ),
            Err(e) => {
                tracing::info!("phase6 migration: llm_calls.body_bytes_dropped add skipped: {e}")
            }
        }

        // Phase 5 migration (agent_turns): add agent rollup columns to
        // agent_turns. Same pattern as the llm_calls block above — each
        // statement runs independently so a failure on one column does not
        // abort the rest.
        // Same NOT NULL-omission rationale as the llm_calls block above.
        let turn_agent_columns = [
            "ALTER TABLE agent_turns ADD COLUMN IF NOT EXISTS tool_surfaces_json VARCHAR;",
            "ALTER TABLE agent_turns ADD COLUMN IF NOT EXISTS tool_call_total UINTEGER DEFAULT 0;",
            "ALTER TABLE agent_turns ADD COLUMN IF NOT EXISTS agent_topology VARCHAR;",
            "ALTER TABLE agent_turns ADD COLUMN IF NOT EXISTS suspicious_skills_json VARCHAR;",
        ];
        for stmt in turn_agent_columns {
            match conn.execute_batch(stmt) {
                Ok(()) => tracing::debug!(
                    sql = stmt,
                    "phase5 migration: agent_turns rollup column added (or absent)"
                ),
                Err(e) => tracing::info!(
                    "phase5 migration: agent_turns rollup column add skipped: {e} (sql: {stmt})"
                ),
            }
        }

        // Phase 5 migration (llm_metrics): add tool_surface dimension column.
        let metrics_dim = "ALTER TABLE llm_metrics ADD COLUMN IF NOT EXISTS tool_surface VARCHAR;";
        match conn.execute_batch(metrics_dim) {
            Ok(()) => tracing::debug!(
                sql = metrics_dim,
                "phase5 migration: llm_metrics tool_surface column added (or already present)"
            ),
            Err(e) => tracing::info!(
                "phase5 migration: llm_metrics tool_surface column add skipped: {e}"
            ),
        }

        info!("storage tables initialized");
        Ok(())
    })
    .await
    .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
}

/// True when llm_metrics has 0 rows but llm_calls has at least 1 row.
/// Indicates either a fresh post-rebuild state or an operator-restored
/// calls.duckdb that lost the rollup; either way, back-filling makes
/// the dashboards work.
fn rollup_empty_but_calls_present(conn: &duckdb::Connection) -> bool {
    let metric_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM llm_metrics", [], |r| r.get(0))
        .unwrap_or(-1);
    if metric_rows != 0 {
        return false;
    }
    let call_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM llm_calls", [], |r| r.get(0))
        .unwrap_or(0);
    call_rows > 0
}

/// Build the SQL that re-aggregates llm_calls into llm_metrics at one
/// granularity. Mirrors `WindowBucket::on_call_complete`. One
/// simplification:
///
/// * `active_calls_*` come out as 0 — concurrency is sampled live and
///   isn't reconstructible from finished calls.
///
/// TPOT is computed per-call from `(complete_time - response_time) /
/// output_tokens` (streaming responses only), matching the live
/// aggregator. Percentiles use DuckDB's `approx_quantile` (t-digest-like)
/// rather than our streaming digest; ~1-2% off on tails but fine for
/// chart rendering.
fn backfill_sql(granularity_label: &str, time_bucket_interval: &str) -> String {
    // The live aggregator emits four tiered dimension rows per bucket
    // (see `dimension_keys()` in h-metrics::aggregator): the specific
    // (wire, model, server_ip) tuple plus wildcard ('*') roll-ups so the
    // dashboard's default queries (no filter → `wire_api = '*' AND
    // model = '*' AND server_ip = '*'`) hit a single pre-aggregated row.
    // Backfill must produce the same shape via GROUPING SETS, with
    // COALESCE turning NULL group-keys into the '*' wildcard the
    // dim-where clauses look for.
    format!(
        "INSERT INTO llm_metrics
        SELECT
            time_bucket({time_bucket_interval}, request_time) AS timestamp,
            source_id,
            '{granularity_label}' AS granularity,
            COALESCE(wire_api,  '*') AS wire_api,
            COALESCE(model,     '*') AS model,
            COALESCE(server_ip, '*') AS server_ip,
            COUNT(*) AS call_count,
            CAST(SUM(CASE WHEN is_stream THEN 1 ELSE 0 END) AS UBIGINT) AS stream_count,
            CAST(SUM(CASE WHEN NOT is_stream THEN 1 ELSE 0 END) AS UBIGINT) AS non_stream_count,
            0 AS active_calls_sum,
            0 AS active_calls_sample_count,
            0 AS active_calls_max,
            CAST(COALESCE(SUM(input_tokens), 0) AS UBIGINT) AS total_input_tokens,
            CAST(COUNT(input_tokens) AS UBIGINT) AS input_token_count,
            CAST(COALESCE(SUM(output_tokens), 0) AS UBIGINT) AS total_output_tokens,
            CAST(COUNT(output_tokens) AS UBIGINT) AS output_token_count,
            CAST(COALESCE(SUM(cache_read_input_tokens), 0) AS UBIGINT) AS total_cache_read_input_tokens,
            CAST(COALESCE(SUM(cache_creation_input_tokens), 0) AS UBIGINT) AS total_cache_creation_input_tokens,
            CAST(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS UBIGINT) AS error_count,
            CAST(SUM(CASE WHEN status_code BETWEEN 400 AND 499 THEN 1 ELSE 0 END) AS UBIGINT) AS error_4xx_count,
            CAST(SUM(CASE WHEN status_code = 429 THEN 1 ELSE 0 END) AS UBIGINT) AS error_429_count,
            CAST(SUM(CASE WHEN status_code >= 500 THEN 1 ELSE 0 END) AS UBIGINT) AS error_5xx_count,
            COALESCE(SUM(ttft_ms), 0) AS ttft_sum,
            CAST(COUNT(ttft_ms) AS UBIGINT) AS ttft_count,
            approx_quantile(ttft_ms, 0.5),
            approx_quantile(ttft_ms, 0.95),
            approx_quantile(ttft_ms, 0.99),
            COALESCE(SUM(CASE WHEN is_stream THEN ttft_ms END), 0) AS ttft_stream_sum,
            CAST(COUNT(CASE WHEN is_stream THEN ttft_ms END) AS UBIGINT) AS ttft_stream_count,
            approx_quantile(CASE WHEN is_stream THEN ttft_ms END, 0.5),
            approx_quantile(CASE WHEN is_stream THEN ttft_ms END, 0.95),
            approx_quantile(CASE WHEN is_stream THEN ttft_ms END, 0.99),
            COALESCE(SUM(CASE WHEN NOT is_stream THEN ttft_ms END), 0) AS ttft_nonstream_sum,
            CAST(COUNT(CASE WHEN NOT is_stream THEN ttft_ms END) AS UBIGINT) AS ttft_nonstream_count,
            approx_quantile(CASE WHEN NOT is_stream THEN ttft_ms END, 0.5),
            approx_quantile(CASE WHEN NOT is_stream THEN ttft_ms END, 0.95),
            approx_quantile(CASE WHEN NOT is_stream THEN ttft_ms END, 0.99),
            COALESCE(SUM(e2e_latency_ms), 0) AS e2e_sum,
            CAST(COUNT(e2e_latency_ms) AS UBIGINT) AS e2e_count,
            approx_quantile(e2e_latency_ms, 0.5),
            approx_quantile(e2e_latency_ms, 0.95),
            approx_quantile(e2e_latency_ms, 0.99),
            -- tpot = (complete - response) ms per output token, streaming only.
            -- Mirrors WindowBucket::on_call_complete. NULLIF guards against
            -- divide-by-zero on calls with output_tokens=0 (rare keep-alive).
            COALESCE(SUM(CASE WHEN is_stream
                AND response_time IS NOT NULL AND complete_time IS NOT NULL
                AND output_tokens IS NOT NULL AND output_tokens > 0
                THEN (EPOCH_US(complete_time) - EPOCH_US(response_time))
                     / 1000.0 / output_tokens END), 0) AS tpot_sum,
            CAST(COUNT(CASE WHEN is_stream
                AND response_time IS NOT NULL AND complete_time IS NOT NULL
                AND output_tokens IS NOT NULL AND output_tokens > 0
                THEN 1 END) AS UBIGINT) AS tpot_count,
            approx_quantile(CASE WHEN is_stream
                AND response_time IS NOT NULL AND complete_time IS NOT NULL
                AND output_tokens IS NOT NULL AND output_tokens > 0
                THEN (EPOCH_US(complete_time) - EPOCH_US(response_time))
                     / 1000.0 / output_tokens END, 0.5),
            approx_quantile(CASE WHEN is_stream
                AND response_time IS NOT NULL AND complete_time IS NOT NULL
                AND output_tokens IS NOT NULL AND output_tokens > 0
                THEN (EPOCH_US(complete_time) - EPOCH_US(response_time))
                     / 1000.0 / output_tokens END, 0.95),
            approx_quantile(CASE WHEN is_stream
                AND response_time IS NOT NULL AND complete_time IS NOT NULL
                AND output_tokens IS NOT NULL AND output_tokens > 0
                THEN (EPOCH_US(complete_time) - EPOCH_US(response_time))
                     / 1000.0 / output_tokens END, 0.99),
            NULL AS tool_surface
        FROM llm_calls
        GROUP BY
            time_bucket({time_bucket_interval}, request_time),
            source_id,
            GROUPING SETS (
                (wire_api, model, server_ip),
                (wire_api, model),
                (server_ip),
                ()
            )",
    )
}

/// Returns true when llm_metrics needs to be dropped and re-created.
/// Trigger: the new `ttft_stream_sum` column does not sit immediately
/// after `ttft_p99` (which is where the canonical CREATE TABLE places
/// it). That state happens after a previous `ALTER TABLE ADD COLUMN`
/// migration appended the new columns to the end of the table — wrong
/// for our positional-appender writer.
///
/// Returns false when the canonical order is already in place, OR when
/// the new columns are absent entirely (which also can't happen after
/// CREATE_LLM_METRICS just ran — kept for safety in case schema parsing
/// regresses).
fn needs_canonical_rebuild(conn: &duckdb::Connection) -> bool {
    // duckdb_columns() ordered by ordinal. column_index is 0-based.
    let sql = "SELECT column_name FROM duckdb_columns() \
               WHERE table_name = 'llm_metrics' \
               ORDER BY column_index";
    let names: Vec<String> = match conn.prepare(sql) {
        Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read llm_metrics column order; skipping rebuild check");
                return false;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "could not prepare column-order query; skipping rebuild check");
            return false;
        }
    };
    if names.is_empty() {
        return false; // table absent — CREATE handled it
    }
    let p99_idx = names.iter().position(|c| c == "ttft_p99");
    let stream_sum_idx = names.iter().position(|c| c == "ttft_stream_sum");
    match (p99_idx, stream_sum_idx) {
        (Some(p), Some(s)) => s != p + 1,
        // p99 present but stream_sum absent → an upgrade from a pre-split
        // schema that somehow didn't get ALTER'd. Rebuild to be safe.
        (Some(_), None) => true,
        // p99 absent → schema is from before TTFT existed; super unlikely
        // in practice (every shipped version has it). CREATE will have
        // re-made the table from scratch already.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use h_storage::StorageBackend;

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
