use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;

use async_trait::async_trait;
use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::info;
use ts_common::error::{AppError, Result};
use ts_llm::agents::build_default_registry;
use ts_llm::model::{ApiType, LlmCall};
use ts_llm::wire_apis as wa;
use ts_metrics::model::{LlmFinishMetric, LlmMetric};
use ts_protocol::HttpExchange;
use ts_turn::AgentTurn;

use crate::query::*;
use crate::retention::{RetentionPolicy, RetentionReport};
use crate::StorageBackend;

/// Default size of the read-connection pool. DuckDB serializes writes at the
/// database layer anyway; extra read connections only help queries.
const DEFAULT_READ_POOL_SIZE: usize = 4;

/// DuckDB storage backend.
///
/// Uses three dedicated writer connections — one per table (calls / turns /
/// metrics) — each serialized by its own Mutex so that flushes on different
/// tables do not block one another. All three share the same underlying
/// DuckDB database instance via `Connection::try_clone`; DuckDB's MVCC
/// handles inter-transaction isolation for writes to disjoint tables.
///
/// A small pool of reader connections is cloned from the calls writer.
/// Queries never contend with writes on any of the writer Mutexes.
pub struct DuckDbBackend {
    write_calls_conn: Arc<StdMutex<Connection>>,
    write_turns_conn: Arc<StdMutex<Connection>>,
    write_metrics_conn: Arc<StdMutex<Connection>>,
    write_exchanges_conn: Arc<StdMutex<Connection>>,
    read_pool: ReadPool,
}

impl DuckDbBackend {
    /// Open a DuckDB database at the given path with a default-sized read pool.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_with_pool(path, DEFAULT_READ_POOL_SIZE)
    }

    pub fn open_with_pool(path: &str, read_pool_size: usize) -> Result<Self> {
        let calls_writer = Connection::open(path)
            .map_err(|e| AppError::Storage(format!("failed to open duckdb: {e}")))?;
        let turns_writer = calls_writer
            .try_clone()
            .map_err(|e| AppError::Storage(format!("failed to clone turns writer: {e}")))?;
        let metrics_writer = calls_writer
            .try_clone()
            .map_err(|e| AppError::Storage(format!("failed to clone metrics writer: {e}")))?;
        let exchanges_writer = calls_writer
            .try_clone()
            .map_err(|e| AppError::Storage(format!("failed to clone exchanges writer: {e}")))?;

        let pool_size = read_pool_size.max(1);
        let mut readers = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let c = calls_writer
                .try_clone()
                .map_err(|e| AppError::Storage(format!("failed to clone read conn: {e}")))?;
            readers.push(c);
        }

        info!(
            "duckdb opened with 4 writer connections + {} readers",
            pool_size
        );

        Ok(Self {
            write_calls_conn: Arc::new(StdMutex::new(calls_writer)),
            write_turns_conn: Arc::new(StdMutex::new(turns_writer)),
            write_metrics_conn: Arc::new(StdMutex::new(metrics_writer)),
            write_exchanges_conn: Arc::new(StdMutex::new(exchanges_writer)),
            read_pool: ReadPool::new(readers),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_conn(&self) -> &StdMutex<Connection> {
        &self.write_calls_conn
    }
}

/// Small async-friendly connection pool over sync DuckDB connections.
/// `acquire()` is async (awaits a semaphore permit); the returned handle
/// dereferences to `&Connection` and is safe to move into `spawn_blocking`.
#[derive(Clone)]
struct ReadPool {
    conns: Arc<StdMutex<Vec<Connection>>>,
    semaphore: Arc<Semaphore>,
}

impl ReadPool {
    fn new(conns: Vec<Connection>) -> Self {
        let size = conns.len();
        Self {
            conns: Arc::new(StdMutex::new(conns)),
            semaphore: Arc::new(Semaphore::new(size)),
        }
    }

    async fn acquire(&self) -> Result<PooledConn> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| AppError::Storage(format!("read pool closed: {e}")))?;
        let conn = {
            let mut guard = self
                .conns
                .lock()
                .map_err(|e| AppError::Storage(format!("read pool poisoned: {e}")))?;
            guard
                .pop()
                .ok_or_else(|| AppError::Storage("read pool invariant violated".to_string()))?
        };
        Ok(PooledConn {
            conn: Some(conn),
            pool: self.conns.clone(),
            _permit: permit,
        })
    }
}

pub(crate) struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<StdMutex<Vec<Connection>>>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            if let Ok(mut g) = self.pool.lock() {
                g.push(c);
            }
        }
    }
}

impl std::ops::Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("conn present until drop")
    }
}

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

/// Serialize HTTP headers as a JSON array of pairs.
/// Output format: `[["content-type","application/json"],["x-request-id","req_xxx"]]`
/// Preserves header order and allows duplicate keys.
fn headers_to_json(headers: &[(String, String)]) -> String {
    use serde_json::Value;
    let pairs: Vec<Value> = headers
        .iter()
        .map(|(k, v)| Value::Array(vec![Value::String(k.clone()), Value::String(v.clone())]))
        .collect();
    Value::Array(pairs).to_string()
}

/// Convert microseconds since epoch to a string DuckDB can parse as TIMESTAMP.
fn us_to_timestamp(us: i64) -> String {
    let secs = us / 1_000_000;
    let micros = (us.rem_euclid(1_000_000)) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, micros * 1000).unwrap_or_default();
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

/// Parse a JSON-encoded array-of-strings (as stored in agent_turns.models_used /
/// subagents_used / call_ids) into a `Vec<String>`. Missing or malformed values
/// degrade to an empty vec — the turn payload is still returnable.
fn parse_json_string_list(raw: Option<&str>) -> Vec<String> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

enum ExtractKind {
    User,
    Assistant,
}

/// Render a BLOB body for the HTTP exchange detail API. UTF-8 text passes
/// through; binary content (gzip, protobuf, …) falls back to a placeholder so
/// the detail page reflects that bytes were captured rather than showing
/// blank.
fn render_body_for_detail(bytes: Option<Vec<u8>>) -> Option<String> {
    let b = bytes?;
    match String::from_utf8(b) {
        Ok(s) => Some(s),
        Err(e) => Some(format!("[binary, {} bytes]", e.into_bytes().len())),
    }
}

/// Load the request_body / response_body of `call_id` from llm_calls and run
/// it through the `agent_kind`-matched profile to produce the full user_input
/// or final_answer text. Returns `None` if the call row is missing, the
/// profile is not registered, or the extractor declines.
fn extract_full_text(
    conn: &Connection,
    agent_kind: &str,
    call_id: Option<&str>,
    kind: ExtractKind,
) -> Option<String> {
    let call_id = call_id?;
    let registry = build_default_registry();
    let profile = registry.find_by_name(agent_kind)?;

    let sql = match kind {
        ExtractKind::User => "SELECT request_body, wire_api FROM llm_calls WHERE id = ?",
        ExtractKind::Assistant => "SELECT response_body, wire_api FROM llm_calls WHERE id = ?",
    };
    let (body, wire_api_stored): (Option<String>, String) = conn
        .query_row(sql, duckdb::params![call_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .ok()?;
    // Resolve the stored value back to its static constant. An unknown value
    // means the DB has a wire_api this binary no longer knows about — drop
    // the extraction rather than fabricate one.
    let wire_api = wa::by_name(&wire_api_stored)?;
    let (request_body, response_body) = match kind {
        ExtractKind::User => (body, None),
        ExtractKind::Assistant => (None, body),
    };

    // Placeholder LlmCall carrying the real wire_api + bodies; other fields
    // are defaulted because current extractors only read these.
    let call = LlmCall {
        source_id: String::new(),
        id: String::new(),
        wire_api,
        model: String::new(),
        api_type: ApiType::Chat,
        request_time: 0,
        response_time: None,
        complete_time: None,
        request_path: String::new(),
        is_stream: false,
        request_body,
        status_code: None,
        finish_reason: None,
        response_body,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        ttft_ms: None,
        e2e_latency_ms: None,
        client_ip: "0.0.0.0".parse().unwrap(),
        client_port: 0,
        server_ip: "0.0.0.0".parse().unwrap(),
        server_port: 0,
        response_id: None,
        request_headers: Vec::new(),
        response_headers: Vec::new(),
    };
    match kind {
        ExtractKind::User => profile.extract_user_input(&call),
        ExtractKind::Assistant => profile.extract_assistant_text(&call),
    }
}

/// Batch version of `extract_full_text`. Given `(agent_kind, call_id)` pairs
/// and an `ExtractKind` selecting which body column to read, issues a single
/// `SELECT ... WHERE id IN (...)` against `llm_calls` and runs each profile's
/// extractor to produce the final text. Returns a map keyed by `call_id`.
///
/// - Missing call rows, unknown `wire_api`s, or extractors that decline are
///   omitted from the result (caller falls back to the preview string).
/// - Empty `requests` short-circuits to an empty map with zero DB work.
fn extract_full_text_batch(
    conn: &Connection,
    kind: ExtractKind,
    requests: &[(String, String)], // (agent_kind, call_id)
) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut out: HashMap<String, String> = HashMap::new();
    if requests.is_empty() {
        return out;
    }

    // Build agent_kind lookup keyed by call_id (last-writer-wins if a call id
    // appears twice — extremely unlikely given AgentTurn invariants).
    let mut agent_by_call: HashMap<&str, &str> = HashMap::new();
    for (ak, cid) in requests {
        agent_by_call.insert(cid.as_str(), ak.as_str());
    }
    let call_ids: Vec<&str> = agent_by_call.keys().copied().collect();

    let col = match kind {
        ExtractKind::User => "request_body",
        ExtractKind::Assistant => "response_body",
    };
    let placeholders = vec!["?"; call_ids.len()].join(",");
    let sql = format!("SELECT id, wire_api, {col} FROM llm_calls WHERE id IN ({placeholders})");

    let registry = build_default_registry();

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let params: Vec<&dyn duckdb::ToSql> =
        call_ids.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let Ok(mut rows) = stmt.query(duckdb::params_from_iter(params.iter().copied())) else {
        return out;
    };

    while let Ok(Some(row)) = rows.next() {
        let Ok(id): std::result::Result<String, _> = row.get(0) else {
            continue;
        };
        let Ok(wire_api_stored): std::result::Result<String, _> = row.get(1) else {
            continue;
        };
        let body: Option<String> = row.get(2).ok();
        let Some(wire_api) = wa::by_name(&wire_api_stored) else {
            continue;
        };
        let Some(agent_kind) = agent_by_call.get(id.as_str()).copied() else {
            continue;
        };
        let Some(profile) = registry.find_by_name(agent_kind) else {
            continue;
        };

        let (request_body, response_body) = match kind {
            ExtractKind::User => (body, None),
            ExtractKind::Assistant => (None, body),
        };
        let call = LlmCall {
            source_id: String::new(),
            id: String::new(),
            wire_api,
            model: String::new(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: String::new(),
            is_stream: false,
            request_body,
            status_code: None,
            finish_reason: None,
            response_body,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: "0.0.0.0".parse().unwrap(),
            client_port: 0,
            server_ip: "0.0.0.0".parse().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
        };
        let extracted = match kind {
            ExtractKind::User => profile.extract_user_input(&call),
            ExtractKind::Assistant => profile.extract_assistant_text(&call),
        };
        if let Some(text) = extracted {
            out.insert(id, text);
        }
    }

    out
}

/// All valid numeric metric field names accepted by `query_metrics_timeseries`.
/// Virtual `*_avg` fields resolve to `SUM(*_sum) / SUM(*_count)` at query time;
/// the raw `*_sum` / `*_count` fields are also accepted for callers that want
/// to do their own aggregation.
const VALID_METRIC_FIELDS: &[&str] = &[
    "call_count",
    "stream_count",
    "non_stream_count",
    "active_calls_avg",
    "active_calls_sum",
    "active_calls_sample_count",
    "active_calls_max",
    "total_input_tokens",
    "input_token_count",
    "total_output_tokens",
    "output_token_count",
    "input_tokens_avg",
    "output_tokens_avg",
    "total_cache_read_input_tokens",
    "total_cache_creation_input_tokens",
    "error_count",
    "error_4xx_count",
    "error_429_count",
    "error_5xx_count",
    // Phase 5 will read llm_finish_metrics directly via a dedicated query
    // path; finish-reason fields are no longer columns of llm_metrics.
    "ttft_avg",
    "ttft_sum",
    "ttft_count",
    "ttft_p50",
    "ttft_p95",
    "ttft_p99",
    "e2e_avg",
    "e2e_sum",
    "e2e_count",
    "e2e_p50",
    "e2e_p95",
    "e2e_p99",
    "tpot_avg",
    "tpot_sum",
    "tpot_count",
    "tpot_p50",
    "tpot_p95",
    "tpot_p99",
];

/// Build the per-field SQL expressions used by `query_metrics_timeseries`.
///
/// * Additive fields (counts, totals, `*_sum`, `*_count`) → plain `SUM`.
/// * Averages (`*_avg`) → exact ratio `SUM(*_sum) / SUM(*_count)`, derived
///   from the additive sum+count pair so multi-row aggregation (slow-response
///   windows, cross-source merging) stays correct.
/// * Per-row percentiles (`*_p50/p95/p99`) → weighted average by the matching
///   `*_count` (number of samples contributing to the row's digest). This is
///   an approximation until serialized t-digest bytes land; weighting by the
///   count field (rather than `call_count`) keeps slow-response rows with
///   `call_count=0` from falsely collapsing the result to zero.
fn build_field_exprs(fields: &[String]) -> Vec<String> {
    fields
        .iter()
        .map(|f| {
            if SUM_FIELDS.contains(&f.as_str()) {
                format!("CAST(SUM({f}) AS DOUBLE)")
            } else if let Some((sum_col, count_col)) = avg_pair(f) {
                format!(
                    "CASE WHEN SUM({count_col}) > 0 \
                     THEN SUM({sum_col}) / SUM({count_col}) ELSE NULL END"
                )
            } else if f.ends_with("_p50") || f.ends_with("_p95") || f.ends_with("_p99") {
                let weight = percentile_weight(f);
                format!(
                    "CASE WHEN SUM({weight}) > 0 \
                     THEN SUM({f} * {weight}) / SUM({weight}) ELSE NULL END"
                )
            } else {
                format!("CAST(SUM({f}) AS DOUBLE)")
            }
        })
        .collect()
}

/// Map `*_avg` virtual field → `(sum_column, count_column)` pair in the
/// physical schema. `None` for fields that are not averages.
fn avg_pair(f: &str) -> Option<(&'static str, &'static str)> {
    match f {
        "active_calls_avg" => Some(("active_calls_sum", "active_calls_sample_count")),
        "input_tokens_avg" => Some(("total_input_tokens", "input_token_count")),
        "output_tokens_avg" => Some(("total_output_tokens", "output_token_count")),
        "ttft_avg" => Some(("ttft_sum", "ttft_count")),
        "e2e_avg" => Some(("e2e_sum", "e2e_count")),
        "tpot_avg" => Some(("tpot_sum", "tpot_count")),
        _ => None,
    }
}

/// Weight column for percentile weighted-avg aggregation.
fn percentile_weight(field: &str) -> &'static str {
    if field.starts_with("ttft") {
        "ttft_count"
    } else if field.starts_with("e2e") {
        "e2e_count"
    } else if field.starts_with("tpot") {
        "tpot_count"
    } else {
        "call_count"
    }
}

/// Fields that represent counts or totals (use SUM when aggregating across groups).
const SUM_FIELDS: &[&str] = &[
    "call_count",
    "stream_count",
    "non_stream_count",
    "active_calls_sum",
    "active_calls_sample_count",
    "active_calls_max",
    "total_input_tokens",
    "input_token_count",
    "total_output_tokens",
    "output_token_count",
    "total_cache_read_input_tokens",
    "total_cache_creation_input_tokens",
    "error_count",
    "error_4xx_count",
    "error_429_count",
    "error_5xx_count",
    "ttft_sum",
    "ttft_count",
    "e2e_sum",
    "e2e_count",
    "tpot_sum",
    "tpot_count",
];

/// Format a list of string values as a SQL IN list with single-quote escaping.
fn sql_in_list(values: &[String]) -> String {
    values
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a WHERE clause segment for dimension filters on an ungrouped query.
///
/// The aggregator (see `ts-metrics/src/aggregator.rs:dimension_keys`) only
/// materializes 4 of the 8 possible wildcard combinations for
/// `(wire_api, model, server_ip)`:
///
/// - `(W, M, S)` — finest
/// - `(W, M, *)` — per (wire_api, model), summed across servers
/// - `(*, *, S)` — per server_ip only
/// - `(*, *, *)` — grand total
///
/// The mapping below picks the coarsest tier that covers the user's filter
/// and SUMs across the remaining rows. A filter on wire_api or model forces
/// us below the `(*, *, ·)` tier; a filter on server_ip forces us off the
/// `server_ip = '*'` coordinate.
fn build_dimension_where(filter: &DimensionFilter) -> String {
    let has_wire = !filter.wire_apis.is_empty();
    let has_model = !filter.models.is_empty();
    let has_server = !filter.server_ips.is_empty();

    let (wire_clause, model_clause) = if !has_wire && !has_model {
        // Stay on (*, *, ·) tier.
        ("wire_api = '*'".to_string(), "model = '*'".to_string())
    } else {
        // Drop to (W, M, ·) tier — either IN-list or all specific values.
        let w = if has_wire {
            format!("wire_api IN ({})", sql_in_list(&filter.wire_apis))
        } else {
            "wire_api != '*'".to_string()
        };
        let m = if has_model {
            format!("model IN ({})", sql_in_list(&filter.models))
        } else {
            "model != '*'".to_string()
        };
        (w, m)
    };

    let server_clause = if has_server {
        format!("server_ip IN ({})", sql_in_list(&filter.server_ips))
    } else {
        "server_ip = '*'".to_string()
    };

    format!("{wire_clause} AND {model_clause} AND {server_clause}")
}

/// Build WHERE clause for queries that GROUP BY `wire_api` or `model`. The
/// group dimension is always forced to a specific value (never `'*'`); the
/// remaining dimensions follow the same filter/tier rules as
/// [`build_dimension_where`]. Any non-recognized `group_by` falls through to
/// the ungrouped builder.
fn build_dimension_where_for_group(filter: &DimensionFilter, group_by: &str) -> String {
    match group_by {
        "wire_api" | "model" => {
            let wire_clause = if !filter.wire_apis.is_empty() {
                format!("wire_api IN ({})", sql_in_list(&filter.wire_apis))
            } else {
                "wire_api != '*'".to_string()
            };
            let model_clause = if !filter.models.is_empty() {
                format!("model IN ({})", sql_in_list(&filter.models))
            } else {
                "model != '*'".to_string()
            };
            let server_clause = if !filter.server_ips.is_empty() {
                format!("server_ip IN ({})", sql_in_list(&filter.server_ips))
            } else {
                "server_ip = '*'".to_string()
            };
            format!("{wire_clause} AND {model_clause} AND {server_clause}")
        }
        _ => build_dimension_where(filter),
    }
}

/// Bindable row prepared outside the writer Mutex.
/// All expensive conversions (IP formatting, enum → string, header JSON,
/// timestamp wrapping) happen before the lock is acquired.
struct PreparedCall {
    id: String,
    source_id: String,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    request_time: Value,
    response_time: Option<Value>,
    complete_time: Option<Value>,
    wire_api: String,
    model: String,
    api_type: String,
    is_stream: bool,
    request_path: String,
    status_code: Option<u16>,
    finish_reason: Option<String>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    total_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
    ttft_ms: Option<f64>,
    e2e_latency_ms: Option<f64>,
    request_body: Option<String>,
    response_body: Option<String>,
    response_id: Option<String>,
    request_headers: String,
    response_headers: String,
}

fn prepare_call(call: LlmCall) -> PreparedCall {
    PreparedCall {
        id: call.id,
        source_id: call.source_id,
        client_ip: call.client_ip.to_string(),
        client_port: call.client_port,
        server_ip: call.server_ip.to_string(),
        server_port: call.server_port,
        request_time: Value::Timestamp(TimeUnit::Microsecond, call.request_time),
        response_time: call
            .response_time
            .map(|us| Value::Timestamp(TimeUnit::Microsecond, us)),
        complete_time: call
            .complete_time
            .map(|us| Value::Timestamp(TimeUnit::Microsecond, us)),
        wire_api: call.wire_api.to_string(),
        model: call.model,
        api_type: call.api_type.to_string(),
        is_stream: call.is_stream,
        request_path: call.request_path,
        status_code: call.status_code,
        finish_reason: call.finish_reason,
        input_tokens: call.input_tokens,
        output_tokens: call.output_tokens,
        total_tokens: call.total_tokens,
        cache_read_input_tokens: call.cache_read_input_tokens,
        cache_creation_input_tokens: call.cache_creation_input_tokens,
        ttft_ms: call.ttft_ms,
        e2e_latency_ms: call.e2e_latency_ms,
        request_body: call.request_body,
        response_body: call.response_body,
        response_id: call.response_id,
        request_headers: headers_to_json(&call.request_headers),
        response_headers: headers_to_json(&call.response_headers),
    }
}

struct PreparedExchange {
    id: String,
    source_id: String,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    method: String,
    uri: String,
    request_headers: String,
    request_body: Option<Vec<u8>>,
    status: Option<u16>,
    response_headers: String,
    response_body: Option<Vec<u8>>,
    is_sse: bool,
    sse_event_count: u32,
    sse_data_bytes: u64,
    request_time: Value,
    response_first_byte_time: Option<Value>,
    response_complete_time: Option<Value>,
}

fn prepare_exchange(x: HttpExchange) -> PreparedExchange {
    let (client_ip, client_port) = x.client_addr();
    let (server_ip, server_port) = x.server_addr();
    let is_sse = x.is_sse();
    let stored_response_body = x.stored_response_body().map(|b| b.to_vec());
    let request_body = if x.request.body.is_empty() {
        None
    } else {
        Some(x.request.body.to_vec())
    };
    PreparedExchange {
        id: x.id,
        source_id: x.request.flow_key.source_id.clone(),
        client_ip: client_ip.to_string(),
        client_port,
        server_ip: server_ip.to_string(),
        server_port,
        method: x.request.method.clone(),
        uri: x.request.uri.clone(),
        request_headers: headers_to_json(&x.request.headers),
        request_body,
        status: Some(x.response.status),
        response_headers: headers_to_json(&x.response.headers),
        response_body: stored_response_body,
        is_sse,
        sse_event_count: x.sse_event_count,
        sse_data_bytes: x.sse_data_bytes,
        request_time: Value::Timestamp(TimeUnit::Microsecond, x.request.timestamp_us),
        response_first_byte_time: Some(Value::Timestamp(
            TimeUnit::Microsecond,
            x.response.first_byte_timestamp_us,
        )),
        response_complete_time: Some(Value::Timestamp(
            TimeUnit::Microsecond,
            x.response.complete_timestamp_us,
        )),
    }
}

struct PreparedTurn {
    turn_id: String,
    source_id: String,
    session_id: String,
    wire_api: String,
    agent_kind: String,
    start_time: Value,
    end_time: Value,
    duration_ms: u64,
    call_count: u32,
    models_used: String,
    subagents_used: String,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_input_tokens: u64,
    total_cache_creation_input_tokens: u64,
    total_cost_usd: Option<f64>,
    status: String,
    final_finish_reason: Option<String>,
    user_input_preview: Option<String>,
    user_call_id: Option<String>,
    final_answer_preview: Option<String>,
    final_call_id: Option<String>,
    call_ids: String,
    metadata: String,
}

fn prepare_turn(t: AgentTurn) -> PreparedTurn {
    PreparedTurn {
        turn_id: t.turn_id,
        source_id: t.source_id,
        session_id: t.session_id,
        wire_api: t.wire_api,
        agent_kind: t.agent_kind,
        start_time: Value::Timestamp(TimeUnit::Microsecond, t.start_time_us),
        end_time: Value::Timestamp(TimeUnit::Microsecond, t.end_time_us),
        duration_ms: t.duration_ms,
        call_count: t.call_count,
        models_used: serde_json::to_string(&t.models_used).unwrap_or_default(),
        subagents_used: serde_json::to_string(&t.subagents_used).unwrap_or_default(),
        total_input_tokens: t.total_input_tokens,
        total_output_tokens: t.total_output_tokens,
        total_cache_read_input_tokens: t.total_cache_read_input_tokens,
        total_cache_creation_input_tokens: t.total_cache_creation_input_tokens,
        total_cost_usd: t.total_cost_usd,
        status: t.status.to_string(),
        final_finish_reason: t.final_finish_reason,
        user_input_preview: t.user_input_preview,
        user_call_id: t.user_call_id,
        final_answer_preview: t.final_answer_preview,
        final_call_id: t.final_call_id,
        call_ids: serde_json::to_string(&t.call_ids).unwrap_or_default(),
        metadata: t.metadata.to_string(),
    }
}

struct PreparedMetric {
    timestamp: Value,
    source_id: String,
    granularity: &'static str,
    wire_api: String,
    model: String,
    server_ip: String,
    inner: LlmMetric,
}

fn prepare_metric(m: LlmMetric) -> PreparedMetric {
    PreparedMetric {
        timestamp: Value::Timestamp(TimeUnit::Microsecond, m.timestamp_us),
        source_id: m.source_id.clone(),
        granularity: m.granularity,
        wire_api: m.wire_api.clone(),
        model: m.model.clone(),
        server_ip: m.server_ip.clone(),
        inner: m,
    }
}

#[async_trait]
impl StorageBackend for DuckDbBackend {
    async fn init(&self) -> Result<()> {
        // Any writer works — they share the same DuckDB instance. Using the
        // calls writer keeps init deterministic.
        let conn = self.write_calls_conn.clone();
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

    async fn write_calls(&self, calls: Vec<LlmCall>) -> Result<()> {
        if calls.is_empty() {
            return Ok(());
        }
        let conn = self.write_calls_conn.clone();
        tokio::task::spawn_blocking(move || {
            // Serialize/format outside the writer Mutex so the lock is held
            // only for the append + flush.
            let prepared: Vec<PreparedCall> = calls.into_iter().map(prepare_call).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("llm_calls")
                .map_err(|e| AppError::Storage(format!("failed to create appender: {e}")))?;
            for p in &prepared {
                appender
                    .append_row(duckdb::params![
                        p.id,
                        p.source_id,
                        p.client_ip,
                        p.client_port,
                        p.server_ip,
                        p.server_port,
                        p.request_time,
                        p.response_time,
                        p.complete_time,
                        p.wire_api,
                        p.model,
                        p.api_type,
                        p.is_stream,
                        p.request_path,
                        p.status_code,
                        p.finish_reason,
                        p.input_tokens,
                        p.output_tokens,
                        p.total_tokens,
                        p.cache_read_input_tokens,
                        p.cache_creation_input_tokens,
                        p.ttft_ms,
                        p.e2e_latency_ms,
                        p.request_body,
                        p.response_body,
                        p.response_id,
                        p.request_headers,
                        p.response_headers,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append call: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush calls: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn write_exchanges(&self, exchanges: Vec<HttpExchange>) -> Result<()> {
        if exchanges.is_empty() {
            return Ok(());
        }
        let conn = self.write_exchanges_conn.clone();
        tokio::task::spawn_blocking(move || {
            let prepared: Vec<PreparedExchange> =
                exchanges.into_iter().map(prepare_exchange).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("http_exchanges")
                .map_err(|e| AppError::Storage(format!("failed to create appender: {e}")))?;
            for p in &prepared {
                appender
                    .append_row(duckdb::params![
                        p.id,
                        p.source_id,
                        p.client_ip,
                        p.client_port,
                        p.server_ip,
                        p.server_port,
                        p.method,
                        p.uri,
                        p.request_headers,
                        p.request_body,
                        p.status,
                        p.response_headers,
                        p.response_body,
                        p.is_sse,
                        p.sse_event_count,
                        p.sse_data_bytes,
                        p.request_time,
                        p.response_first_byte_time,
                        p.response_complete_time,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append exchange: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush exchanges: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_http_exchange_by_id(&self, id: &str) -> Result<Option<HttpExchangeDetail>> {
        let conn = self.read_pool.acquire().await?;
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let sql = "
                SELECT id, source_id,
                    client_ip, client_port, server_ip, server_port,
                    method, uri,
                    request_headers, request_body,
                    status, response_headers, response_body, is_sse,
                    sse_event_count, sse_data_bytes,
                    epoch_ms(request_time),
                    epoch_ms(response_first_byte_time),
                    epoch_ms(response_complete_time)
                FROM http_exchanges
                WHERE id = ?
            ";
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare exchange query: {e}")))?;
            let result = stmt.query_row(duckdb::params![id], |row| {
                let request_body_bytes: Option<Vec<u8>> = row.get(9)?;
                let response_body_bytes: Option<Vec<u8>> = row.get(12)?;
                Ok(HttpExchangeDetail {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    client_ip: row.get(2)?,
                    client_port: row.get(3)?,
                    server_ip: row.get(4)?,
                    server_port: row.get(5)?,
                    method: row.get(6)?,
                    uri: row.get(7)?,
                    request_headers: row.get(8)?,
                    request_body: render_body_for_detail(request_body_bytes),
                    status: row.get(10)?,
                    response_headers: row.get(11)?,
                    response_body: render_body_for_detail(response_body_bytes),
                    is_sse: row.get(13)?,
                    sse_event_count: row.get(14)?,
                    sse_data_bytes: row.get(15)?,
                    request_time: row.get(16)?,
                    response_first_byte_time: row.get(17)?,
                    response_complete_time: row.get(18)?,
                })
            });
            match result {
                Ok(d) => Ok(Some(d)),
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(AppError::Storage(format!(
                    "failed to query http_exchange by id: {e}"
                ))),
            }
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_http_exchanges(&self, query: &HttpExchangesQuery) -> Result<HttpExchangesPage> {
        // `duration_ms` is a derived expression; the others are plain columns.
        const VALID_SORT_FIELDS: &[&str] = &["request_time", "status", "duration_ms"];
        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.to_uppercase() == "ASC" {
            "ASC"
        } else {
            "DESC"
        };

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();
        let sort_order = sort_order.to_string();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let mut where_parts = vec![
                "request_time >= ?".to_string(),
                "request_time < ?".to_string(),
            ];
            if !query.server_ips.is_empty() {
                let list: Vec<String> = query
                    .server_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("server_ip IN ({})", list.join(", ")));
            }
            if !query.client_ips.is_empty() {
                let list: Vec<String> = query
                    .client_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("client_ip IN ({})", list.join(", ")));
            }
            if !query.methods.is_empty() {
                let list: Vec<String> = query
                    .methods
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("method IN ({})", list.join(", ")));
            }
            if !query.status_codes.is_empty() {
                let list: Vec<String> = query.status_codes.iter().map(|c| c.to_string()).collect();
                where_parts.push(format!("status IN ({})", list.join(", ")));
            }
            if let Some(substr) = query
                .uri_contains
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                where_parts.push(format!("uri LIKE '%{}%'", substr.replace('\'', "''")));
            }
            if let Some(sse) = query.is_sse {
                where_parts.push(format!("is_sse = {sse}"));
            }

            let where_sql = where_parts.join(" AND ");
            // Map virtual field → column/expression for ORDER BY. `duration_ms`
            // becomes an expression; plain columns pass through. NULLS LAST
            // keeps incomplete (duration/status=None) rows from dominating
            // descending sort.
            let order_expr = match query.sort_by.as_str() {
                "duration_ms" => "epoch_ms(response_complete_time - request_time) NULLS LAST",
                "status" => "status NULLS LAST",
                _ => "request_time",
            };

            // COUNT
            let count_sql = format!("SELECT COUNT(*) FROM http_exchanges WHERE {where_sql}");
            let mut count_stmt = conn
                .prepare(&count_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare count query: {e}")))?;
            let total: u64 = count_stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| row.get(0))
                .map_err(|e| AppError::Storage(format!("failed to execute count query: {e}")))?;

            // Items
            let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
            let limit = query.page_size;
            let items_sql = format!(
                "SELECT id, source_id, epoch_ms(request_time), \
                 method, uri, client_ip, server_ip, server_port, \
                 status, is_sse, \
                 CASE WHEN response_complete_time IS NOT NULL \
                      THEN epoch_ms(response_complete_time - request_time) \
                      ELSE NULL END AS duration_ms \
                 FROM http_exchanges WHERE {where_sql} \
                 ORDER BY {order_expr} {sort_order} \
                 LIMIT {limit} OFFSET {offset}"
            );
            let mut items_stmt = conn
                .prepare(&items_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare items query: {e}")))?;

            let mut items = Vec::new();
            let mut rows = items_stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute items query: {e}")))?;
            while let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                items.push(HttpExchangeListItem {
                    id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    source_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_time: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    method: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    uri: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    client_ip: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_ip: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_port: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    status: row
                        .get::<_, Option<u16>>(8)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    is_sse: row
                        .get(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    duration_ms: row
                        .get::<_, Option<f64>>(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                });
            }
            Ok(HttpExchangesPage { total, items })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn write_metrics(&self, metrics: Vec<LlmMetric>) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        let conn = self.write_metrics_conn.clone();
        tokio::task::spawn_blocking(move || {
            let prepared: Vec<PreparedMetric> = metrics.into_iter().map(prepare_metric).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("llm_metrics")
                .map_err(|e| AppError::Storage(format!("failed to create appender: {e}")))?;
            for p in &prepared {
                let m = &p.inner;
                appender
                    .append_row(duckdb::params![
                        p.timestamp,
                        p.source_id,
                        p.granularity,
                        p.wire_api,
                        p.model,
                        p.server_ip,
                        m.call_count,
                        m.stream_count,
                        m.non_stream_count,
                        m.active_calls_sum,
                        m.active_calls_sample_count,
                        m.active_calls_max,
                        m.total_input_tokens,
                        m.input_token_count,
                        m.total_output_tokens,
                        m.output_token_count,
                        m.total_cache_read_input_tokens,
                        m.total_cache_creation_input_tokens,
                        m.error_count,
                        m.error_4xx_count,
                        m.error_429_count,
                        m.error_5xx_count,
                        m.ttft_sum,
                        m.ttft_count,
                        m.ttft_p50,
                        m.ttft_p95,
                        m.ttft_p99,
                        m.e2e_sum,
                        m.e2e_count,
                        m.e2e_p50,
                        m.e2e_p95,
                        m.e2e_p99,
                        m.tpot_sum,
                        m.tpot_count,
                        m.tpot_p50,
                        m.tpot_p95,
                        m.tpot_p99,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append metric: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush metrics: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn write_finish_metrics(&self, metrics: Vec<LlmFinishMetric>) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        // Shares the metrics writer Mutex with `write_metrics` so the two
        // long/wide rollups for one bucket flush serialize against each other
        // — they always come in pairs from the bucket finalizer and writing
        // them on the same connection avoids cross-table interleaving.
        let conn = self.write_metrics_conn.clone();
        tokio::task::spawn_blocking(move || {
            // Pre-format the timestamp Value outside the writer lock, same
            // pattern as `prepare_metric`.
            let prepared: Vec<(Value, LlmFinishMetric)> = metrics
                .into_iter()
                .map(|m| (Value::Timestamp(TimeUnit::Microsecond, m.timestamp_us), m))
                .collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn.appender("llm_finish_metrics").map_err(|e| {
                AppError::Storage(format!("failed to create llm_finish_metrics appender: {e}"))
            })?;
            for (ts, m) in &prepared {
                appender
                    .append_row(duckdb::params![
                        ts,
                        m.source_id,
                        m.granularity,
                        m.wire_api,
                        m.model,
                        m.server_ip,
                        m.finish_reason,
                        m.count,
                    ])
                    .map_err(|e| {
                        AppError::Storage(format!("failed to append finish metric: {e}"))
                    })?;
            }
            appender.flush().map_err(|e| {
                AppError::Storage(format!("failed to flush llm_finish_metrics: {e}"))
            })?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn write_turns(&self, turns: Vec<AgentTurn>) -> Result<()> {
        if turns.is_empty() {
            return Ok(());
        }
        let conn = self.write_turns_conn.clone();
        tokio::task::spawn_blocking(move || {
            let prepared: Vec<PreparedTurn> = turns.into_iter().map(prepare_turn).collect();

            let conn = conn
                .lock()
                .map_err(|e| AppError::Storage(format!("failed to lock writer: {e}")))?;
            let mut appender = conn
                .appender("agent_turns")
                .map_err(|e| AppError::Storage(format!("failed to create turns appender: {e}")))?;
            for p in &prepared {
                appender
                    .append_row(duckdb::params![
                        p.turn_id,
                        p.source_id,
                        p.session_id,
                        p.wire_api,
                        p.agent_kind,
                        p.start_time,
                        p.end_time,
                        p.duration_ms,
                        p.call_count,
                        p.models_used,
                        p.subagents_used,
                        p.total_input_tokens,
                        p.total_output_tokens,
                        p.total_cache_read_input_tokens,
                        p.total_cache_creation_input_tokens,
                        p.total_cost_usd,
                        p.status,
                        p.final_finish_reason,
                        p.user_input_preview,
                        p.user_call_id,
                        p.final_answer_preview,
                        p.final_call_id,
                        p.call_ids,
                        p.metadata,
                    ])
                    .map_err(|e| AppError::Storage(format!("failed to append turn: {e}")))?;
            }
            appender
                .flush()
                .map_err(|e| AppError::Storage(format!("failed to flush turns: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_metrics_timeseries(
        &self,
        query: &MetricsTimeseriesQuery,
    ) -> Result<Vec<MetricsTimeseriesRow>> {
        // Validate all requested fields
        for field in &query.fields {
            if !VALID_METRIC_FIELDS.contains(&field.as_str()) {
                return Err(AppError::Storage(format!("invalid metric field: {field}")));
            }
        }

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let field_exprs = build_field_exprs(&query.fields);
            let fields_sql = field_exprs.join(", ");
            let rows = if let Some(ref group_by) = query.group_by {
                // Grouped query: aggregate across the group dimension plus source_id.
                let dim_where = build_dimension_where_for_group(&query.filter, group_by);
                let sql = format!(
                    "SELECT epoch(timestamp) AS ts, {group_by}, {fields_sql} \
                     FROM llm_metrics \
                     WHERE {dim_where} AND granularity = ? AND timestamp >= ? AND timestamp < ? \
                     GROUP BY timestamp, {group_by} \
                     ORDER BY timestamp, {group_by}"
                );

                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    AppError::Storage(format!("failed to prepare timeseries query: {e}"))
                })?;

                let mut rows = Vec::new();
                let mut query_rows = stmt
                    .query(duckdb::params![query.granularity, start_ts, end_ts])
                    .map_err(|e| {
                        AppError::Storage(format!("failed to execute timeseries query: {e}"))
                    })?;
                while let Some(row) = query_rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let ts: i64 = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("ts read error: {e}")))?;
                    let group: String = row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("group read error: {e}")))?;
                    let mut values = Vec::new();
                    for i in 0..query.fields.len() {
                        let v: Option<f64> = row
                            .get(2 + i)
                            .map_err(|e| AppError::Storage(format!("field read error: {e}")))?;
                        values.push(v);
                    }
                    rows.push(MetricsTimeseriesRow {
                        timestamp: ts,
                        group: Some(group),
                        values,
                    });
                }
                rows
            } else {
                // Ungrouped query: still must GROUP BY timestamp because the
                // per-source aggregators emit one row per source per (ts,
                // dim). Without the GROUP BY we'd return N overlapping rows
                // at each timestamp (N = number of capture sources).
                let dim_where = build_dimension_where(&query.filter);
                let sql = format!(
                    "SELECT epoch(timestamp) AS ts, {fields_sql} \
                     FROM llm_metrics \
                     WHERE {dim_where} AND granularity = ? AND timestamp >= ? AND timestamp < ? \
                     GROUP BY timestamp \
                     ORDER BY timestamp"
                );

                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    AppError::Storage(format!("failed to prepare timeseries query: {e}"))
                })?;

                let mut rows = Vec::new();
                let mut query_rows = stmt
                    .query(duckdb::params![query.granularity, start_ts, end_ts])
                    .map_err(|e| {
                        AppError::Storage(format!("failed to execute timeseries query: {e}"))
                    })?;
                while let Some(row) = query_rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let ts: i64 = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("ts read error: {e}")))?;
                    let mut values = Vec::new();
                    for i in 0..query.fields.len() {
                        let v: Option<f64> = row
                            .get(1 + i)
                            .map_err(|e| AppError::Storage(format!("field read error: {e}")))?;
                        values.push(v);
                    }
                    rows.push(MetricsTimeseriesRow {
                        timestamp: ts,
                        group: None,
                        values,
                    });
                }
                rows
            };

            Ok(rows)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_metrics_summary(
        &self,
        query: &MetricsSummaryQuery,
    ) -> Result<MetricsSummaryRow> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let dim_where = build_dimension_where(&query.filter);
            let sql = format!(
                "
                SELECT
                    COALESCE(SUM(call_count), 0),
                    COALESCE(SUM(error_count), 0),
                    COALESCE(SUM(error_4xx_count), 0),
                    COALESCE(SUM(error_429_count), 0),
                    COALESCE(SUM(error_5xx_count), 0),
                    COALESCE(SUM(total_input_tokens), 0),
                    COALESCE(SUM(total_output_tokens), 0),
                    CASE WHEN SUM(ttft_count) > 0
                         THEN SUM(ttft_sum) / SUM(ttft_count) ELSE NULL END,
                    CASE WHEN SUM(e2e_count) > 0
                         THEN SUM(e2e_sum) / SUM(e2e_count) ELSE NULL END,
                    CASE WHEN SUM(tpot_count) > 0
                         THEN SUM(tpot_sum) / SUM(tpot_count) ELSE NULL END
                FROM llm_metrics
                WHERE {dim_where}
                  AND granularity = '10s'
                  AND timestamp >= ? AND timestamp < ?
            "
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare summary query: {e}")))?;

            let row = stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| {
                    Ok(MetricsSummaryRow {
                        call_count: row.get::<_, u64>(0)?,
                        error_count: row.get::<_, u64>(1)?,
                        error_4xx_count: row.get::<_, u64>(2)?,
                        error_429_count: row.get::<_, u64>(3)?,
                        error_5xx_count: row.get::<_, u64>(4)?,
                        total_input_tokens: row.get::<_, u64>(5)?,
                        total_output_tokens: row.get::<_, u64>(6)?,
                        ttft_avg: row.get::<_, Option<f64>>(7)?,
                        e2e_avg: row.get::<_, Option<f64>>(8)?,
                        tpot_avg: row.get::<_, Option<f64>>(9)?,
                    })
                })
                .map_err(|e| AppError::Storage(format!("failed to execute summary query: {e}")))?;

            Ok(row)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_metrics_models(
        &self,
        query: &MetricsModelsQuery,
    ) -> Result<Vec<MetricsModelRow>> {
        const VALID_SORT_FIELDS: &[&str] = &[
            "call_count",
            "error_count",
            "total_input_tokens",
            "total_output_tokens",
            "ttft_avg",
            "ttft_p95",
            "e2e_avg",
            "e2e_p95",
            "tpot_avg",
        ];

        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.to_uppercase() == "ASC" {
            "ASC"
        } else {
            "DESC"
        };

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();
        let sort_order = sort_order.to_string();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let sort_by = &query.sort_by;
            let limit = query.limit;
            // Per-(wire_api, model) breakdown shares the grouped-tier logic:
            // both dimensions are always specific, server_ip follows filter.
            let dim_where = build_dimension_where_for_group(&query.filter, "wire_api");

            let sql = format!(
                "
                SELECT * FROM (
                    SELECT
                        wire_api,
                        model,
                        COALESCE(SUM(call_count), 0) AS call_count,
                        COALESCE(SUM(error_count), 0) AS error_count,
                        COALESCE(SUM(error_4xx_count), 0) AS error_4xx_count,
                        COALESCE(SUM(error_429_count), 0) AS error_429_count,
                        COALESCE(SUM(error_5xx_count), 0) AS error_5xx_count,
                        COALESCE(SUM(total_input_tokens), 0) AS total_input_tokens,
                        COALESCE(SUM(total_output_tokens), 0) AS total_output_tokens,
                        CASE WHEN SUM(ttft_count) > 0
                             THEN SUM(ttft_sum) / SUM(ttft_count)
                             ELSE NULL END AS ttft_avg,
                        CASE WHEN SUM(ttft_count) > 0
                             THEN SUM(ttft_p95 * ttft_count) / SUM(ttft_count)
                             ELSE NULL END AS ttft_p95,
                        CASE WHEN SUM(e2e_count) > 0
                             THEN SUM(e2e_sum) / SUM(e2e_count)
                             ELSE NULL END AS e2e_avg,
                        CASE WHEN SUM(e2e_count) > 0
                             THEN SUM(e2e_p95 * e2e_count) / SUM(e2e_count)
                             ELSE NULL END AS e2e_p95,
                        CASE WHEN SUM(tpot_count) > 0
                             THEN SUM(tpot_sum) / SUM(tpot_count)
                             ELSE NULL END AS tpot_avg
                    FROM llm_metrics
                    WHERE {dim_where}
                      AND granularity = '10s'
                      AND timestamp >= ? AND timestamp < ?
                    GROUP BY wire_api, model
                ) sub
                ORDER BY {sort_by} {sort_order}
                LIMIT {limit}
            "
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare models query: {e}")))?;

            let mut rows = Vec::new();
            let mut query_rows = stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute models query: {e}")))?;

            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                rows.push(MetricsModelRow {
                    wire_api: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    model: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    call_count: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_count: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_4xx_count: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_429_count: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    error_5xx_count: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_input_tokens: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_output_tokens: row
                        .get(8)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttft_avg: row
                        .get(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttft_p95: row
                        .get(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    e2e_avg: row
                        .get(11)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    e2e_p95: row
                        .get(12)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    tpot_avg: row
                        .get(13)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                });
            }

            Ok(rows)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_finish_reasons(
        &self,
        query: &FinishReasonsQuery,
    ) -> Result<Vec<FinishReasonTimeseries>> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            // Pick the matching pre-aggregated dimension tier:
            //   - wire_apis/models both non-empty → (W, M, *) tier, IN-list filter
            //   - both empty → (*, *, *) tier
            //   - only one non-empty → drop to (W, M, *) tier and SUM over
            //     `<other_dim> != '*'` rows. The writer emits (W,M,*), (W,M,·),
            //     (*,*,·), (*,*,*) tiers (see `dimension_keys`); selecting
            //     `wire_api IN (…) AND model != '*' AND server_ip = '*'` lands
            //     squarely on the (W, M, *) rows for the requested wire_apis,
            //     and SUM gives the cross-model rollup the caller wants.
            //
            // Inlined via format! to match the file's `sql_in_list` convention
            // for IN-list filters. DuckDB has no backslash escaping in string
            // literals, so the doubled-quote escape (`''`) inside `sql_in_list`
            // is complete and safe against injection.
            let has_wire = !query.wire_apis.is_empty();
            let has_model = !query.models.is_empty();
            let has_server = !query.server_ips.is_empty();
            let wire_clause = if has_wire {
                format!("wire_api IN ({})", sql_in_list(&query.wire_apis))
            } else if has_model {
                "wire_api != '*'".to_string()
            } else {
                "wire_api = '*'".to_string()
            };
            let model_clause = if has_model {
                format!("model IN ({})", sql_in_list(&query.models))
            } else if has_wire {
                "model != '*'".to_string()
            } else {
                "model = '*'".to_string()
            };
            // server_ip is independent of wire/model: aggregator emits both
            // (·,·,S) and (·,·,*) tiers in parallel for each (W,M) state.
            let server_clause = if has_server {
                format!("server_ip IN ({})", sql_in_list(&query.server_ips))
            } else {
                "server_ip = '*'".to_string()
            };

            let sql = format!(
                "SELECT epoch_us(timestamp) AS ts_us, finish_reason, SUM(count) AS c \
                 FROM llm_finish_metrics \
                 WHERE granularity = ? \
                   AND timestamp >= ? AND timestamp < ? \
                   AND {wire_clause} AND {model_clause} \
                   AND {server_clause} \
                 GROUP BY ts_us, finish_reason \
                 ORDER BY finish_reason ASC, ts_us ASC"
            );

            let mut stmt = conn.prepare(&sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare finish-reasons query: {e}"))
            })?;
            let mut query_rows = stmt
                .query(duckdb::params![query.granularity, start_ts, end_ts])
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute finish-reasons query: {e}"))
                })?;

            // Bucket rows into series by finish_reason. ORDER BY guarantees
            // each series' points arrive contiguously and timestamp-sorted.
            let mut out: Vec<FinishReasonTimeseries> = Vec::new();
            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let ts_us: i64 = row
                    .get(0)
                    .map_err(|e| AppError::Storage(format!("ts read error: {e}")))?;
                let finish_reason: String = row
                    .get(1)
                    .map_err(|e| AppError::Storage(format!("reason read error: {e}")))?;
                let count: u64 = row
                    .get(2)
                    .map_err(|e| AppError::Storage(format!("count read error: {e}")))?;

                match out.last_mut() {
                    Some(last) if last.finish_reason == finish_reason => {
                        last.points.push((ts_us, count));
                    }
                    _ => out.push(FinishReasonTimeseries {
                        finish_reason,
                        points: vec![(ts_us, count)],
                    }),
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage> {
        const VALID_SORT_FIELDS: &[&str] = &[
            "request_time",
            "status_code",
            "ttft_ms",
            "e2e_latency_ms",
            "input_tokens",
            "output_tokens",
        ];

        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.to_uppercase() == "ASC" {
            "ASC"
        } else {
            "DESC"
        };

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();
        let sort_order = sort_order.to_string();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            // Build WHERE clauses
            let mut where_parts = vec![
                "request_time >= ?".to_string(),
                "request_time < ?".to_string(),
            ];

            if !query.filter.wire_apis.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .wire_apis
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("wire_api IN ({})", list.join(", ")));
            }
            if !query.filter.models.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .models
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("model IN ({})", list.join(", ")));
            }
            if !query.filter.server_ips.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .server_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("server_ip IN ({})", list.join(", ")));
            }
            if !query.status_codes.is_empty() {
                let list: Vec<String> = query.status_codes.iter().map(|c| c.to_string()).collect();
                where_parts.push(format!("status_code IN ({})", list.join(", ")));
            }
            if !query.finish_reasons.is_empty() {
                let list: Vec<String> = query
                    .finish_reasons
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("finish_reason IN ({})", list.join(", ")));
            }
            if !query.client_ips.is_empty() {
                let list: Vec<String> = query
                    .client_ips
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("client_ip IN ({})", list.join(", ")));
            }
            if let Some(substr) = query
                .request_path_contains
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                where_parts.push(format!(
                    "request_path LIKE '%{}%'",
                    substr.replace('\'', "''")
                ));
            }
            let where_sql = where_parts.join(" AND ");
            let sort_by = &query.sort_by;

            // COUNT query
            let count_sql = format!("SELECT COUNT(*) FROM llm_calls WHERE {where_sql}");
            let mut count_stmt = conn
                .prepare(&count_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare count query: {e}")))?;
            let total: u64 = count_stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| row.get(0))
                .map_err(|e| AppError::Storage(format!("failed to execute count query: {e}")))?;

            // Items query
            let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
            let limit = query.page_size;
            let items_sql = format!(
                "SELECT id, source_id, epoch_ms(request_time), wire_api, model, status_code, is_stream, \
                 finish_reason, ttft_ms, e2e_latency_ms, input_tokens, output_tokens, \
                 client_ip, request_path \
                 FROM llm_calls WHERE {where_sql} \
                 ORDER BY {sort_by} {sort_order} \
                 LIMIT {limit} OFFSET {offset}"
            );

            let mut items_stmt = conn
                .prepare(&items_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare items query: {e}")))?;

            let mut items = Vec::new();
            let mut query_rows = items_stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute items query: {e}")))?;

            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                items.push(CallListItem {
                    id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    source_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_time: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    wire_api: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    model: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    status_code: row
                        .get::<_, Option<u16>>(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    is_stream: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    finish_reason: row
                        .get::<_, Option<String>>(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttft_ms: row
                        .get::<_, Option<f64>>(8)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    e2e_latency_ms: row
                        .get::<_, Option<f64>>(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    input_tokens: row
                        .get::<_, Option<u32>>(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    output_tokens: row
                        .get::<_, Option<u32>>(11)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    client_ip: row
                        .get(12)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_path: row
                        .get(13)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                });
            }

            Ok(CallsPage { total, items })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>> {
        let conn = self.read_pool.acquire().await?;
        let id = id.to_string();

        tokio::task::spawn_blocking(move || {
            let sql = "
                SELECT
                    id, source_id,
                    epoch_ms(request_time),
                    epoch_ms(response_time),
                    epoch_ms(complete_time),
                    wire_api, model, api_type, is_stream, request_path,
                    status_code, finish_reason,
                    input_tokens, output_tokens, total_tokens,
                    ttft_ms, e2e_latency_ms,
                    response_id,
                    client_ip, client_port, server_ip, server_port,
                    request_body, response_body,
                    request_headers, response_headers
                FROM llm_calls
                WHERE id = ?
                LIMIT 1
            ";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare call_by_id query: {e}"))
            })?;

            let result = stmt.query_row(duckdb::params![id], |row| {
                Ok(CallDetail {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    request_time: row.get(2)?,
                    response_time: row.get(3)?,
                    complete_time: row.get(4)?,
                    wire_api: row.get(5)?,
                    model: row.get(6)?,
                    api_type: row.get(7)?,
                    is_stream: row.get(8)?,
                    request_path: row.get(9)?,
                    status_code: row.get(10)?,
                    finish_reason: row.get(11)?,
                    input_tokens: row.get(12)?,
                    output_tokens: row.get(13)?,
                    total_tokens: row.get(14)?,
                    ttft_ms: row.get(15)?,
                    e2e_latency_ms: row.get(16)?,
                    response_id: row.get(17)?,
                    client_ip: row.get(18)?,
                    client_port: row.get(19)?,
                    server_ip: row.get(20)?,
                    server_port: row.get(21)?,
                    request_body: row.get(22)?,
                    response_body: row.get(23)?,
                    request_headers: row.get(24)?,
                    response_headers: row.get(25)?,
                })
            });

            match result {
                Ok(detail) => Ok(Some(detail)),
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(AppError::Storage(format!(
                    "failed to query call by id: {e}"
                ))),
            }
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_turns(&self, query: &TurnsQuery) -> Result<TurnsPage> {
        const VALID_SORT_FIELDS: &[&str] = &[
            "start_time",
            "end_time",
            "duration_ms",
            "call_count",
            "total_input_tokens",
            "total_output_tokens",
        ];

        if !VALID_SORT_FIELDS.contains(&query.sort_by.as_str()) {
            return Err(AppError::Storage(format!(
                "invalid sort_by field: {}",
                query.sort_by
            )));
        }
        let sort_order = if query.sort_order.to_uppercase() == "ASC" {
            "ASC"
        } else {
            "DESC"
        };

        let conn = self.read_pool.acquire().await?;
        let query = query.clone();
        let sort_order = sort_order.to_string();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let mut where_parts = vec!["start_time >= ?".to_string(), "start_time < ?".to_string()];

            if !query.filter.wire_apis.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .wire_apis
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("wire_api IN ({})", list.join(", ")));
            }
            if !query.filter.models.is_empty() {
                // models_used is stored as a JSON-encoded VARCHAR of Vec<String>.
                // Match if any requested model appears in the stored list.
                let list: Vec<String> = query
                    .filter
                    .models
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!(
                    "list_has_any(CAST(CAST(models_used AS JSON) AS VARCHAR[]), [{}])",
                    list.join(", ")
                ));
            }
            if !query.statuses.is_empty() {
                let list: Vec<String> = query
                    .statuses
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("status IN ({})", list.join(", ")));
            }
            if !query.agent_kinds.is_empty() {
                let list: Vec<String> = query
                    .agent_kinds
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("agent_kind IN ({})", list.join(", ")));
            }

            let where_sql = where_parts.join(" AND ");
            let sort_by = &query.sort_by;

            let count_sql = format!("SELECT COUNT(*) FROM agent_turns WHERE {where_sql}");
            let mut count_stmt = conn
                .prepare(&count_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare count query: {e}")))?;
            let total: u64 = count_stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| row.get(0))
                .map_err(|e| AppError::Storage(format!("failed to execute count query: {e}")))?;

            let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
            let limit = query.page_size;
            let items_sql = format!(
                "SELECT turn_id, source_id, session_id, \
                 epoch_ms(start_time), epoch_ms(end_time), duration_ms, \
                 wire_api, agent_kind, models_used, call_count, \
                 total_input_tokens, total_output_tokens, status, \
                 final_finish_reason, user_input_preview, final_answer_preview \
                 FROM agent_turns WHERE {where_sql} \
                 ORDER BY {sort_by} {sort_order} \
                 LIMIT {limit} OFFSET {offset}"
            );

            let mut items_stmt = conn
                .prepare(&items_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare items query: {e}")))?;

            let mut items = Vec::new();
            let mut query_rows = items_stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| AppError::Storage(format!("failed to execute items query: {e}")))?;

            while let Some(row) = query_rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let models_used_raw: Option<String> = row
                    .get(8)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let models_used = parse_json_string_list(models_used_raw.as_deref());
                let primary_model = models_used.first().cloned();
                items.push(TurnListItem {
                    turn_id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    source_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    session_id: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    start_time: row
                        .get(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    end_time: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    duration_ms: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    wire_api: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    agent_kind: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    primary_model,
                    models_used,
                    call_count: row
                        .get(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_input_tokens: row
                        .get(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    total_output_tokens: row
                        .get(11)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    status: row
                        .get(12)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    final_finish_reason: row
                        .get::<_, Option<String>>(13)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    user_input_preview: row
                        .get::<_, Option<String>>(14)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    final_answer_preview: row
                        .get::<_, Option<String>>(15)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                });
            }

            Ok(TurnsPage { total, items })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_turn_by_id(&self, turn_id: &str) -> Result<Option<TurnDetail>> {
        let conn = self.read_pool.acquire().await?;
        let turn_id = turn_id.to_string();

        tokio::task::spawn_blocking(move || {
            let sql = "
                SELECT
                    turn_id, source_id, session_id, wire_api, agent_kind,
                    epoch_ms(start_time), epoch_ms(end_time), duration_ms, call_count,
                    models_used, subagents_used,
                    total_input_tokens, total_output_tokens,
                    total_cache_read_input_tokens, total_cache_creation_input_tokens,
                    total_cost_usd, status, final_finish_reason,
                    user_input_preview, user_call_id,
                    final_answer_preview, final_call_id,
                    call_ids, metadata
                FROM agent_turns
                WHERE turn_id = ?
            ";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare turn_by_id query: {e}"))
            })?;

            #[allow(clippy::type_complexity)]
            let result = stmt.query_row(duckdb::params![turn_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,          // turn_id
                    row.get::<_, String>(1)?,          // source_id
                    row.get::<_, String>(2)?,          // session_id
                    row.get::<_, String>(3)?,          // wire_api
                    row.get::<_, String>(4)?,          // agent_kind
                    row.get::<_, i64>(5)?,             // start_time
                    row.get::<_, i64>(6)?,             // end_time
                    row.get::<_, u64>(7)?,             // duration_ms
                    row.get::<_, u32>(8)?,             // call_count
                    row.get::<_, Option<String>>(9)?,  // models_used
                    row.get::<_, Option<String>>(10)?, // subagents_used
                    row.get::<_, u64>(11)?,            // total_input_tokens
                    row.get::<_, u64>(12)?,            // total_output_tokens
                    row.get::<_, u64>(13)?,            // total_cache_read_input_tokens
                    row.get::<_, u64>(14)?,            // total_cache_creation_input_tokens
                    row.get::<_, Option<f64>>(15)?,    // total_cost_usd
                    row.get::<_, String>(16)?,         // status
                    row.get::<_, Option<String>>(17)?, // final_finish_reason
                    row.get::<_, Option<String>>(18)?, // user_input_preview
                    row.get::<_, Option<String>>(19)?, // user_call_id
                    row.get::<_, Option<String>>(20)?, // final_answer_preview
                    row.get::<_, Option<String>>(21)?, // final_call_id
                    row.get::<_, Option<String>>(22)?, // call_ids (JSON)
                    row.get::<_, Option<String>>(23)?, // metadata
                ))
            });

            let tuple = match result {
                Ok(t) => t,
                Err(duckdb::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => {
                    return Err(AppError::Storage(format!(
                        "failed to query turn by id: {e}"
                    )));
                }
            };

            let (
                turn_id,
                source_id,
                session_id,
                wire_api,
                agent_kind,
                start_time,
                end_time,
                duration_ms,
                call_count,
                models_used_raw,
                subagents_used_raw,
                total_input_tokens,
                total_output_tokens,
                total_cache_read_input_tokens,
                total_cache_creation_input_tokens,
                total_cost_usd,
                status,
                final_finish_reason,
                user_input_preview,
                user_call_id,
                final_answer_preview,
                final_call_id,
                call_ids_raw,
                metadata_raw,
            ) = tuple;

            let models_used = parse_json_string_list(models_used_raw.as_deref());
            let subagents_used = parse_json_string_list(subagents_used_raw.as_deref());
            let call_ids = parse_json_string_list(call_ids_raw.as_deref());
            let metadata = metadata_raw
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());

            // `truncate_preview` in ts-turn appends `…` only when it truncates,
            // so a preview that does not end in `…` is already the full text —
            // skip the llm_calls lookup + profile re-extraction in that case.
            let user_input = match user_input_preview.as_deref() {
                Some(p) if !p.ends_with('…') => user_input_preview.clone(),
                _ => extract_full_text(
                    &conn,
                    &agent_kind,
                    user_call_id.as_deref(),
                    ExtractKind::User,
                )
                .or_else(|| user_input_preview.clone()),
            };
            let final_answer = match final_answer_preview.as_deref() {
                Some(p) if !p.ends_with('…') => final_answer_preview.clone(),
                _ => extract_full_text(
                    &conn,
                    &agent_kind,
                    final_call_id.as_deref(),
                    ExtractKind::Assistant,
                )
                .or_else(|| final_answer_preview.clone()),
            };

            Ok(Some(TurnDetail {
                turn_id,
                source_id,
                session_id,
                wire_api,
                agent_kind,
                start_time,
                end_time,
                duration_ms,
                call_count,
                models_used,
                subagents_used,
                total_input_tokens,
                total_output_tokens,
                total_cache_read_input_tokens,
                total_cache_creation_input_tokens,
                total_cost_usd,
                status,
                final_finish_reason,
                user_call_id,
                user_input,
                final_call_id,
                final_answer,
                call_ids,
                metadata,
            }))
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_turn_calls(&self, turn_id: &str) -> Result<Vec<TurnCallItem>> {
        let conn = self.read_pool.acquire().await?;
        let turn_id = turn_id.to_string();

        tokio::task::spawn_blocking(move || {
            // Step 1: fetch the turn's call_ids by PK. JSON-array column is
            // parsed with the same helper as query_turn_by_id.
            let call_ids_raw: Option<String> = {
                let mut stmt = conn
                    .prepare("SELECT call_ids FROM agent_turns WHERE turn_id = ?")
                    .map_err(|e| {
                        AppError::Storage(format!("failed to prepare turn_calls step1: {e}"))
                    })?;
                match stmt.query_row(duckdb::params![turn_id], |row| {
                    row.get::<_, Option<String>>(0)
                }) {
                    Ok(v) => v,
                    Err(duckdb::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
                    Err(e) => {
                        return Err(AppError::Storage(format!(
                            "failed to execute turn_calls step1: {e}"
                        )));
                    }
                }
            };

            let call_ids = parse_json_string_list(call_ids_raw.as_deref());
            if call_ids.is_empty() {
                return Ok(Vec::new());
            }

            // Step 2: fetch calls by id via PK point-lookups in IN (...).
            let placeholders = std::iter::repeat("?")
                .take(call_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT
                    id,
                    epoch_ms(request_time),
                    epoch_ms(response_time),
                    epoch_ms(complete_time),
                    wire_api, model, status_code, is_stream,
                    finish_reason, ttft_ms, e2e_latency_ms,
                    input_tokens, output_tokens,
                    request_path, client_ip, client_port,
                    server_ip, server_port,
                    request_body, response_body,
                    request_headers, response_headers
                FROM llm_calls
                WHERE id IN ({placeholders})
                ORDER BY request_time ASC, complete_time ASC"
            );

            let mut stmt = conn.prepare(&sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare turn_calls step2: {e}"))
            })?;

            let mut rows = stmt
                .query(duckdb::params_from_iter(call_ids.iter()))
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute turn_calls step2: {e}"))
                })?;

            let mut items = Vec::new();
            let mut seq: u32 = 0;
            while let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                seq += 1;
                items.push(TurnCallItem {
                    id: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    sequence: seq,
                    request_time: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    response_time: row
                        .get::<_, Option<i64>>(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    complete_time: row
                        .get::<_, Option<i64>>(3)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    wire_api: row
                        .get(4)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    model: row
                        .get(5)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    status_code: row
                        .get::<_, Option<u16>>(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    is_stream: row
                        .get(7)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    finish_reason: row
                        .get::<_, Option<String>>(8)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttft_ms: row
                        .get::<_, Option<f64>>(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    e2e_latency_ms: row
                        .get::<_, Option<f64>>(10)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    input_tokens: row
                        .get::<_, Option<u32>>(11)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    output_tokens: row
                        .get::<_, Option<u32>>(12)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_path: row
                        .get(13)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    client_ip: row
                        .get(14)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    client_port: row
                        .get(15)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_ip: row
                        .get(16)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    server_port: row
                        .get(17)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_body: row
                        .get::<_, Option<String>>(18)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    response_body: row
                        .get::<_, Option<String>>(19)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_headers: row
                        .get::<_, Option<String>>(20)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    response_headers: row
                        .get::<_, Option<String>>(21)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                });
            }

            Ok(items)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_sessions(&self, query: &SessionListQuery) -> Result<SessionsPage> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);
            let page_size = query.page_size.max(1);

            // Step 1 WHERE: time window + optional source/agent_kind. Both
            // optional fields are session-stable (same session -> same value),
            // so pushing them into WHERE does not truncate the lifetime
            // aggregates computed in Step 2.
            let mut where_parts: Vec<String> = vec![
                "end_time >= ?".to_string(),
                "end_time < ?".to_string(),
            ];
            if let Some(sid) = &query.source_id {
                where_parts.push(format!("source_id = '{}'", sid.replace('\'', "''")));
            }
            if let Some(ak) = &query.agent_kind {
                where_parts.push(format!("agent_kind = '{}'", ak.replace('\'', "''")));
            }
            let where_sql = where_parts.join(" AND ");

            // Cursor HAVING clause. Tuple comparison lets us sort by
            // (MAX(end_time), source_id, session_id) DESC uniformly.
            let (having_sql, cursor_ts) = if let Some(c) = &query.cursor {
                let ts = us_to_timestamp(c.last_turn_at_ms.saturating_mul(1000));
                let sid = c.source_id.replace('\'', "''");
                let sess = c.session_id.replace('\'', "''");
                (
                    format!(
                        " HAVING (MAX(end_time), source_id, session_id) < (CAST(? AS TIMESTAMP), '{sid}', '{sess}')"
                    ),
                    Some(ts),
                )
            } else {
                (String::new(), None)
            };

            // Fetch one extra row to detect the next page without a count query.
            let limit = (page_size as u64) + 1;

            let step1_sql = format!(
                "SELECT source_id, session_id, epoch_ms(MAX(end_time)) AS last_ms \
                 FROM agent_turns \
                 WHERE {where_sql} \
                 GROUP BY source_id, session_id{having_sql} \
                 ORDER BY MAX(end_time) DESC, source_id DESC, session_id DESC \
                 LIMIT {limit}"
            );

            let mut stmt = conn.prepare(&step1_sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare sessions step1: {e}"))
            })?;

            let mut key_rows: Vec<(String, String, i64)> = Vec::new();
            {
                let mut rows = match &cursor_ts {
                    Some(cts) => stmt.query(duckdb::params![start_ts, end_ts, cts]),
                    None => stmt.query(duckdb::params![start_ts, end_ts]),
                }
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute sessions step1: {e}"))
                })?;

                while let Some(row) = rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let src: String = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let sess: String = row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let ms: i64 = row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    key_rows.push((src, sess, ms));
                }
            }

            let has_more = key_rows.len() > page_size as usize;
            if has_more {
                key_rows.truncate(page_size as usize);
            }
            if key_rows.is_empty() {
                return Ok(SessionsPage {
                    items: vec![],
                    next_cursor: None,
                });
            }

            // Step 2: full-lifetime aggregate + first-turn preview via
            // ROW_NUMBER(). Pair list is inlined because DuckDB's `IN ((?, ?))`
            // with positional params gets awkward and the ids are trusted
            // internal strings already vetted by Step 1.
            let pairs_sql = key_rows
                .iter()
                .map(|(s, k, _)| {
                    format!(
                        "('{}', '{}')",
                        s.replace('\'', "''"),
                        k.replace('\'', "''")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            let step2_sql = format!(
                "SELECT source_id, session_id, \
                        epoch_ms(MIN(start_time)) AS first_ms, \
                        epoch_ms(MAX(end_time))   AS last_ms, \
                        COUNT(*) AS turn_count, \
                        SUM(call_count) AS call_count, \
                        SUM(total_input_tokens) AS total_in, \
                        SUM(total_output_tokens) AS total_out, \
                        SUM(total_cache_read_input_tokens) AS total_cr, \
                        SUM(total_cache_creation_input_tokens) AS total_cc, \
                        SUM(total_cost_usd) AS total_cost, \
                        MIN(agent_kind) AS agent_kind, \
                        MIN(CASE WHEN rn = 1 THEN user_input_preview END) AS first_input, \
                        MIN(CASE WHEN rn = 1 THEN user_call_id      END) AS first_call_id \
                 FROM ( \
                    SELECT source_id, session_id, start_time, end_time, call_count, \
                           total_input_tokens, total_output_tokens, \
                           total_cache_read_input_tokens, total_cache_creation_input_tokens, \
                           total_cost_usd, agent_kind, user_input_preview, user_call_id, \
                           ROW_NUMBER() OVER (PARTITION BY source_id, session_id ORDER BY start_time) AS rn \
                    FROM agent_turns \
                    WHERE (source_id, session_id) IN ({pairs_sql}) \
                 ) t \
                 GROUP BY source_id, session_id"
            );

            let mut stmt2 = conn.prepare(&step2_sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare sessions step2: {e}"))
            })?;

            use std::collections::HashMap;
            let mut agg: HashMap<(String, String), SessionListItem> = HashMap::new();
            {
                let mut rows = stmt2.query([]).map_err(|e| {
                    AppError::Storage(format!("failed to execute sessions step2: {e}"))
                })?;
                while let Some(row) = rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let src: String = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let sess: String = row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let item = SessionListItem {
                        source_id: src.clone(),
                        session_id: sess.clone(),
                        last_turn_at_in_window: 0,
                        first_turn_at: row
                            .get(2)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        last_turn_at: row
                            .get(3)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        turn_count: row
                            .get(4)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        call_count: row
                            .get(5)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_input_tokens: row
                            .get(6)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_output_tokens: row
                            .get(7)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_cache_read_input_tokens: row
                            .get(8)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_cache_creation_input_tokens: row
                            .get(9)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_cost_usd: row
                            .get::<_, Option<f64>>(10)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        agent_kind: row
                            .get(11)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        first_user_input_preview: row
                            .get::<_, Option<String>>(12)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        first_user_call_id: row
                            .get::<_, Option<String>>(13)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    };
                    agg.insert((src, sess), item);
                }
            }

            // Preserve Step 1's ordering and inject last_turn_at_in_window.
            let mut items: Vec<SessionListItem> = Vec::with_capacity(key_rows.len());
            for (src, sess, in_window_ms) in &key_rows {
                if let Some(mut it) = agg.remove(&(src.clone(), sess.clone())) {
                    it.last_turn_at_in_window = *in_window_ms;
                    items.push(it);
                }
            }

            let next_cursor = if has_more {
                items.last().map(|it| {
                    encode_session_cursor(&SessionListCursor {
                        last_turn_at_ms: it.last_turn_at_in_window,
                        source_id: it.source_id.clone(),
                        session_id: it.session_id.clone(),
                    })
                })
            } else {
                None
            };

            Ok(SessionsPage { items, next_cursor })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_session_by_id(
        &self,
        source_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionDetail>> {
        let conn = self.read_pool.acquire().await?;
        let source_id = source_id.to_string();
        let session_id = session_id.to_string();

        tokio::task::spawn_blocking(move || {
            let sql = "SELECT source_id, session_id, \
                              epoch_ms(MIN(start_time)) AS first_ms, \
                              epoch_ms(MAX(end_time))   AS last_ms, \
                              COUNT(*) AS turn_count, \
                              SUM(call_count) AS call_count, \
                              SUM(total_input_tokens) AS total_in, \
                              SUM(total_output_tokens) AS total_out, \
                              SUM(total_cache_read_input_tokens) AS total_cr, \
                              SUM(total_cache_creation_input_tokens) AS total_cc, \
                              SUM(total_cost_usd) AS total_cost, \
                              MIN(agent_kind) AS agent_kind, \
                              MIN(CASE WHEN rn = 1 THEN user_input_preview END) AS first_input, \
                              MIN(CASE WHEN rn = 1 THEN user_call_id      END) AS first_call_id \
                       FROM ( \
                          SELECT source_id, session_id, start_time, end_time, call_count, \
                                 total_input_tokens, total_output_tokens, \
                                 total_cache_read_input_tokens, total_cache_creation_input_tokens, \
                                 total_cost_usd, agent_kind, user_input_preview, user_call_id, \
                                 ROW_NUMBER() OVER (PARTITION BY source_id, session_id ORDER BY start_time) AS rn \
                          FROM agent_turns \
                          WHERE source_id = ? AND session_id = ? \
                       ) t \
                       GROUP BY source_id, session_id";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare session_by_id: {e}"))
            })?;
            let mut rows = stmt
                .query(duckdb::params![source_id, session_id])
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute session_by_id: {e}"))
                })?;

            let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            else {
                return Ok(None);
            };
            // GROUP BY always emits a row when the subquery has at least one
            // match; when the session has zero turns the subquery is empty and
            // the outer aggregate emits nothing -> handled above.
            Ok(Some(SessionDetail {
                source_id: row
                    .get(0)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                session_id: row
                    .get(1)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                first_turn_at: row
                    .get(2)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                last_turn_at: row
                    .get(3)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                turn_count: row
                    .get(4)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                call_count: row
                    .get(5)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_input_tokens: row
                    .get(6)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_output_tokens: row
                    .get(7)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_cache_read_input_tokens: row
                    .get(8)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_cache_creation_input_tokens: row
                    .get(9)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_cost_usd: row
                    .get::<_, Option<f64>>(10)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                agent_kind: row
                    .get(11)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                first_user_input_preview: row
                    .get::<_, Option<String>>(12)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                first_user_call_id: row
                    .get::<_, Option<String>>(13)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            }))
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<SessionTurnsPage> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let page_size = query.page_size.max(1);
            let limit = (page_size as u64) + 1;

            // Cursor filter (tuple comparison). ORDER BY start_time DESC, turn_id DESC.
            let (cursor_sql, cursor_values) = if let Some(c) = &query.cursor {
                let ts = us_to_timestamp(c.start_time_us);
                (
                    " AND (start_time, turn_id) < (CAST(? AS TIMESTAMP), ?)".to_string(),
                    Some((ts, c.turn_id.clone())),
                )
            } else {
                (String::new(), None)
            };

            // Paging query. SELECT returns SessionTurnItem columns + preview +
            // call_id for each side so we know whether to run full-text
            // extraction below.
            let sql = format!(
                "SELECT turn_id, source_id, session_id, \
                        epoch_ms(start_time)   AS start_ms, \
                        epoch_ms(end_time)     AS end_ms, \
                        duration_ms, wire_api, agent_kind, \
                        models_used, call_count, \
                        total_input_tokens, total_output_tokens, \
                        status, final_finish_reason, \
                        user_input_preview, user_call_id, \
                        final_answer_preview, final_call_id \
                 FROM agent_turns \
                 WHERE source_id = ? AND session_id = ?{cursor_sql} \
                 ORDER BY start_time DESC, turn_id DESC \
                 LIMIT {limit}"
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare session_turns: {e}")))?;

            #[allow(clippy::type_complexity)]
            let mut fetched: Vec<(
                String,
                String,
                String,
                i64,
                i64,
                u64,
                String,
                String,
                Option<String>,
                u32,
                u64,
                u64,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            )> = Vec::new();

            {
                let mut rows = match &cursor_values {
                    Some((ts, sid)) => {
                        stmt.query(duckdb::params![query.source_id, query.session_id, ts, sid])
                    }
                    None => stmt.query(duckdb::params![query.source_id, query.session_id]),
                }
                .map_err(|e| AppError::Storage(format!("failed to execute session_turns: {e}")))?;

                while let Some(row) = rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let tuple = (
                        row.get(0)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(1)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(2)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(3)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(4)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(5)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(6)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(7)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(8)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(9)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(10)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(11)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(12)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(13)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(14)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(15)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(16)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(17)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    );
                    fetched.push(tuple);
                }
            }

            // Fetch+1 pattern: if we got page_size + 1 rows, there's a next page.
            let has_more = fetched.len() as u64 > page_size as u64;
            if has_more {
                fetched.truncate(page_size as usize);
            }

            // Gather call-ids that need full-text extraction (preview ended with `…`).
            let mut need_user: Vec<(String, String)> = Vec::new(); // (agent_kind, call_id)
            let mut need_assistant: Vec<(String, String)> = Vec::new();
            for t in &fetched {
                let agent_kind = t.7.clone();
                let user_preview = &t.14;
                let user_call_id = &t.15;
                let final_preview = &t.16;
                let final_call_id = &t.17;
                if let (Some(p), Some(cid)) = (user_preview, user_call_id) {
                    if p.ends_with('…') {
                        need_user.push((agent_kind.clone(), cid.clone()));
                    }
                }
                if let (Some(p), Some(cid)) = (final_preview, final_call_id) {
                    if p.ends_with('…') {
                        need_assistant.push((agent_kind, cid.clone()));
                    }
                }
            }
            let user_map = extract_full_text_batch(&conn, ExtractKind::User, &need_user);
            let asst_map = extract_full_text_batch(&conn, ExtractKind::Assistant, &need_assistant);

            let mut items: Vec<SessionTurnItem> = Vec::with_capacity(fetched.len());
            for t in fetched {
                let (
                    turn_id,
                    source_id,
                    session_id,
                    start_ms,
                    end_ms,
                    duration_ms,
                    wire_api,
                    agent_kind,
                    models_used_raw,
                    call_count,
                    total_input_tokens,
                    total_output_tokens,
                    status,
                    final_finish_reason,
                    user_preview,
                    user_call_id,
                    final_preview,
                    final_call_id,
                ) = t;

                let user_input = match (user_preview.as_deref(), user_call_id.as_deref()) {
                    (Some(p), _) if !p.ends_with('…') => Some(p.to_string()),
                    (_, Some(cid)) => user_map.get(cid).cloned().or_else(|| user_preview.clone()),
                    _ => user_preview.clone(),
                };
                let final_answer = match (final_preview.as_deref(), final_call_id.as_deref()) {
                    (Some(p), _) if !p.ends_with('…') => Some(p.to_string()),
                    (_, Some(cid)) => asst_map.get(cid).cloned().or_else(|| final_preview.clone()),
                    _ => final_preview.clone(),
                };

                let models_used = parse_json_string_list(models_used_raw.as_deref());
                let primary_model = models_used.first().cloned();

                items.push(SessionTurnItem {
                    turn_id,
                    source_id,
                    session_id,
                    start_time: start_ms,
                    end_time: end_ms,
                    duration_ms,
                    wire_api,
                    agent_kind,
                    primary_model,
                    models_used,
                    call_count,
                    total_input_tokens,
                    total_output_tokens,
                    status,
                    final_finish_reason,
                    user_input,
                    final_answer,
                });
            }

            let next_cursor = if has_more {
                items.last().map(|last| {
                    encode_session_turns_cursor(&SessionTurnsCursor {
                        start_time_us: last.start_time.saturating_mul(1000),
                        turn_id: last.turn_id.clone(),
                    })
                })
            } else {
                None
            };

            Ok(SessionTurnsPage { items, next_cursor })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_distinct_wire_apis(&self) -> Result<Vec<String>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT wire_api FROM llm_metrics WHERE wire_api != '*' ORDER BY wire_api"
            ).map_err(|e| AppError::Storage(format!("failed to prepare distinct_wire_apis query: {e}")))?;
            let mut rows = stmt.query([])
                .map_err(|e| AppError::Storage(format!("failed to execute distinct_wire_apis query: {e}")))?;
            let mut result = Vec::new();
            while let Some(row) = rows.next().map_err(|e| AppError::Storage(format!("row error: {e}")))? {
                let v: String = row.get(0).map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                result.push(v);
            }
            Ok(result)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_distinct_models(&self) -> Result<Vec<String>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let mut stmt = conn
                .prepare("SELECT DISTINCT model FROM llm_metrics WHERE model != '*' ORDER BY model")
                .map_err(|e| {
                    AppError::Storage(format!("failed to prepare distinct_models query: {e}"))
                })?;
            let mut rows = stmt.query([]).map_err(|e| {
                AppError::Storage(format!("failed to execute distinct_models query: {e}"))
            })?;
            let mut result = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let v: String = row
                    .get(0)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                result.push(v);
            }
            Ok(result)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_distinct_server_ips(&self) -> Result<Vec<String>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT server_ip FROM llm_metrics WHERE server_ip != '*' ORDER BY server_ip"
            ).map_err(|e| AppError::Storage(format!("failed to prepare distinct_server_ips query: {e}")))?;
            let mut rows = stmt.query([])
                .map_err(|e| AppError::Storage(format!("failed to execute distinct_server_ips query: {e}")))?;
            let mut result = Vec::new();
            while let Some(row) = rows.next().map_err(|e| AppError::Storage(format!("row error: {e}")))? {
                let v: String = row.get(0).map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                result.push(v);
            }
            Ok(result)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            // Source: llm_finish_metrics. The `wire_api != '*'` filter excludes
            // the cross-wire-api rollup tier; finish_reason is always concrete
            // in this table (no `*` rows for finish_reason itself), but we keep
            // the symmetry for safety against future schema changes.
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT wire_api, finish_reason \
                     FROM llm_finish_metrics \
                     WHERE wire_api != '*' AND finish_reason != '*' \
                     ORDER BY wire_api, finish_reason",
                )
                .map_err(|e| {
                    AppError::Storage(format!(
                        "failed to prepare distinct_finish_reasons query: {e}"
                    ))
                })?;
            let mut rows = stmt.query([]).map_err(|e| {
                AppError::Storage(format!(
                    "failed to execute distinct_finish_reasons query: {e}"
                ))
            })?;
            let mut result = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            {
                let wire_api: String = row
                    .get(0)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                let finish_reason: String = row
                    .get(1)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                result.push(DistinctFinishReason {
                    wire_api,
                    finish_reason,
                });
            }
            Ok(result)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        let calls_conn = self.write_calls_conn.clone();
        let turns_conn = self.write_turns_conn.clone();
        let metrics_conn = self.write_metrics_conn.clone();
        let exchanges_conn = self.write_exchanges_conn.clone();

        tokio::task::spawn_blocking(move || {
            let mut report = RetentionReport::default();

            if let Some(cutoff) = policy.calls_before {
                let ts = timestamp_value(cutoff)?;
                let conn = calls_conn
                    .lock()
                    .map_err(|e| AppError::Storage(format!("failed to lock calls writer: {e}")))?;
                let n = conn
                    .execute(
                        "DELETE FROM llm_calls WHERE request_time < ?1",
                        duckdb::params![ts],
                    )
                    .map_err(|e| AppError::Storage(format!("failed to delete llm_calls: {e}")))?;
                report.calls_deleted = n as u64;
            }

            if let Some(cutoff) = policy.http_exchanges_before {
                let ts = timestamp_value(cutoff)?;
                let conn = exchanges_conn.lock().map_err(|e| {
                    AppError::Storage(format!("failed to lock exchanges writer: {e}"))
                })?;
                let n = conn
                    .execute(
                        "DELETE FROM http_exchanges WHERE request_time < ?1",
                        duckdb::params![ts],
                    )
                    .map_err(|e| {
                        AppError::Storage(format!("failed to delete http_exchanges: {e}"))
                    })?;
                report.http_exchanges_deleted = n as u64;
            }

            if let Some(cutoff) = policy.turns_before {
                let ts = timestamp_value(cutoff)?;
                let conn = turns_conn
                    .lock()
                    .map_err(|e| AppError::Storage(format!("failed to lock turns writer: {e}")))?;
                let n = conn
                    .execute(
                        "DELETE FROM agent_turns WHERE end_time < ?1",
                        duckdb::params![ts],
                    )
                    .map_err(|e| AppError::Storage(format!("failed to delete agent_turns: {e}")))?;
                report.turns_deleted = n as u64;
            }

            for (label, cutoff) in &policy.metrics_before {
                let ts = timestamp_value(*cutoff)?;
                let conn = metrics_conn.lock().map_err(|e| {
                    AppError::Storage(format!("failed to lock metrics writer: {e}"))
                })?;
                let n = conn
                    .execute(
                        "DELETE FROM llm_metrics WHERE granularity = ?1 AND timestamp < ?2",
                        duckdb::params![label, ts],
                    )
                    .map_err(|e| {
                        AppError::Storage(format!("failed to delete llm_metrics[{label}]: {e}"))
                    })?;
                // Mirror the sweep on the long-format finish-reason table so
                // the two stay in lock-step (same writer connection / same
                // (granularity, timestamp) cutoff).
                conn.execute(
                    "DELETE FROM llm_finish_metrics WHERE granularity = ?1 AND timestamp < ?2",
                    duckdb::params![label, ts],
                )
                .map_err(|e| {
                    AppError::Storage(format!("failed to delete llm_finish_metrics[{label}]: {e}"))
                })?;
                report.metrics_deleted.insert(label.clone(), n as u64);
            }

            // DuckDB DELETEs create MVCC tombstones; CHECKPOINT is what
            // actually shrinks the on-disk file. Skip if nothing was deleted.
            if report.total() > 0 {
                let conn = calls_conn.lock().map_err(|e| {
                    AppError::Storage(format!("failed to lock writer for checkpoint: {e}"))
                })?;
                conn.execute_batch("CHECKPOINT")
                    .map_err(|e| AppError::Storage(format!("checkpoint failed: {e}")))?;
            }

            Ok(report)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
}

/// Convert a `SystemTime` into a DuckDB microsecond-precision timestamp value.
fn timestamp_value(t: SystemTime) -> Result<Value> {
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| AppError::Storage(format!("retention cutoff before UNIX epoch: {e}")))?;
    let micros = i64::try_from(dur.as_micros())
        .map_err(|_| AppError::Storage("retention cutoff out of i64 range".to_string()))?;
    Ok(Value::Timestamp(TimeUnit::Microsecond, micros))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageBackend;
    use std::net::IpAddr;
    use ts_llm::model::ApiType;

    fn in_memory_backend() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
    }

    #[tokio::test]
    async fn http_exchange_round_trip() {
        use bytes::Bytes;
        use std::sync::Arc;
        use ts_protocol::model::{HttpRequestData, HttpResponseData};
        use ts_protocol::net::FlowKey;
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        let client_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let server_ip: IpAddr = "10.0.0.2".parse().unwrap();
        let request = Arc::new(HttpRequestData {
            flow_key: FlowKey::new("source-x".into(), client_ip, 54321, server_ip, 443),
            client_addr: (client_ip, 54321),
            server_addr: (server_ip, 443),
            method: "POST".into(),
            uri: "/v1/chat/completions".into(),
            version: 1,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from_static(br#"{"model":"gpt-4"}"#),
            timestamp_us: 1_700_000_000_000_000,
        });
        let response = Arc::new(HttpResponseData {
            flow_key: request.flow_key.clone(),
            client_addr: request.client_addr,
            server_addr: request.server_addr,
            status: 200,
            version: 1,
            headers: vec![("x-request-id".into(), "req_abc".into())],
            body: Bytes::from_static(br#"{"choices":[]}"#),
            first_byte_timestamp_us: 1_700_000_000_500_000,
            complete_timestamp_us: 1_700_000_001_000_000,
        });
        let exchange = ts_protocol::HttpExchange {
            id: "xchg-rt-1".to_string(),
            request,
            response,
            sse_event_count: 0,
            sse_data_bytes: 0,
        };
        backend
            .write_exchanges(vec![exchange.clone()])
            .await
            .unwrap();
        let got = backend
            .query_http_exchange_by_id("xchg-rt-1")
            .await
            .unwrap()
            .expect("round-tripped exchange");
        assert_eq!(got.id, "xchg-rt-1");
        assert_eq!(got.client_port, 54321);
        assert_eq!(got.method, "POST");
        assert_eq!(got.status, Some(200));
        assert!(!got.is_sse);
        assert_eq!(got.request_body.as_deref(), Some(r#"{"model":"gpt-4"}"#));
        assert_eq!(got.response_body.as_deref(), Some(r#"{"choices":[]}"#));
    }

    #[tokio::test]
    async fn http_exchange_sse_round_trip_response_body_none() {
        use bytes::Bytes;
        use std::sync::Arc;
        use ts_protocol::model::{HttpRequestData, HttpResponseData};
        use ts_protocol::net::FlowKey;
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        let client_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let server_ip: IpAddr = "10.0.0.2".parse().unwrap();
        let request = Arc::new(HttpRequestData {
            flow_key: FlowKey::new("source-sse".into(), client_ip, 1, server_ip, 443),
            client_addr: (client_ip, 1),
            server_addr: (server_ip, 443),
            method: "POST".into(),
            uri: "/v1/messages".into(),
            version: 1,
            headers: vec![],
            body: Bytes::new(),
            timestamp_us: 1,
        });
        let response = Arc::new(HttpResponseData {
            flow_key: request.flow_key.clone(),
            client_addr: request.client_addr,
            server_addr: request.server_addr,
            status: 200,
            version: 1,
            // text/event-stream content-type drives is_sse() = true, which
            // makes `stored_response_body()` return None regardless of the
            // parser-emitted empty `body`.
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: Bytes::new(),
            first_byte_timestamp_us: 2,
            complete_timestamp_us: 3,
        });
        let exchange = ts_protocol::HttpExchange {
            id: "xchg-sse-1".to_string(),
            request,
            response,
            sse_event_count: 3,
            sse_data_bytes: 42,
        };
        backend.write_exchanges(vec![exchange]).await.unwrap();
        let got = backend
            .query_http_exchange_by_id("xchg-sse-1")
            .await
            .unwrap()
            .unwrap();
        assert!(got.is_sse);
        assert!(got.response_body.is_none());
        assert_eq!(got.sse_event_count, 3);
        assert_eq!(got.sse_data_bytes, 42);
    }

    #[tokio::test]
    async fn http_exchange_missing_id_returns_none() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        let got = backend.query_http_exchange_by_id("nope").await.unwrap();
        assert!(got.is_none());
    }

    fn sample_call() -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "01912345-6789-7abc-def0-123456789abc".to_string(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            request_time: 1_700_000_000_000_000,
            response_time: Some(1_700_000_000_500_000),
            complete_time: Some(1_700_000_001_000_000),
            request_path: "/v1/chat/completions".to_string(),
            is_stream: true,
            request_body: Some(r#"{"model":"gpt-4"}"#.to_string()),
            status_code: Some(200),
            finish_reason: Some("stop".to_string()),
            response_body: Some(r#"{"choices":[...]}"#.to_string()),
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: Some(500.0),
            e2e_latency_ms: Some(1000.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 54321,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 8080,
            response_id: Some("chatcmpl-test123".to_string()),
            request_headers: vec![
                ("authorization".to_string(), "Bearer sk-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            response_headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-request-id".to_string(), "req_abc123".to_string()),
            ],
        }
    }

    #[tokio::test]
    async fn test_write_calls_single() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, model, is_stream, input_tokens FROM llm_calls")
            .unwrap();
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, "01912345-6789-7abc-def0-123456789abc");
        assert_eq!(row.1, "gpt-4");
        assert!(row.2);
        assert_eq!(row.3, Some(100));
    }

    #[tokio::test]
    async fn test_write_calls_new_fields() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT response_id, request_headers, response_headers FROM llm_calls")
            .unwrap();
        let (resp_id, req_hdr, resp_hdr) = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        assert_eq!(resp_id.as_deref(), Some("chatcmpl-test123"));
        // Verify headers are stored as JSON array of pairs
        let req_parsed: serde_json::Value = serde_json::from_str(&req_hdr).unwrap();
        assert!(req_parsed.is_array());
        assert_eq!(req_parsed[0][0], "authorization");
        assert_eq!(req_parsed[0][1], "Bearer sk-test");
        let resp_parsed: serde_json::Value = serde_json::from_str(&resp_hdr).unwrap();
        assert_eq!(resp_parsed[1][0], "x-request-id");
        assert_eq!(resp_parsed[1][1], "req_abc123");
    }

    #[tokio::test]
    async fn test_write_calls_id_present() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM llm_calls").unwrap();
        let id: String = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(id, "01912345-6789-7abc-def0-123456789abc");
    }

    #[tokio::test]
    async fn test_write_calls_empty_batch() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        backend.write_calls(vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn test_init_creates_tables() {
        let backend = in_memory_backend();
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
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        backend.init().await.unwrap();
    }

    fn sample_metric() -> LlmMetric {
        LlmMetric {
            timestamp_us: 1_700_000_000_000_000,
            source_id: String::new(),
            granularity: "1m",
            wire_api: wa::OPENAI_CHAT.to_string(),
            model: "gpt-4".to_string(),
            server_ip: "10.0.0.2".to_string(),
            call_count: 42,
            stream_count: 30,
            non_stream_count: 12,
            // active calls avg 3.5 → sum 147 across 42 samples.
            active_calls_sum: 147,
            active_calls_sample_count: 42,
            active_calls_max: 8,
            total_input_tokens: 10000,
            input_token_count: 42,
            total_output_tokens: 5000,
            output_token_count: 42,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            error_count: 2,
            error_4xx_count: 1,
            error_429_count: 0,
            error_5xx_count: 1,
            // ttft_avg 150 × 42 = 6300.
            ttft_sum: 6300.0,
            ttft_count: 42,
            ttft_p50: Some(120.0),
            ttft_p95: Some(350.0),
            ttft_p99: Some(500.0),
            // e2e_avg 1200 × 42 = 50400.
            e2e_sum: 50_400.0,
            e2e_count: 42,
            e2e_p50: Some(1000.0),
            e2e_p95: Some(2500.0),
            e2e_p99: Some(4000.0),
            // tpot_avg 22.2 × 30 streaming = 666.
            tpot_sum: 666.0,
            tpot_count: 30,
            tpot_p50: Some(23.8),
            tpot_p95: Some(12.5),
            tpot_p99: Some(8.3),
        }
    }

    #[tokio::test]
    async fn test_write_metrics_single() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let metric = sample_metric();
        backend.write_metrics(vec![metric]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT granularity, model, call_count, ttft_p50 FROM llm_metrics")
            .unwrap();
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<f64>>(3)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, "1m");
        assert_eq!(row.1, "gpt-4");
        assert_eq!(row.2, 42);
        assert_eq!(row.3, Some(120.0));
    }

    #[tokio::test]
    async fn test_write_metrics_empty_batch() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();
        backend.write_metrics(vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn test_write_metrics_new_columns() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let metric = sample_metric();
        backend.write_metrics(vec![metric]).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT total_output_tokens, output_token_count, tpot_sum, tpot_count \
                 FROM llm_metrics",
            )
            .unwrap();
        let row = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, 5000);
        assert_eq!(row.1, 42);
        // tpot_sum 666 / tpot_count 30 = 22.2
        assert!((row.2 - 666.0).abs() < 1e-6);
        assert_eq!(row.3, 30);
    }

    #[tokio::test]
    async fn test_write_finish_metrics_round_trip() {
        // Phase 4 long-format finish-reason table. Inserts mixed raw provider
        // values and verifies that the row count, key columns, and per-reason
        // counts round-trip without any normalization.
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let base = LlmFinishMetric {
            timestamp_us: 1_700_000_000_000_000,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: wa::OPENAI_CHAT.to_string(),
            model: "gpt-4".to_string(),
            server_ip: "10.0.0.2".to_string(),
            finish_reason: String::new(),
            count: 0,
        };
        let rows = vec![
            LlmFinishMetric {
                finish_reason: "stop".into(),
                count: 35,
                ..base.clone()
            },
            LlmFinishMetric {
                finish_reason: "length".into(),
                count: 3,
                ..base.clone()
            },
            LlmFinishMetric {
                finish_reason: "tool_calls".into(),
                count: 2,
                ..base.clone()
            },
            // Unknown / future provider value preserved verbatim.
            LlmFinishMetric {
                finish_reason: "pause_turn".into(),
                count: 1,
                ..base
            },
        ];
        backend.write_finish_metrics(rows).await.unwrap();

        let conn = backend.test_conn().lock().unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM llm_finish_metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 4);

        let stop_count: u64 = conn
            .query_row(
                "SELECT count FROM llm_finish_metrics WHERE finish_reason = 'stop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stop_count, 35);

        let pause_count: u64 = conn
            .query_row(
                "SELECT count FROM llm_finish_metrics WHERE finish_reason = 'pause_turn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pause_count, 1);
    }

    #[tokio::test]
    async fn query_finish_reasons_groups_by_raw_value() {
        // Phase 5: long-format read path. Two timestamps × three raw provider
        // finish_reason values, written at the (*, *, *) tier so the default
        // (no wire_api / no model filter) read picks them up.
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let bucket_a: i64 = 1_700_000_000_000_000;
        let bucket_b: i64 = 1_700_000_060_000_000; // +60s, next 1m bucket
        let mk = |ts_us: i64, reason: &str, count: u64| LlmFinishMetric {
            timestamp_us: ts_us,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: "*".to_string(),
            model: "*".to_string(),
            server_ip: "*".to_string(),
            finish_reason: reason.to_string(),
            count,
        };

        backend
            .write_finish_metrics(vec![
                mk(bucket_a, "end_turn", 12),
                mk(bucket_a, "tool_use", 4),
                mk(bucket_a, "max_tokens", 1),
                mk(bucket_b, "end_turn", 7),
                mk(bucket_b, "pause_turn", 2),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: bucket_a - 1,
                end_us: bucket_b + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: Vec::new(),
            models: Vec::new(),
            server_ips: Vec::new(),
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        // One series per distinct raw value; alphabetical by finish_reason.
        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(
            names,
            vec!["end_turn", "max_tokens", "pause_turn", "tool_use"]
        );

        let end_turn = series
            .iter()
            .find(|s| s.finish_reason == "end_turn")
            .unwrap();
        assert_eq!(end_turn.points, vec![(bucket_a, 12), (bucket_b, 7)]);

        let pause_turn = series
            .iter()
            .find(|s| s.finish_reason == "pause_turn")
            .unwrap();
        assert_eq!(pause_turn.points, vec![(bucket_b, 2)]);

        let max_tokens = series
            .iter()
            .find(|s| s.finish_reason == "max_tokens")
            .unwrap();
        assert_eq!(max_tokens.points, vec![(bucket_a, 1)]);
    }

    #[tokio::test]
    async fn query_finish_reasons_filters_by_wire_api() {
        // With `wire_api = Some("openai_chat")` and no model filter, the read
        // sums per-model rows at the (W, M, *) tier.
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts: i64 = 1_700_000_000_000_000;
        let mk = |wire: &str, model: &str, reason: &str, count: u64| LlmFinishMetric {
            timestamp_us: ts,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: wire.to_string(),
            model: model.to_string(),
            server_ip: "*".to_string(),
            finish_reason: reason.to_string(),
            count,
        };

        backend
            .write_finish_metrics(vec![
                mk(wa::OPENAI_CHAT, "gpt-4", "stop", 5),
                mk(wa::OPENAI_CHAT, "gpt-4o", "stop", 2),
                mk(wa::OPENAI_CHAT, "gpt-4", "length", 1),
                mk(wa::ANTHROPIC, "claude-3", "end_turn", 9),
                // Fully-rolled-up tier for the same window — must be excluded
                // by the read (server_ip='*' AND wire_api filter).
                mk("*", "*", "stop", 99),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: ts - 1,
                end_us: ts + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: vec![wa::OPENAI_CHAT.to_string()],
            models: Vec::new(),
            server_ips: Vec::new(),
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        // Only openai_chat finish reasons; counts summed across models.
        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(names, vec!["length", "stop"]);
        let stop = series.iter().find(|s| s.finish_reason == "stop").unwrap();
        assert_eq!(stop.points, vec![(ts, 7)]); // 5 + 2
        let length = series.iter().find(|s| s.finish_reason == "length").unwrap();
        assert_eq!(length.points, vec![(ts, 1)]);
    }

    #[tokio::test]
    async fn query_finish_reasons_filters_by_multi_wire_api() {
        // With `wire_apis = ["openai_chat", "anthropic"]` (CSV expansion at the
        // API layer), the read sums per-model rows at the (W, M, *) tier across
        // all listed wire_apis — same finish_reason in different wire_apis
        // collapses into a single series.
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts: i64 = 1_700_000_000_000_000;
        let mk = |wire: &str, model: &str, reason: &str, count: u64| LlmFinishMetric {
            timestamp_us: ts,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: wire.to_string(),
            model: model.to_string(),
            server_ip: "*".to_string(),
            finish_reason: reason.to_string(),
            count,
        };

        backend
            .write_finish_metrics(vec![
                mk(wa::OPENAI_CHAT, "gpt-4", "stop", 5),
                mk(wa::OPENAI_CHAT, "gpt-4o", "stop", 2),
                mk(wa::ANTHROPIC, "claude-3", "stop", 3),
                mk(wa::ANTHROPIC, "claude-3", "end_turn", 9),
                // A wire_api outside the filter must NOT contribute.
                mk("gemini", "gemini-pro", "stop", 100),
                // Fully-rolled-up tier — must be excluded by server_ip='*' AND
                // the wire_api IN-list filter.
                mk("*", "*", "stop", 99),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: ts - 1,
                end_us: ts + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: vec![wa::OPENAI_CHAT.to_string(), wa::ANTHROPIC.to_string()],
            models: Vec::new(),
            server_ips: Vec::new(),
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(names, vec!["end_turn", "stop"]);
        // stop sums across both wire_apis and their models: 5 + 2 + 3 = 10.
        let stop = series.iter().find(|s| s.finish_reason == "stop").unwrap();
        assert_eq!(stop.points, vec![(ts, 10)]);
        let end_turn = series
            .iter()
            .find(|s| s.finish_reason == "end_turn")
            .unwrap();
        assert_eq!(end_turn.points, vec![(ts, 9)]);
    }

    #[tokio::test]
    async fn query_finish_reasons_filters_by_server_ip() {
        // With `server_ips = ["10.0.0.1"]` and no wire/model filter, the read
        // lands on the (*, *, S) tier and SUMs only the listed servers.
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts: i64 = 1_700_000_000_000_000;
        let mk = |server: &str, reason: &str, count: u64| LlmFinishMetric {
            timestamp_us: ts,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: "*".to_string(),
            model: "*".to_string(),
            server_ip: server.to_string(),
            finish_reason: reason.to_string(),
            count,
        };

        backend
            .write_finish_metrics(vec![
                mk("10.0.0.1", "end_turn", 5),
                mk("10.0.0.1", "tool_use", 2),
                mk("10.0.0.2", "end_turn", 7),
                // Cross-server rollup tier — must be excluded by the IN-list.
                mk("*", "end_turn", 99),
            ])
            .await
            .unwrap();

        let q = FinishReasonsQuery {
            time_range: TimeRange {
                start_us: ts - 1,
                end_us: ts + 1_000_000,
            },
            granularity: "1m".to_string(),
            wire_apis: Vec::new(),
            models: Vec::new(),
            server_ips: vec!["10.0.0.1".to_string()],
        };
        let series = backend.query_finish_reasons(&q).await.unwrap();

        let names: Vec<&str> = series.iter().map(|s| s.finish_reason.as_str()).collect();
        assert_eq!(names, vec!["end_turn", "tool_use"]);
        let end_turn = series
            .iter()
            .find(|s| s.finish_reason == "end_turn")
            .unwrap();
        assert_eq!(end_turn.points, vec![(ts, 5)]);
        let tool_use = series
            .iter()
            .find(|s| s.finish_reason == "tool_use")
            .unwrap();
        assert_eq!(tool_use.points, vec![(ts, 2)]);
    }

    // ===== Task 3: query_distinct_* tests =====

    #[tokio::test]
    async fn test_query_distinct_wire_apis() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        // Write metrics with wire APIs "openai-chat", "anthropic", and "*"
        let mut m1 = sample_metric();
        m1.wire_api = wa::OPENAI_CHAT.to_string();
        m1.model = "gpt-4".to_string();
        m1.server_ip = "10.0.0.1".to_string();

        let mut m2 = sample_metric();
        m2.wire_api = wa::ANTHROPIC.to_string();
        m2.model = "claude-3".to_string();
        m2.server_ip = "10.0.0.1".to_string();

        let mut m3 = sample_metric();
        m3.wire_api = "*".to_string();
        m3.model = "*".to_string();
        m3.server_ip = "*".to_string();

        backend.write_metrics(vec![m1, m2, m3]).await.unwrap();

        let wire_apis = backend.query_distinct_wire_apis().await.unwrap();
        assert_eq!(wire_apis, vec![wa::ANTHROPIC, wa::OPENAI_CHAT]);
    }

    #[tokio::test]
    async fn test_query_distinct_models() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let mut m1 = sample_metric();
        m1.wire_api = wa::OPENAI_CHAT.to_string();
        m1.model = "gpt-4".to_string();
        m1.server_ip = "10.0.0.1".to_string();

        let mut m2 = sample_metric();
        m2.wire_api = wa::OPENAI_CHAT.to_string();
        m2.model = "gpt-3.5".to_string();
        m2.server_ip = "10.0.0.1".to_string();

        let mut m3 = sample_metric();
        m3.wire_api = "*".to_string();
        m3.model = "*".to_string();
        m3.server_ip = "*".to_string();

        backend.write_metrics(vec![m1, m2, m3]).await.unwrap();

        let models = backend.query_distinct_models().await.unwrap();
        assert_eq!(models, vec!["gpt-3.5", "gpt-4"]);
    }

    #[tokio::test]
    async fn test_query_distinct_server_ips() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let mut m1 = sample_metric();
        m1.wire_api = wa::OPENAI_CHAT.to_string();
        m1.model = "gpt-4".to_string();
        m1.server_ip = "10.0.0.1".to_string();

        let mut m2 = sample_metric();
        m2.wire_api = wa::OPENAI_CHAT.to_string();
        m2.model = "gpt-4".to_string();
        m2.server_ip = "10.0.0.2".to_string();

        let mut m3 = sample_metric();
        m3.wire_api = "*".to_string();
        m3.model = "*".to_string();
        m3.server_ip = "*".to_string();

        backend.write_metrics(vec![m1, m2, m3]).await.unwrap();

        let server_ips = backend.query_distinct_server_ips().await.unwrap();
        assert_eq!(server_ips, vec!["10.0.0.1", "10.0.0.2"]);
    }

    #[tokio::test]
    async fn query_distinct_finish_reasons_returns_pairs() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts: i64 = 1_700_000_000_000_000;
        let mk = |wire: &str, reason: &str| LlmFinishMetric {
            timestamp_us: ts,
            source_id: String::new(),
            granularity: "1m".to_string(),
            wire_api: wire.to_string(),
            model: "m".to_string(),
            server_ip: "*".to_string(),
            finish_reason: reason.to_string(),
            count: 1,
        };

        backend
            .write_finish_metrics(vec![
                mk(wa::ANTHROPIC, "end_turn"),
                mk(wa::ANTHROPIC, "tool_use"),
                mk(wa::ANTHROPIC, "end_turn"), // duplicate — DISTINCT collapses
                mk(wa::OPENAI_CHAT, "stop"),
                mk(wa::OPENAI_CHAT, "tool_calls"),
                // Cross-wire-api rollup tier — must be excluded.
                mk("*", "stop"),
            ])
            .await
            .unwrap();

        let pairs = backend.query_distinct_finish_reasons().await.unwrap();
        let as_tuples: Vec<(&str, &str)> = pairs
            .iter()
            .map(|p| (p.wire_api.as_str(), p.finish_reason.as_str()))
            .collect();
        // Sorted by (wire_api, finish_reason) ascending — alphabetical so
        // anthropic comes before openai-chat.
        assert_eq!(
            as_tuples,
            vec![
                (wa::ANTHROPIC, "end_turn"),
                (wa::ANTHROPIC, "tool_use"),
                (wa::OPENAI_CHAT, "stop"),
                (wa::OPENAI_CHAT, "tool_calls"),
            ]
        );
    }

    // ===== Task 4: query_metrics_timeseries tests =====

    #[tokio::test]
    async fn test_query_metrics_timeseries_basic() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        // Two global wildcard metrics at different timestamps
        let mut m1 = sample_metric();
        m1.timestamp_us = 1_700_000_000_000_000;
        m1.granularity = "1m";
        m1.wire_api = "*".to_string();
        m1.model = "*".to_string();
        m1.server_ip = "*".to_string();
        m1.ttft_p50 = Some(100.0);
        m1.ttft_p95 = Some(200.0);

        let mut m2 = sample_metric();
        m2.timestamp_us = 1_700_000_060_000_000; // +60s
        m2.granularity = "1m";
        m2.wire_api = "*".to_string();
        m2.model = "*".to_string();
        m2.server_ip = "*".to_string();
        m2.ttft_p50 = Some(150.0);
        m2.ttft_p95 = Some(300.0);

        backend.write_metrics(vec![m1, m2]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: 1_700_000_000_000_000,
                end_us: 1_700_000_120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["ttft_p50".to_string(), "ttft_p95".to_string()],
            group_by: None,
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].group.is_none());
        assert_eq!(rows[0].values[0], Some(100.0));
        assert_eq!(rows[0].values[1], Some(200.0));
        assert_eq!(rows[1].values[0], Some(150.0));
        assert_eq!(rows[1].values[1], Some(300.0));
    }

    #[tokio::test]
    async fn test_query_metrics_timeseries_group_by_wire_api() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        // Per-model rows: (wire_api, model, server_ip='*')
        // These are what the aggregator actually produces. group_by=wire_api
        // should SUM across models within each wire_api.
        let mut m = sample_metric();
        m.timestamp_us = ts;
        m.granularity = "1m";
        m.server_ip = "*".to_string();

        m.wire_api = wa::OPENAI_CHAT.to_string();
        m.model = "gpt-4".to_string();
        m.call_count = 200;
        backend.write_metrics(vec![m.clone()]).await.unwrap();

        m.model = "gpt-3.5".to_string();
        m.call_count = 100;
        backend.write_metrics(vec![m.clone()]).await.unwrap();

        m.wire_api = wa::ANTHROPIC.to_string();
        m.model = "claude-3".to_string();
        m.call_count = 50;
        backend.write_metrics(vec![m]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["call_count".to_string()],
            group_by: Some("wire_api".to_string()),
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        // Should have 2 rows: anthropic and openai (aggregated across models)
        assert_eq!(rows.len(), 2);
        let anthropic_row = rows
            .iter()
            .find(|r| r.group.as_deref() == Some(wa::ANTHROPIC))
            .unwrap();
        let openai_row = rows
            .iter()
            .find(|r| r.group.as_deref() == Some(wa::OPENAI_CHAT))
            .unwrap();
        assert_eq!(anthropic_row.values[0], Some(50.0));
        assert_eq!(openai_row.values[0], Some(300.0)); // 200 + 100
    }

    // With per-source aggregators, the sink receives one row per (source_id,
    // ts, dim). The ungrouped timeseries query MUST GROUP BY timestamp so
    // the caller sees one point per timestamp (call_count summed, ttft
    // weighted-averaged by call_count). Before this fix the branch had
    // no GROUP BY and returned N overlapping rows per timestamp.
    #[tokio::test]
    async fn test_multi_source_ungrouped_timeseries_merges() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut source0 = sample_metric();
        source0.timestamp_us = ts;
        source0.source_id = "s0".into();
        source0.granularity = "1m";
        source0.wire_api = "*".into();
        source0.model = "*".into();
        source0.server_ip = "*".into();
        source0.call_count = 10;
        source0.ttft_count = 10;
        source0.ttft_p50 = Some(100.0);
        source0.error_count = 1;

        let mut source1 = sample_metric();
        source1.timestamp_us = ts;
        source1.source_id = "s1".into();
        source1.granularity = "1m";
        source1.wire_api = "*".into();
        source1.model = "*".into();
        source1.server_ip = "*".into();
        source1.call_count = 30;
        source1.ttft_count = 30;
        source1.ttft_p50 = Some(200.0);
        source1.error_count = 3;

        backend.write_metrics(vec![source0, source1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec![
                "call_count".to_string(),
                "ttft_p50".to_string(),
                "error_count".to_string(),
            ],
            group_by: None,
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "ungrouped query must return 1 row per timestamp across sources, got {}",
            rows.len()
        );
        assert_eq!(rows[0].values[0], Some(40.0), "call_count SUM = 10 + 30");
        // weighted avg by ttft_count: (100*10 + 200*30) / 40 = 175
        let p50 = rows[0].values[1].unwrap();
        assert!((p50 - 175.0).abs() < 0.01, "weighted p50 ≈ 175, got {p50}");
        assert_eq!(rows[0].values[2], Some(4.0), "error_count SUM = 1 + 3");
    }

    #[tokio::test]
    async fn test_multi_source_grouped_timeseries_merges() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut s0 = sample_metric();
        s0.timestamp_us = ts;
        s0.source_id = "s0".into();
        s0.granularity = "1m";
        s0.wire_api = wa::OPENAI_CHAT.into();
        s0.model = "gpt-4".into();
        s0.server_ip = "*".into();
        s0.call_count = 10;

        let mut s1 = sample_metric();
        s1.timestamp_us = ts;
        s1.source_id = "s1".into();
        s1.granularity = "1m";
        s1.wire_api = wa::OPENAI_CHAT.into();
        s1.model = "gpt-4".into();
        s1.server_ip = "*".into();
        s1.call_count = 40;

        backend.write_metrics(vec![s0, s1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["call_count".to_string()],
            group_by: Some("wire_api".to_string()),
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group.as_deref(), Some(wa::OPENAI_CHAT));
        assert_eq!(rows[0].values[0], Some(50.0), "grouped SUM across sources");
    }

    // ===== Task 5: query_metrics_summary tests =====

    #[tokio::test]
    async fn test_query_metrics_summary() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts1 = 1_700_000_000_000_000i64;
        let ts2 = ts1 + 10_000_000; // +10s

        let mut m1 = sample_metric();
        m1.timestamp_us = ts1;
        m1.granularity = "10s";
        m1.wire_api = "*".to_string();
        m1.model = "*".to_string();
        m1.server_ip = "*".to_string();
        m1.call_count = 100;
        m1.stream_count = 80;
        m1.error_count = 5;
        m1.error_4xx_count = 3;
        m1.error_429_count = 1;
        m1.error_5xx_count = 2;
        m1.total_input_tokens = 10_000;
        m1.total_output_tokens = 5_000;
        // ttft avg 100 over 100 samples → sum 10_000
        m1.ttft_sum = 10_000.0;
        m1.ttft_count = 100;
        m1.e2e_sum = 50_000.0;
        m1.e2e_count = 100;
        // tpot avg 40 over 80 streaming samples → sum 3200
        m1.tpot_sum = 3_200.0;
        m1.tpot_count = 80;

        let mut m2 = sample_metric();
        m2.timestamp_us = ts2;
        m2.granularity = "10s";
        m2.wire_api = "*".to_string();
        m2.model = "*".to_string();
        m2.server_ip = "*".to_string();
        m2.call_count = 200;
        m2.stream_count = 160;
        m2.error_count = 10;
        m2.error_4xx_count = 6;
        m2.error_429_count = 2;
        m2.error_5xx_count = 4;
        m2.total_input_tokens = 20_000;
        m2.total_output_tokens = 10_000;
        // ttft avg 200 over 200 samples → sum 40_000
        m2.ttft_sum = 40_000.0;
        m2.ttft_count = 200;
        m2.e2e_sum = 200_000.0;
        m2.e2e_count = 200;
        // tpot avg 60 over 160 streaming samples → sum 9600
        m2.tpot_sum = 9_600.0;
        m2.tpot_count = 160;

        backend.write_metrics(vec![m1, m2]).await.unwrap();

        let query = MetricsSummaryQuery {
            time_range: TimeRange {
                start_us: ts1,
                end_us: ts2 + 10_000_000,
            },
            filter: DimensionFilter::default(),
        };

        let summary = backend.query_metrics_summary(&query).await.unwrap();
        assert_eq!(summary.call_count, 300);
        assert_eq!(summary.error_count, 15);
        assert_eq!(summary.error_4xx_count, 9);
        assert_eq!(summary.error_429_count, 3);
        assert_eq!(summary.error_5xx_count, 6);
        assert_eq!(summary.total_input_tokens, 30_000);
        assert_eq!(summary.total_output_tokens, 15_000);
        // Exact avg via sum+count: (10000 + 40000) / 300 = 166.666...
        let ttft_avg = summary.ttft_avg.unwrap();
        assert!(
            (ttft_avg - 500.0 / 3.0).abs() < 0.01,
            "expected ~166.67, got {ttft_avg}"
        );
        // tpot exact avg: (3200 + 9600) / 240 = 53.33
        let tpot_avg = summary.tpot_avg.unwrap();
        assert!(
            (tpot_avg - 160.0 / 3.0).abs() < 0.01,
            "expected ~53.33, got {tpot_avg}"
        );
    }

    // ===== Task 6: query_metrics_models tests =====

    #[tokio::test]
    async fn test_query_metrics_models() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut m_gpt4 = sample_metric();
        m_gpt4.timestamp_us = ts;
        m_gpt4.granularity = "10s";
        m_gpt4.wire_api = wa::OPENAI_CHAT.to_string();
        m_gpt4.model = "gpt-4".to_string();
        m_gpt4.server_ip = "*".to_string();
        m_gpt4.call_count = 100;
        m_gpt4.stream_count = 80;
        // ttft avg 150 over 100 → sum 15000
        m_gpt4.ttft_sum = 15_000.0;
        m_gpt4.ttft_count = 100;
        m_gpt4.ttft_p95 = Some(400.0);
        m_gpt4.e2e_sum = 100_000.0;
        m_gpt4.e2e_count = 100;
        m_gpt4.e2e_p95 = Some(3000.0);
        // tpot avg 20 over 80 → sum 1600
        m_gpt4.tpot_sum = 1_600.0;
        m_gpt4.tpot_count = 80;

        let mut m_claude = sample_metric();
        m_claude.timestamp_us = ts;
        m_claude.granularity = "10s";
        m_claude.wire_api = wa::ANTHROPIC.to_string();
        m_claude.model = "claude-3".to_string();
        m_claude.server_ip = "*".to_string();
        m_claude.call_count = 200;
        m_claude.stream_count = 150;
        // ttft avg 120 over 200 → sum 24000
        m_claude.ttft_sum = 24_000.0;
        m_claude.ttft_count = 200;
        m_claude.ttft_p95 = Some(300.0);
        m_claude.e2e_sum = 160_000.0;
        m_claude.e2e_count = 200;
        m_claude.e2e_p95 = Some(2000.0);
        // tpot avg 22 over 150 → sum 3300
        m_claude.tpot_sum = 3_300.0;
        m_claude.tpot_count = 150;

        backend.write_metrics(vec![m_gpt4, m_claude]).await.unwrap();

        let query = MetricsModelsQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 10_000_000,
            },
            filter: DimensionFilter::default(),
            sort_by: "call_count".to_string(),
            sort_order: "DESC".to_string(),
            limit: 10,
        };

        let rows = backend.query_metrics_models(&query).await.unwrap();
        assert_eq!(rows.len(), 2);
        // claude-3 should come first (200 > 100)
        assert_eq!(rows[0].wire_api, wa::ANTHROPIC);
        assert_eq!(rows[0].model, "claude-3");
        assert_eq!(rows[0].call_count, 200);
        assert_eq!(rows[1].wire_api, wa::OPENAI_CHAT);
        assert_eq!(rows[1].model, "gpt-4");
        assert_eq!(rows[1].call_count, 100);
    }

    // ===== Dimension filter WHERE-clause builder tests =====
    //
    // The aggregator emits 4 wildcard combinations per event: (W,M,S),
    // (W,M,*), (*,*,S), (*,*,*). These tests lock in the mapping from a
    // user filter set to the correct pre-aggregated tier.

    #[test]
    fn test_build_dimension_where_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where(&f),
            "wire_api = '*' AND model = '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_server_only() {
        let f = DimensionFilter {
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api = '*' AND model = '*' AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_only() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model != '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_model_only() {
        let f = DimensionFilter {
            models: vec!["gpt-4".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api != '*' AND model IN ('gpt-4') AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_and_model() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            models: vec!["gpt-4".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model IN ('gpt-4') AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_and_server() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_model_and_server() {
        let f = DimensionFilter {
            models: vec!["gpt-4".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api != '*' AND model IN ('gpt-4') AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_all_three() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            models: vec!["gpt-4".into()],
            server_ips: vec!["10.0.0.1".into()],
        };
        assert_eq!(
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model IN ('gpt-4') AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_for_group_wire_api_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where_for_group(&f, "wire_api"),
            "wire_api != '*' AND model != '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_for_group_with_server_filter() {
        let f = DimensionFilter {
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where_for_group(&f, "wire_api"),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
        assert_eq!(
            build_dimension_where_for_group(&f, "model"),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
    }

    // ===== Integration: filters actually narrow the returned data =====

    #[tokio::test]
    async fn test_query_metrics_summary_wire_api_filter() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        // (W, M, *) tier rows — two wire_apis.
        let mut openai_row = sample_metric();
        openai_row.timestamp_us = ts;
        openai_row.granularity = "10s";
        openai_row.wire_api = wa::OPENAI_CHAT.into();
        openai_row.model = "gpt-4".into();
        openai_row.server_ip = "*".into();
        openai_row.call_count = 100;

        let mut anthropic_row = sample_metric();
        anthropic_row.timestamp_us = ts;
        anthropic_row.granularity = "10s";
        anthropic_row.wire_api = wa::ANTHROPIC.into();
        anthropic_row.model = "claude-3".into();
        anthropic_row.server_ip = "*".into();
        anthropic_row.call_count = 200;

        // (*, *, *) tier row — must NOT be counted when a wire_api filter is
        // applied (otherwise we'd double-count).
        let mut total_row = sample_metric();
        total_row.timestamp_us = ts;
        total_row.granularity = "10s";
        total_row.wire_api = "*".into();
        total_row.model = "*".into();
        total_row.server_ip = "*".into();
        total_row.call_count = 300;

        backend
            .write_metrics(vec![openai_row, anthropic_row, total_row])
            .await
            .unwrap();

        let query = MetricsSummaryQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 10_000_000,
            },
            filter: DimensionFilter {
                wire_apis: vec![wa::OPENAI_CHAT.into()],
                ..Default::default()
            },
        };
        let summary = backend.query_metrics_summary(&query).await.unwrap();
        assert_eq!(
            summary.call_count, 100,
            "filter should return only the openai row"
        );
    }

    #[tokio::test]
    async fn test_query_metrics_models_wire_api_filter() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut gpt4 = sample_metric();
        gpt4.timestamp_us = ts;
        gpt4.granularity = "10s";
        gpt4.wire_api = wa::OPENAI_CHAT.into();
        gpt4.model = "gpt-4".into();
        gpt4.server_ip = "*".into();
        gpt4.call_count = 100;

        let mut claude = sample_metric();
        claude.timestamp_us = ts;
        claude.granularity = "10s";
        claude.wire_api = wa::ANTHROPIC.into();
        claude.model = "claude-3".into();
        claude.server_ip = "*".into();
        claude.call_count = 200;

        backend.write_metrics(vec![gpt4, claude]).await.unwrap();

        let query = MetricsModelsQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 10_000_000,
            },
            filter: DimensionFilter {
                wire_apis: vec![wa::OPENAI_CHAT.into()],
                ..Default::default()
            },
            sort_by: "call_count".into(),
            sort_order: "DESC".into(),
            limit: 10,
        };
        let rows = backend.query_metrics_models(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].wire_api, wa::OPENAI_CHAT);
        assert_eq!(rows[0].model, "gpt-4");
    }

    #[tokio::test]
    async fn test_query_metrics_timeseries_wire_api_filter_ungrouped() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        // (W, M, *) tier — two wire_apis worth of rows at the same timestamp.
        let mut gpt4 = sample_metric();
        gpt4.timestamp_us = ts;
        gpt4.granularity = "1m";
        gpt4.wire_api = wa::OPENAI_CHAT.into();
        gpt4.model = "gpt-4".into();
        gpt4.server_ip = "*".into();
        gpt4.call_count = 100;

        let mut claude = sample_metric();
        claude.timestamp_us = ts;
        claude.granularity = "1m";
        claude.wire_api = wa::ANTHROPIC.into();
        claude.model = "claude-3".into();
        claude.server_ip = "*".into();
        claude.call_count = 200;

        // (*, *, *) tier row must not be included alongside the filter.
        let mut total_row = sample_metric();
        total_row.timestamp_us = ts;
        total_row.granularity = "1m";
        total_row.wire_api = "*".into();
        total_row.model = "*".into();
        total_row.server_ip = "*".into();
        total_row.call_count = 300;

        backend
            .write_metrics(vec![gpt4, claude, total_row])
            .await
            .unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".into(),
            filter: DimensionFilter {
                wire_apis: vec![wa::OPENAI_CHAT.into()],
                ..Default::default()
            },
            fields: vec!["call_count".into()],
            group_by: None,
        };
        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Some(100.0));
    }

    // ===== Task 7: query_calls and query_call_by_id tests =====

    #[tokio::test]
    async fn test_query_calls_basic() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let call = sample_call();
        let call_time = call.request_time;
        backend.write_calls(vec![call]).await.unwrap();

        let query = CallsQuery {
            time_range: TimeRange {
                start_us: call_time - 1,
                end_us: call_time + 1_000_000,
            },
            filter: DimensionFilter::default(),
            status_codes: vec![],
            finish_reasons: vec![],
            client_ips: vec![],
            request_path_contains: None,
            sort_by: "request_time".to_string(),
            sort_order: "DESC".to_string(),
            page: 1,
            page_size: 10,
        };

        let page = backend.query_calls(&query).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "01912345-6789-7abc-def0-123456789abc");
        assert_eq!(page.items[0].model, "gpt-4");
        assert_eq!(page.items[0].status_code, Some(200));
        assert_eq!(page.items[0].input_tokens, Some(100));
        assert_eq!(page.items[0].output_tokens, Some(50));
    }

    #[tokio::test]
    async fn test_query_calls_filter_status_code() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let mut call_200 = sample_call();
        call_200.id = "call-200".to_string();
        call_200.status_code = Some(200);

        let mut call_429 = sample_call();
        call_429.id = "call-429".to_string();
        call_429.status_code = Some(429);

        let call_time = call_200.request_time;
        backend.write_calls(vec![call_200, call_429]).await.unwrap();

        let query = CallsQuery {
            time_range: TimeRange {
                start_us: call_time - 1,
                end_us: call_time + 1_000_000,
            },
            filter: DimensionFilter::default(),
            status_codes: vec![429],
            finish_reasons: vec![],
            client_ips: vec![],
            request_path_contains: None,
            sort_by: "request_time".to_string(),
            sort_order: "DESC".to_string(),
            page: 1,
            page_size: 10,
        };

        let page = backend.query_calls(&query).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "call-429");
        assert_eq!(page.items[0].status_code, Some(429));
    }

    #[tokio::test]
    async fn test_query_call_by_id() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let call = sample_call();
        backend.write_calls(vec![call]).await.unwrap();

        // Query by existing id
        let detail = backend
            .query_call_by_id("01912345-6789-7abc-def0-123456789abc")
            .await
            .unwrap();
        assert!(detail.is_some());
        let detail = detail.unwrap();
        assert_eq!(detail.id, "01912345-6789-7abc-def0-123456789abc");
        assert_eq!(detail.model, "gpt-4");
        assert_eq!(detail.wire_api, wa::OPENAI_CHAT);
        assert_eq!(detail.status_code, Some(200));
        assert_eq!(detail.input_tokens, Some(100));
        assert_eq!(detail.output_tokens, Some(50));
        assert_eq!(detail.total_tokens, Some(150));
        assert!(detail.request_body.is_some());
        assert!(detail.response_body.is_some());
        assert!(detail.request_headers.is_some());
        assert!(detail.response_headers.is_some());

        // Query nonexistent id
        let not_found = backend.query_call_by_id("does-not-exist").await.unwrap();
        assert!(not_found.is_none());
    }
}

#[cfg(test)]
mod turn_tests {
    use super::*;
    use std::net::IpAddr;
    use ts_llm::model::ApiType;
    use ts_turn::{AgentTurn, TurnStatus};

    fn sample_turn(
        turn_id: &str,
        session_id: &str,
        wire_api: &str,
        models_used: Vec<&str>,
        start_us: i64,
        duration_ms: u64,
        call_count: u32,
        call_ids: Vec<&str>,
        status: TurnStatus,
    ) -> AgentTurn {
        AgentTurn {
            source_id: String::new(),
            turn_id: turn_id.into(),
            session_id: session_id.into(),
            wire_api: wire_api.into(),
            agent_kind: "claude-cli".into(),
            start_time_us: start_us,
            end_time_us: start_us + (duration_ms as i64) * 1000,
            duration_ms,
            call_count,
            models_used: models_used.into_iter().map(String::from).collect(),
            subagents_used: vec![],
            total_input_tokens: 100,
            total_output_tokens: 50,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_cost_usd: None,
            status,
            final_finish_reason: Some("complete".into()),
            user_input_preview: Some("hello".into()),
            user_call_id: None,
            final_answer_preview: Some("world".into()),
            final_call_id: None,
            call_ids: call_ids.into_iter().map(String::from).collect(),
            metadata: serde_json::json!({}),
        }
    }

    fn mk_call_with_time(id: &str, request_time_us: i64) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: id.into(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".into(),
            api_type: ApiType::Chat,
            request_time: request_time_us,
            response_time: Some(request_time_us + 100_000),
            complete_time: Some(request_time_us + 500_000),
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
            ttft_ms: Some(100.0),
            e2e_latency_ms: Some(500.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 1000,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 8080,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        }
    }

    #[tokio::test]
    async fn round_trip_one_turn() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        let turn = sample_turn(
            "t1",
            "s1",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            1_700_000_000_000_000,
            1500,
            3,
            vec!["call-42"],
            TurnStatus::Complete,
        );
        backend.write_turns(vec![turn]).await.unwrap();
    }

    fn base_turns_query() -> TurnsQuery {
        TurnsQuery {
            time_range: TimeRange {
                start_us: 1_700_000_000_000_000 - 1,
                end_us: 1_800_000_000_000_000,
            },
            filter: DimensionFilter::default(),
            statuses: vec![],
            agent_kinds: vec![],
            sort_by: "start_time".into(),
            sort_order: "desc".into(),
            page: 1,
            page_size: 50,
        }
    }

    #[tokio::test]
    async fn query_turns_filters_and_paginates() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let base = 1_700_000_000_000_000_i64;
        let turns = vec![
            sample_turn(
                "t1",
                "s1",
                wa::OPENAI_CHAT,
                vec!["gpt-4"],
                base + 1_000_000,
                100,
                1,
                vec!["c1"],
                TurnStatus::Complete,
            ),
            sample_turn(
                "t2",
                "s1",
                wa::ANTHROPIC,
                vec!["claude-sonnet"],
                base + 2_000_000,
                200,
                2,
                vec!["c2", "c3"],
                TurnStatus::Complete,
            ),
            sample_turn(
                "t3",
                "s2",
                wa::OPENAI_CHAT,
                vec!["gpt-4o"],
                base + 3_000_000,
                300,
                3,
                vec!["c4"],
                TurnStatus::Incomplete,
            ),
            sample_turn(
                "t4",
                "s3",
                wa::OPENAI_CHAT,
                vec!["gpt-4", "gpt-4o"],
                base + 4_000_000,
                400,
                4,
                vec!["c5"],
                TurnStatus::Complete,
            ),
        ];
        backend.write_turns(turns).await.unwrap();

        // No filter: all 4 turns, default sort_by=start_time DESC
        let page = backend.query_turns(&base_turns_query()).await.unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(page.items.len(), 4);
        assert_eq!(page.items[0].turn_id, "t4");
        assert_eq!(page.items[3].turn_id, "t1");
        assert_eq!(page.items[0].primary_model.as_deref(), Some("gpt-4"));
        assert_eq!(page.items[0].models_used, vec!["gpt-4", "gpt-4o"]);

        // wire_api filter
        let mut q = base_turns_query();
        q.filter.wire_apis = vec![wa::ANTHROPIC.into()];
        let page = backend.query_turns(&q).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].turn_id, "t2");

        // Model filter via list_has_any — should include t1 and t4 (both list gpt-4)
        let mut q = base_turns_query();
        q.filter.models = vec!["gpt-4".into()];
        let page = backend.query_turns(&q).await.unwrap();
        assert_eq!(page.total, 2);
        let ids: Vec<_> = page.items.iter().map(|t| t.turn_id.clone()).collect();
        assert!(ids.contains(&"t1".to_string()));
        assert!(ids.contains(&"t4".to_string()));

        // Status filter (TurnStatus Display: incomplete)
        let mut q = base_turns_query();
        q.statuses = vec!["incomplete".into()];
        let page = backend.query_turns(&q).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].turn_id, "t3");

        // Sort by duration_ms ASC
        let mut q = base_turns_query();
        q.sort_by = "duration_ms".into();
        q.sort_order = "asc".into();
        let page = backend.query_turns(&q).await.unwrap();
        let durations: Vec<_> = page.items.iter().map(|t| t.duration_ms).collect();
        assert_eq!(durations, vec![100, 200, 300, 400]);

        // Pagination
        let mut q = base_turns_query();
        q.page_size = 2;
        q.page = 1;
        let page1 = backend.query_turns(&q).await.unwrap();
        assert_eq!(page1.total, 4);
        assert_eq!(page1.items.len(), 2);
        q.page = 2;
        let page2 = backend.query_turns(&q).await.unwrap();
        assert_eq!(page2.items.len(), 2);
        assert_ne!(page1.items[0].turn_id, page2.items[0].turn_id);

        // Invalid sort field is rejected
        let mut q = base_turns_query();
        q.sort_by = "bogus".into();
        assert!(backend.query_turns(&q).await.is_err());
    }

    #[tokio::test]
    async fn query_turn_by_id_hit_and_miss() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let turn = sample_turn(
            "t-detail",
            "s1",
            wa::ANTHROPIC,
            vec!["claude-sonnet", "claude-haiku"],
            1_700_000_000_000_000,
            1500,
            2,
            vec!["call-a", "call-b"],
            TurnStatus::Complete,
        );
        backend.write_turns(vec![turn]).await.unwrap();

        let hit = backend.query_turn_by_id("t-detail").await.unwrap();
        let d = hit.expect("turn exists");
        assert_eq!(d.turn_id, "t-detail");
        assert_eq!(d.models_used, vec!["claude-sonnet", "claude-haiku"]);
        assert_eq!(d.call_ids, vec!["call-a", "call-b"]);
        // With no user_call_id/final_call_id, full text falls back to previews.
        assert_eq!(d.user_input.as_deref(), Some("hello"));
        assert_eq!(d.final_answer.as_deref(), Some("world"));

        let miss = backend.query_turn_by_id("does-not-exist").await.unwrap();
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn query_turn_by_id_skips_calls_lookup_when_preview_complete() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // A matching llm_calls row exists with full-body text that differs from
        // the preview. If the optimization works, we return the preview
        // (short, no trailing `…`) and never touch the body.
        let base = 1_700_000_000_000_000_i64;
        let mut user_call = mk_call_with_time("c-user", base + 1_000);
        user_call.wire_api = wa::ANTHROPIC;
        user_call.request_body =
            Some(r#"{"messages":[{"role":"user","content":"DB-USER-FULL"}]}"#.into());
        let mut asst_call = mk_call_with_time("c-asst", base + 2_000);
        asst_call.wire_api = wa::ANTHROPIC;
        asst_call.response_body =
            Some(r#"{"content":[{"type":"text","text":"DB-ASSISTANT-FULL"}]}"#.into());
        backend
            .write_calls(vec![user_call, asst_call])
            .await
            .unwrap();

        let mut turn = sample_turn(
            "t-short",
            "s-short",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            base,
            1500,
            2,
            vec!["c-user", "c-asst"],
            TurnStatus::Complete,
        );
        turn.user_input_preview = Some("hi".into());
        turn.user_call_id = Some("c-user".into());
        turn.final_answer_preview = Some("bye".into());
        turn.final_call_id = Some("c-asst".into());
        backend.write_turns(vec![turn]).await.unwrap();

        let d = backend
            .query_turn_by_id("t-short")
            .await
            .unwrap()
            .expect("turn exists");
        // Preview is returned as-is; no llm_calls lookup happened.
        assert_eq!(d.user_input.as_deref(), Some("hi"));
        assert_eq!(d.final_answer.as_deref(), Some("bye"));
    }

    #[tokio::test]
    async fn query_turn_by_id_reads_full_text_when_preview_truncated() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Truncated previews (ending in `…`) must fall through to the llm_calls
        // lookup and return the full body text.
        let base = 1_700_000_000_000_000_i64;
        let full_user: String = "u".repeat(600);
        let full_asst: String = "a".repeat(600);
        let mut user_call = mk_call_with_time("c-user", base + 1_000);
        user_call.wire_api = wa::ANTHROPIC;
        user_call.request_body = Some(
            serde_json::json!({
                "messages": [{ "role": "user", "content": &full_user }]
            })
            .to_string(),
        );
        let mut asst_call = mk_call_with_time("c-asst", base + 2_000);
        asst_call.wire_api = wa::ANTHROPIC;
        asst_call.response_body = Some(
            serde_json::json!({
                "content": [{ "type": "text", "text": &full_asst }]
            })
            .to_string(),
        );
        backend
            .write_calls(vec![user_call, asst_call])
            .await
            .unwrap();

        let truncated_user: String = "u".repeat(500) + "…";
        let truncated_asst: String = "a".repeat(500) + "…";
        let mut turn = sample_turn(
            "t-long",
            "s-long",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            base,
            1500,
            2,
            vec!["c-user", "c-asst"],
            TurnStatus::Complete,
        );
        turn.user_input_preview = Some(truncated_user);
        turn.user_call_id = Some("c-user".into());
        turn.final_answer_preview = Some(truncated_asst);
        turn.final_call_id = Some("c-asst".into());
        backend.write_turns(vec![turn]).await.unwrap();

        let d = backend
            .query_turn_by_id("t-long")
            .await
            .unwrap()
            .expect("turn exists");
        assert_eq!(d.user_input.as_deref(), Some(full_user.as_str()));
        assert_eq!(d.final_answer.as_deref(), Some(full_asst.as_str()));
    }

    #[tokio::test]
    async fn query_turn_calls_orders_and_sequences() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let base = 1_700_000_000_000_000_i64;
        // Insert calls out of chronological order to confirm ORDER BY works.
        let calls = vec![
            mk_call_with_time("call-b", base + 2_000_000),
            mk_call_with_time("call-a", base + 1_000_000),
            mk_call_with_time("call-c", base + 3_000_000),
            // Extra call not in the turn's call_ids — must be excluded.
            mk_call_with_time("call-other", base + 500_000),
        ];
        backend.write_calls(calls).await.unwrap();

        let turn = sample_turn(
            "t-calls",
            "s1",
            wa::OPENAI_CHAT,
            vec!["gpt-4"],
            base,
            3000,
            3,
            vec!["call-a", "call-b", "call-c"],
            TurnStatus::Complete,
        );
        backend.write_turns(vec![turn]).await.unwrap();

        let items = backend.query_turn_calls("t-calls").await.unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "call-a");
        assert_eq!(items[0].sequence, 1);
        assert_eq!(items[1].id, "call-b");
        assert_eq!(items[1].sequence, 2);
        assert_eq!(items[2].id, "call-c");
        assert_eq!(items[2].sequence, 3);
        assert!(items[0].request_time < items[1].request_time);

        // Unknown turn → empty vec (not error).
        let empty = backend.query_turn_calls("no-such-turn").await.unwrap();
        assert!(empty.is_empty());
    }

    fn sample_turn_for_session(
        turn_id: &str,
        session_id: &str,
        start_us: i64,
        user_input: Option<&str>,
    ) -> AgentTurn {
        let mut t = sample_turn(
            turn_id,
            session_id,
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            start_us,
            500,
            1,
            vec![turn_id],
            TurnStatus::Complete,
        );
        t.user_input_preview = user_input.map(String::from);
        t.user_call_id = user_input.map(|_| format!("call-{turn_id}"));
        t
    }

    #[tokio::test]
    async fn query_sessions_window_filters_and_aggregates_full_lifetime() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Three sessions, each with multiple turns spread over time.
        //   S1: turns at t=10, t=50 (lifetime 10..50, middle turn in window 40..60)
        //   S2: turns at t=30, t=45 (both in window)
        //   S3: turns at t=100, t=200 (out of window)
        let base = 1_700_000_000_000_000_i64;
        let us = |secs: i64| base + secs * 1_000_000;
        backend
            .write_turns(vec![
                sample_turn_for_session("t1a", "S1", us(10), Some("first S1")),
                sample_turn_for_session("t1b", "S1", us(50), None),
                sample_turn_for_session("t2a", "S2", us(30), Some("first S2")),
                sample_turn_for_session("t2b", "S2", us(45), None),
                sample_turn_for_session("t3a", "S3", us(100), Some("first S3")),
                sample_turn_for_session("t3b", "S3", us(200), None),
            ])
            .await
            .unwrap();

        // Window [40, 60). S3 entirely out, so excluded. S1 has t=50 in window.
        // S2 has t=45 in window. Both S1 and S2 should return full-lifetime aggregates.
        let page = backend
            .query_sessions(&SessionListQuery {
                time_range: TimeRange {
                    start_us: us(40),
                    end_us: us(60),
                },
                source_id: None,
                agent_kind: None,
                cursor: None,
                page_size: 10,
            })
            .await
            .unwrap();

        assert_eq!(page.items.len(), 2);
        // Sort key is MAX(end_time_in_window) DESC. S1's in-window turn ends
        // latest (t=50 + 500ms), so S1 should be first.
        let s1 = &page.items[0];
        assert_eq!(s1.session_id, "S1");
        assert_eq!(s1.turn_count, 2); // full lifetime: both turns counted
        assert_eq!(s1.first_user_input_preview.as_deref(), Some("first S1"));
        // first_turn_at should be the lifetime's MIN(start_time), not the
        // in-window one. S1's earliest turn is at t=10.
        assert_eq!(s1.first_turn_at, (us(10)) / 1000);

        let s2 = &page.items[1];
        assert_eq!(s2.session_id, "S2");
        assert_eq!(s2.turn_count, 2);
        assert_eq!(s2.first_user_input_preview.as_deref(), Some("first S2"));

        assert!(page.next_cursor.is_none());

        // Page size 1 + cursor roundtrip.
        let p1 = backend
            .query_sessions(&SessionListQuery {
                time_range: TimeRange {
                    start_us: us(40),
                    end_us: us(60),
                },
                source_id: None,
                agent_kind: None,
                cursor: None,
                page_size: 1,
            })
            .await
            .unwrap();
        assert_eq!(p1.items.len(), 1);
        assert_eq!(p1.items[0].session_id, "S1");
        let cursor = p1.next_cursor.expect("has next page");
        let decoded = decode_session_cursor(&cursor).expect("cursor decodes");

        let p2 = backend
            .query_sessions(&SessionListQuery {
                time_range: TimeRange {
                    start_us: us(40),
                    end_us: us(60),
                },
                source_id: None,
                agent_kind: None,
                cursor: Some(decoded),
                page_size: 1,
            })
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 1);
        assert_eq!(p2.items[0].session_id, "S2");
        assert!(p2.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_session_by_id_and_turns_roundtrip() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let base = 1_700_000_000_000_000_i64;
        let us = |secs: i64| base + secs * 1_000_000;
        backend
            .write_turns(vec![
                sample_turn_for_session("ta", "SX", us(10), Some("opener")),
                sample_turn_for_session("tb", "SX", us(20), None),
                sample_turn_for_session("tc", "SX", us(30), None),
            ])
            .await
            .unwrap();

        let d = backend
            .query_session_by_id("", "SX")
            .await
            .unwrap()
            .expect("session exists");
        assert_eq!(d.session_id, "SX");
        assert_eq!(d.turn_count, 3);
        assert_eq!(d.first_user_input_preview.as_deref(), Some("opener"));

        let miss = backend.query_session_by_id("", "ZZZ").await.unwrap();
        assert!(miss.is_none());

        // Turns list: ordered by start_time DESC.
        let turns = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "SX".into(),
                cursor: None,
                page_size: 10,
            })
            .await
            .unwrap();
        assert_eq!(turns.items.len(), 3);
        assert_eq!(turns.items[0].turn_id, "tc");
        assert_eq!(turns.items[2].turn_id, "ta");
        // Fewer rows than page_size → no next page.
        assert!(turns.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_session_turns_cursor_pagination() {
        use crate::query::decode_session_turns_cursor;

        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Seed 5 turns in session "S-CURSOR" with strictly increasing start_time.
        // Short previews (no `…`) keep this test purely about cursor mechanics —
        // no full-text extraction round-trip is triggered.
        let base = 1_700_000_000_000_000_i64;
        let us = |secs: i64| base + secs * 1_000_000;
        let turns: Vec<AgentTurn> = (0..5)
            .map(|i| {
                sample_turn_for_session(
                    &format!("turn-{i}"),
                    "S-CURSOR",
                    us(i as i64 * 10),
                    Some("hi"),
                )
            })
            .collect();
        backend.write_turns(turns).await.unwrap();

        // Page 1: newest 2 (turn-4, turn-3).
        let p1 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-CURSOR".into(),
                cursor: None,
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p1.items.len(), 2);
        assert_eq!(p1.items[0].turn_id, "turn-4");
        assert_eq!(p1.items[1].turn_id, "turn-3");
        let cursor1 = p1.next_cursor.expect("more pages");

        // Page 2: turn-2, turn-1.
        let p2 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-CURSOR".into(),
                cursor: decode_session_turns_cursor(&cursor1),
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 2);
        assert_eq!(p2.items[0].turn_id, "turn-2");
        assert_eq!(p2.items[1].turn_id, "turn-1");
        let cursor2 = p2.next_cursor.expect("more pages");

        // Page 3: turn-0, no next cursor.
        let p3 = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-CURSOR".into(),
                cursor: decode_session_turns_cursor(&cursor2),
                page_size: 2,
            })
            .await
            .unwrap();
        assert_eq!(p3.items.len(), 1);
        assert_eq!(p3.items[0].turn_id, "turn-0");
        assert!(p3.next_cursor.is_none());
    }

    #[tokio::test]
    async fn query_session_turns_extracts_full_text_when_preview_truncated() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        // Build llm_calls carrying real bodies that the Anthropic profile
        // extractor can parse. Bodies must be long enough that the preview is
        // `…`-terminated (i.e. > 500 chars so the stored preview is truncated).
        let base = 1_700_000_000_000_000_i64;
        let full_user: String = "u".repeat(600);
        let full_asst: String = "a".repeat(600);

        let mut user_call = mk_call_with_time("sc-user", base + 1_000);
        user_call.wire_api = wa::ANTHROPIC;
        user_call.request_body = Some(
            serde_json::json!({
                "messages": [{ "role": "user", "content": &full_user }]
            })
            .to_string(),
        );

        let mut asst_call = mk_call_with_time("sc-asst", base + 2_000);
        asst_call.wire_api = wa::ANTHROPIC;
        asst_call.response_body = Some(
            serde_json::json!({
                "content": [{ "type": "text", "text": &full_asst }]
            })
            .to_string(),
        );
        backend
            .write_calls(vec![user_call, asst_call])
            .await
            .unwrap();

        // Turn with `…`-terminated previews pointing at the call ids above.
        let truncated_user: String = "u".repeat(500) + "…";
        let truncated_asst: String = "a".repeat(500) + "…";
        let mut turn = sample_turn(
            "st-long",
            "S-EXTRACT",
            wa::ANTHROPIC,
            vec!["claude-sonnet"],
            base,
            1500,
            2,
            vec!["sc-user", "sc-asst"],
            TurnStatus::Complete,
        );
        turn.user_input_preview = Some(truncated_user);
        turn.user_call_id = Some("sc-user".into());
        turn.final_answer_preview = Some(truncated_asst);
        turn.final_call_id = Some("sc-asst".into());
        backend.write_turns(vec![turn]).await.unwrap();

        let page = backend
            .query_session_turns(&SessionTurnsQuery {
                source_id: String::new(),
                session_id: "S-EXTRACT".into(),
                cursor: None,
                page_size: 10,
            })
            .await
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].user_input.as_deref(),
            Some(full_user.as_str()),
            "user_input should be full text, not truncated preview"
        );
        assert_eq!(
            page.items[0].final_answer.as_deref(),
            Some(full_asst.as_str()),
            "final_answer should be full text, not truncated preview"
        );
    }
}

#[cfg(test)]
mod concurrent_tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use ts_llm::model::ApiType;
    use ts_metrics::model::LlmMetric;
    use ts_turn::{AgentTurn, TurnStatus};

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
        }
    }

    fn mk_turn(i: usize) -> AgentTurn {
        AgentTurn {
            source_id: String::new(),
            turn_id: format!("turn-{i:08}"),
            session_id: format!("session-{}", i % 10),
            wire_api: wa::OPENAI_CHAT.into(),
            agent_kind: "test".into(),
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
            .query_row("SELECT COUNT(*) FROM llm_calls", [], |r| r.get(0))
            .unwrap();
        let turns_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_turns", [], |r| r.get(0))
            .unwrap();
        let metrics_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM llm_metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(calls_count, expected);
        assert_eq!(turns_count, expected);
        assert_eq!(metrics_count, expected);
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::StorageBackend;
    use std::net::IpAddr;
    use std::time::{Duration, SystemTime};
    use ts_llm::model::ApiType;
    use ts_metrics::model::{LlmFinishMetric, LlmMetric};
    use ts_turn::{AgentTurn, TurnStatus};

    fn mk_call(id: &str, request_time_us: i64) -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: id.into(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".into(),
            api_type: ApiType::Chat,
            request_time: request_time_us,
            response_time: Some(request_time_us + 100_000),
            complete_time: Some(request_time_us + 500_000),
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
            ttft_ms: Some(100.0),
            e2e_latency_ms: Some(500.0),
            client_ip: "10.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 1000,
            server_ip: "10.0.0.2".parse::<IpAddr>().unwrap(),
            server_port: 8080,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        }
    }

    fn mk_turn(id: &str, start_us: i64, duration_ms: u64) -> AgentTurn {
        AgentTurn {
            source_id: String::new(),
            turn_id: id.into(),
            session_id: "s".into(),
            wire_api: wa::OPENAI_CHAT.into(),
            agent_kind: "claude-cli".into(),
            start_time_us: start_us,
            end_time_us: start_us + (duration_ms as i64) * 1000,
            duration_ms,
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
            call_ids: vec![id.into()],
            metadata: serde_json::json!({}),
        }
    }

    fn mk_finish_metric(
        granularity: &'static str,
        ts_us: i64,
        finish_reason: &str,
        count: u64,
    ) -> LlmFinishMetric {
        LlmFinishMetric {
            timestamp_us: ts_us,
            source_id: String::new(),
            granularity: granularity.into(),
            wire_api: wa::OPENAI_CHAT.into(),
            model: "gpt-4".into(),
            server_ip: "10.0.0.2".into(),
            finish_reason: finish_reason.into(),
            count,
        }
    }

    fn mk_metric(granularity: &'static str, ts_us: i64) -> LlmMetric {
        LlmMetric {
            timestamp_us: ts_us,
            source_id: String::new(),
            granularity,
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
        }
    }

    #[tokio::test]
    async fn apply_retention_deletes_only_old_rows_per_table() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();

        let now = SystemTime::now();
        let now_us = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64;
        let day_us: i64 = 86_400 * 1_000_000;

        // Calls: 1 old (30d), 1 new (1h).
        backend
            .write_calls(vec![
                mk_call("c-old", now_us - 30 * day_us),
                mk_call("c-new", now_us - 3600 * 1_000_000),
            ])
            .await
            .unwrap();

        // Turns: 1 old (end_time 31d ago), 1 new (today).
        backend
            .write_turns(vec![
                mk_turn("t-old", now_us - 31 * day_us, 1000),
                mk_turn("t-new", now_us - 3600 * 1_000_000, 1000),
            ])
            .await
            .unwrap();

        // Metrics: one old + one new per granularity.
        let old_ts = now_us - 10 * day_us;
        let new_ts = now_us - 600 * 1_000_000;
        backend
            .write_metrics(vec![
                mk_metric("10s", old_ts),
                mk_metric("10s", new_ts),
                mk_metric("1m", old_ts),
                mk_metric("1m", new_ts),
                mk_metric("5m", old_ts),
                mk_metric("5m", new_ts),
                mk_metric("1h", old_ts),
                mk_metric("1h", new_ts),
            ])
            .await
            .unwrap();

        // Finish metrics: 2 paired rows per granularity (one old, one new),
        // mirroring llm_metrics so retention sweeps both tables in lock-step.
        backend
            .write_finish_metrics(vec![
                mk_finish_metric("10s", old_ts, "stop", 5),
                mk_finish_metric("10s", new_ts, "stop", 7),
                mk_finish_metric("1m", old_ts, "stop", 5),
                mk_finish_metric("1m", new_ts, "stop", 7),
                mk_finish_metric("5m", old_ts, "stop", 5),
                mk_finish_metric("5m", new_ts, "stop", 7),
                mk_finish_metric("1h", old_ts, "stop", 5),
                mk_finish_metric("1h", new_ts, "stop", 7),
            ])
            .await
            .unwrap();

        let policy = RetentionPolicy {
            calls_before: Some(now - Duration::from_secs(7 * 86_400)),
            turns_before: Some(now - Duration::from_secs(14 * 86_400)),
            http_exchanges_before: None,
            metrics_before: vec![
                ("10s".to_string(), now - Duration::from_secs(86_400)),
                ("1m".to_string(), now - Duration::from_secs(7 * 86_400)),
                ("5m".to_string(), now - Duration::from_secs(7 * 86_400)),
                // "1h" omitted — must be untouched.
            ],
        };

        let report = backend.apply_retention(policy).await.unwrap();
        assert_eq!(report.calls_deleted, 1);
        assert_eq!(report.turns_deleted, 1);
        assert_eq!(report.metrics_deleted.get("10s"), Some(&1));
        assert_eq!(report.metrics_deleted.get("1m"), Some(&1));
        assert_eq!(report.metrics_deleted.get("5m"), Some(&1));
        assert_eq!(report.metrics_deleted.get("1h"), None);

        let conn = backend.test_conn().lock().unwrap();
        let calls_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM llm_calls", [], |r| r.get(0))
            .unwrap();
        let turns_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_turns", [], |r| r.get(0))
            .unwrap();
        let total_metrics: i64 = conn
            .query_row("SELECT COUNT(*) FROM llm_metrics", [], |r| r.get(0))
            .unwrap();
        let h1_metrics: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM llm_metrics WHERE granularity = '1h'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(calls_count, 1);
        assert_eq!(turns_count, 1);
        // 8 rows, 3 deleted, 1h untouched → 5 left.
        assert_eq!(total_metrics, 5);
        assert_eq!(h1_metrics, 2, "1h granularity must not be swept");

        // Phase 4 long-format finish-reason table is swept by the same
        // (granularity, timestamp) cutoffs as llm_metrics. Inserted 8 rows
        // (4 granularities × old/new); 3 old rows for 10s/1m/5m must be
        // deleted, both 1h rows must remain → 5 rows total.
        let total_finish_metrics: i64 = conn
            .query_row("SELECT COUNT(*) FROM llm_finish_metrics", [], |r| r.get(0))
            .unwrap();
        let h1_finish_metrics: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM llm_finish_metrics WHERE granularity = '1h'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // For each swept granularity, the old (10d-ago) row must be gone
        // and the new (10m-ago) row must survive.
        for gran in ["10s", "1m", "5m"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM llm_finish_metrics WHERE granularity = ?1",
                    duckdb::params![gran],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "granularity {gran}: only the new row should remain");
        }
        assert_eq!(total_finish_metrics, 5);
        assert_eq!(h1_finish_metrics, 2, "1h granularity must not be swept");
    }

    #[tokio::test]
    async fn apply_retention_with_empty_policy_is_noop() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        backend.init().await.unwrap();
        let report = backend
            .apply_retention(RetentionPolicy::default())
            .await
            .unwrap();
        assert_eq!(report.total(), 0);
    }
}
