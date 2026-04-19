use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;

use async_trait::async_trait;
use duckdb::types::{TimeUnit, Value};
use duckdb::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::info;
use ts_common::error::{AppError, Result};
use ts_llm::model::{ApiType, LlmCall, ProviderFormat};
use ts_llm::profiles::build_default_registry;
use ts_metrics::model::LlmMetric;
use ts_turn::LlmTurn;

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

        let pool_size = read_pool_size.max(1);
        let mut readers = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let c = calls_writer
                .try_clone()
                .map_err(|e| AppError::Storage(format!("failed to clone read conn: {e}")))?;
            readers.push(c);
        }

        info!(
            "duckdb opened with 3 writer connections + {} readers",
            pool_size
        );

        Ok(Self {
            write_calls_conn: Arc::new(StdMutex::new(calls_writer)),
            write_turns_conn: Arc::new(StdMutex::new(turns_writer)),
            write_metrics_conn: Arc::new(StdMutex::new(metrics_writer)),
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
    stream_id         VARCHAR NOT NULL DEFAULT '',
    tenant_id         VARCHAR,
    client_ip         VARCHAR NOT NULL,
    client_port       USMALLINT NOT NULL,
    server_ip         VARCHAR NOT NULL,
    server_port       USMALLINT NOT NULL,
    request_time      TIMESTAMP NOT NULL,
    response_time     TIMESTAMP,
    complete_time     TIMESTAMP,
    provider          VARCHAR NOT NULL,
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
    ttfb_ms           DOUBLE,
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
    stream_id           VARCHAR NOT NULL,
    granularity         VARCHAR NOT NULL,
    provider            VARCHAR NOT NULL,
    model               VARCHAR NOT NULL,
    server_ip           VARCHAR NOT NULL,
    request_count       UBIGINT NOT NULL,
    stream_count        UBIGINT NOT NULL,
    non_stream_count    UBIGINT NOT NULL,
    concurrency_sum          UBIGINT NOT NULL,
    concurrency_sample_count UBIGINT NOT NULL,
    concurrency_max          UINTEGER NOT NULL,
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
    finish_complete_count  UBIGINT NOT NULL,
    finish_length_count    UBIGINT NOT NULL,
    finish_tool_use_count  UBIGINT NOT NULL,
    finish_error_count     UBIGINT NOT NULL,
    finish_cancelled_count UBIGINT NOT NULL,
    ttfb_sum            DOUBLE NOT NULL,
    ttfb_count          UBIGINT NOT NULL,
    ttfb_p50            DOUBLE,
    ttfb_p95            DOUBLE,
    ttfb_p99            DOUBLE,
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

const CREATE_LLM_TURNS: &str = "
CREATE TABLE IF NOT EXISTS llm_turns (
    turn_id                   VARCHAR NOT NULL PRIMARY KEY,
    stream_id                 VARCHAR NOT NULL DEFAULT '',
    session_id                VARCHAR NOT NULL,
    tenant_id                 VARCHAR,
    provider                  VARCHAR NOT NULL,
    client_kind               VARCHAR NOT NULL,
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

/// Parse a JSON-encoded array-of-strings (as stored in llm_turns.models_used /
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

/// Load the request_body / response_body of `call_id` from llm_calls and run
/// it through the `client_kind`-matched profile to produce the full user_input
/// or final_answer text. Returns `None` if the call row is missing, the
/// profile is not registered, or the extractor declines.
fn extract_full_text(
    conn: &Connection,
    client_kind: &str,
    call_id: Option<&str>,
    kind: ExtractKind,
) -> Option<String> {
    let call_id = call_id?;
    let registry = build_default_registry();
    let profile = registry.find_by_name(client_kind)?;

    let sql = match kind {
        ExtractKind::User => "SELECT request_body FROM llm_calls WHERE id = ?",
        ExtractKind::Assistant => "SELECT response_body FROM llm_calls WHERE id = ?",
    };
    let body: Option<String> = conn
        .query_row(sql, duckdb::params![call_id], |row| row.get(0))
        .ok()?;
    let (request_body, response_body) = match kind {
        ExtractKind::User => (body, None),
        ExtractKind::Assistant => (None, body),
    };

    // Placeholder LlmCall — profile extractors read only the body fields.
    let call = LlmCall {
        stream_id: String::new(),
        id: String::new(),
        provider: ProviderFormat::Anthropic,
        model: String::new(),
        api_type: ApiType::Chat,
        tenant_id: None,
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
        ttfb_ms: None,
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

/// All valid numeric metric field names accepted by `query_metrics_timeseries`.
/// Virtual `*_avg` fields resolve to `SUM(*_sum) / SUM(*_count)` at query time;
/// the raw `*_sum` / `*_count` fields are also accepted for callers that want
/// to do their own aggregation.
const VALID_METRIC_FIELDS: &[&str] = &[
    "request_count",
    "stream_count",
    "non_stream_count",
    "concurrency_avg",
    "concurrency_sum",
    "concurrency_sample_count",
    "concurrency_max",
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
    "finish_complete_count",
    "finish_length_count",
    "finish_tool_use_count",
    "finish_error_count",
    "finish_cancelled_count",
    "ttfb_avg",
    "ttfb_sum",
    "ttfb_count",
    "ttfb_p50",
    "ttfb_p95",
    "ttfb_p99",
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
///   windows, cross-stream merging) stays correct.
/// * Per-row percentiles (`*_p50/p95/p99`) → weighted average by the matching
///   `*_count` (number of samples contributing to the row's digest). This is
///   an approximation until serialized t-digest bytes land; weighting by the
///   count field (rather than `request_count`) keeps slow-response rows with
///   `request_count=0` from falsely collapsing the result to zero.
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
        "concurrency_avg" => Some(("concurrency_sum", "concurrency_sample_count")),
        "input_tokens_avg" => Some(("total_input_tokens", "input_token_count")),
        "output_tokens_avg" => Some(("total_output_tokens", "output_token_count")),
        "ttfb_avg" => Some(("ttfb_sum", "ttfb_count")),
        "e2e_avg" => Some(("e2e_sum", "e2e_count")),
        "tpot_avg" => Some(("tpot_sum", "tpot_count")),
        _ => None,
    }
}

/// Weight column for percentile weighted-avg aggregation.
fn percentile_weight(field: &str) -> &'static str {
    if field.starts_with("ttfb") {
        "ttfb_count"
    } else if field.starts_with("e2e") {
        "e2e_count"
    } else if field.starts_with("tpot") {
        "tpot_count"
    } else {
        "request_count"
    }
}

/// Fields that represent counts or totals (use SUM when aggregating across groups).
const SUM_FIELDS: &[&str] = &[
    "request_count",
    "stream_count",
    "non_stream_count",
    "concurrency_sum",
    "concurrency_sample_count",
    "concurrency_max",
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
    "finish_complete_count",
    "finish_length_count",
    "finish_tool_use_count",
    "finish_error_count",
    "finish_cancelled_count",
    "ttfb_sum",
    "ttfb_count",
    "e2e_sum",
    "e2e_count",
    "tpot_sum",
    "tpot_count",
];

/// Build a WHERE clause segment for dimension filters (ungrouped queries).
/// Empty filter vec => match wildcard '*'. Non-empty => IN (...).
fn build_dimension_where(filter: &DimensionFilter) -> String {
    let provider_clause = if filter.providers.is_empty() {
        "provider = '*'".to_string()
    } else {
        let list: Vec<String> = filter
            .providers
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect();
        format!("provider IN ({})", list.join(", "))
    };
    let model_clause = if filter.models.is_empty() {
        "model = '*'".to_string()
    } else {
        let list: Vec<String> = filter
            .models
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect();
        format!("model IN ({})", list.join(", "))
    };
    let server_clause = if filter.server_ips.is_empty() {
        "server_ip = '*'".to_string()
    } else {
        let list: Vec<String> = filter
            .server_ips
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect();
        format!("server_ip IN ({})", list.join(", "))
    };
    format!("{provider_clause} AND {model_clause} AND {server_clause}")
}

/// Build WHERE clause for grouped timeseries queries.
/// group_by="provider": returns per-model rows (provider != '*', model != '*', server_ip = '*')
///   filtered by provider filter if specified.
/// group_by="model": returns per-model rows (provider != '*', model != '*', server_ip = '*')
///   filtered by model filter if specified.
fn build_dimension_where_for_group(filter: &DimensionFilter, group_by: &str) -> String {
    match group_by {
        "provider" => {
            let provider_clause = if filter.providers.is_empty() {
                "provider != '*'".to_string()
            } else {
                let list: Vec<String> = filter
                    .providers
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                format!("provider IN ({})", list.join(", "))
            };
            // No pre-aggregated (provider, *, *) rows exist — the aggregator
            // produces (provider, model, *) rows.  GROUP BY provider in the
            // timeseries query will SUM across models for each provider.
            format!("{provider_clause} AND model != '*' AND server_ip = '*'")
        }
        "model" => {
            let provider_clause = if filter.providers.is_empty() {
                "provider != '*'".to_string()
            } else {
                let list: Vec<String> = filter
                    .providers
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                format!("provider IN ({})", list.join(", "))
            };
            let model_clause = if filter.models.is_empty() {
                "model != '*'".to_string()
            } else {
                let list: Vec<String> = filter
                    .models
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                format!("model IN ({})", list.join(", "))
            };
            format!("{provider_clause} AND {model_clause} AND server_ip = '*'")
        }
        _ => build_dimension_where(filter),
    }
}

/// Bindable row prepared outside the writer Mutex.
/// All expensive conversions (IP formatting, enum → string, header JSON,
/// timestamp wrapping) happen before the lock is acquired.
struct PreparedCall {
    id: String,
    stream_id: String,
    tenant_id: Option<String>,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    request_time: Value,
    response_time: Option<Value>,
    complete_time: Option<Value>,
    provider: String,
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
    ttfb_ms: Option<f64>,
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
        stream_id: call.stream_id,
        tenant_id: call.tenant_id,
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
        provider: call.provider.to_string(),
        model: call.model,
        api_type: call.api_type.to_string(),
        is_stream: call.is_stream,
        request_path: call.request_path,
        status_code: call.status_code,
        finish_reason: call.finish_reason.map(|r| r.to_string()),
        input_tokens: call.input_tokens,
        output_tokens: call.output_tokens,
        total_tokens: call.total_tokens,
        cache_read_input_tokens: call.cache_read_input_tokens,
        cache_creation_input_tokens: call.cache_creation_input_tokens,
        ttfb_ms: call.ttfb_ms,
        e2e_latency_ms: call.e2e_latency_ms,
        request_body: call.request_body,
        response_body: call.response_body,
        response_id: call.response_id,
        request_headers: headers_to_json(&call.request_headers),
        response_headers: headers_to_json(&call.response_headers),
    }
}

struct PreparedTurn {
    turn_id: String,
    stream_id: String,
    session_id: String,
    tenant_id: Option<String>,
    provider: String,
    client_kind: String,
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

fn prepare_turn(t: LlmTurn) -> PreparedTurn {
    PreparedTurn {
        turn_id: t.turn_id,
        stream_id: t.stream_id,
        session_id: t.session_id,
        tenant_id: t.tenant_id,
        provider: t.provider,
        client_kind: t.client_kind,
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
    stream_id: String,
    granularity: &'static str,
    provider: String,
    model: String,
    server_ip: String,
    inner: LlmMetric,
}

fn prepare_metric(m: LlmMetric) -> PreparedMetric {
    PreparedMetric {
        timestamp: Value::Timestamp(TimeUnit::Microsecond, m.timestamp_us),
        stream_id: m.stream_id.clone(),
        granularity: m.granularity,
        provider: m.provider.clone(),
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
            conn.execute_batch(CREATE_LLM_TURNS)
                .map_err(|e| AppError::Storage(format!("failed to create llm_turns: {e}")))?;
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
                        p.stream_id,
                        p.tenant_id,
                        p.client_ip,
                        p.client_port,
                        p.server_ip,
                        p.server_port,
                        p.request_time,
                        p.response_time,
                        p.complete_time,
                        p.provider,
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
                        p.ttfb_ms,
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
                        p.stream_id,
                        p.granularity,
                        p.provider,
                        p.model,
                        p.server_ip,
                        m.request_count,
                        m.stream_count,
                        m.non_stream_count,
                        m.concurrency_sum,
                        m.concurrency_sample_count,
                        m.concurrency_max,
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
                        m.finish_complete_count,
                        m.finish_length_count,
                        m.finish_tool_use_count,
                        m.finish_error_count,
                        m.finish_cancelled_count,
                        m.ttfb_sum,
                        m.ttfb_count,
                        m.ttfb_p50,
                        m.ttfb_p95,
                        m.ttfb_p99,
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

    async fn write_turns(&self, turns: Vec<LlmTurn>) -> Result<()> {
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
                .appender("llm_turns")
                .map_err(|e| AppError::Storage(format!("failed to create turns appender: {e}")))?;
            for p in &prepared {
                appender
                    .append_row(duckdb::params![
                        p.turn_id,
                        p.stream_id,
                        p.session_id,
                        p.tenant_id,
                        p.provider,
                        p.client_kind,
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
                // Grouped query: aggregate across the group dimension plus stream_id.
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
                // per-stream aggregators emit one row per stream per (ts,
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

            let sql = "
                SELECT
                    COALESCE(SUM(request_count), 0),
                    COALESCE(SUM(error_count), 0),
                    COALESCE(SUM(error_4xx_count), 0),
                    COALESCE(SUM(error_429_count), 0),
                    COALESCE(SUM(error_5xx_count), 0),
                    COALESCE(SUM(total_input_tokens), 0),
                    COALESCE(SUM(total_output_tokens), 0),
                    CASE WHEN SUM(ttfb_count) > 0
                         THEN SUM(ttfb_sum) / SUM(ttfb_count) ELSE NULL END,
                    CASE WHEN SUM(e2e_count) > 0
                         THEN SUM(e2e_sum) / SUM(e2e_count) ELSE NULL END,
                    CASE WHEN SUM(tpot_count) > 0
                         THEN SUM(tpot_sum) / SUM(tpot_count) ELSE NULL END
                FROM llm_metrics
                WHERE provider = '*' AND model = '*' AND server_ip = '*'
                  AND granularity = '10s'
                  AND timestamp >= ? AND timestamp < ?
            ";

            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare summary query: {e}")))?;

            let row = stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| {
                    Ok(MetricsSummaryRow {
                        request_count: row.get::<_, u64>(0)?,
                        error_count: row.get::<_, u64>(1)?,
                        error_4xx_count: row.get::<_, u64>(2)?,
                        error_429_count: row.get::<_, u64>(3)?,
                        error_5xx_count: row.get::<_, u64>(4)?,
                        total_input_tokens: row.get::<_, u64>(5)?,
                        total_output_tokens: row.get::<_, u64>(6)?,
                        ttfb_avg: row.get::<_, Option<f64>>(7)?,
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
            "request_count",
            "error_count",
            "total_input_tokens",
            "total_output_tokens",
            "ttfb_avg",
            "ttfb_p95",
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

            let sql = format!(
                "
                SELECT * FROM (
                    SELECT
                        provider,
                        model,
                        COALESCE(SUM(request_count), 0) AS request_count,
                        COALESCE(SUM(error_count), 0) AS error_count,
                        COALESCE(SUM(error_4xx_count), 0) AS error_4xx_count,
                        COALESCE(SUM(error_429_count), 0) AS error_429_count,
                        COALESCE(SUM(error_5xx_count), 0) AS error_5xx_count,
                        COALESCE(SUM(total_input_tokens), 0) AS total_input_tokens,
                        COALESCE(SUM(total_output_tokens), 0) AS total_output_tokens,
                        CASE WHEN SUM(ttfb_count) > 0
                             THEN SUM(ttfb_sum) / SUM(ttfb_count)
                             ELSE NULL END AS ttfb_avg,
                        CASE WHEN SUM(ttfb_count) > 0
                             THEN SUM(ttfb_p95 * ttfb_count) / SUM(ttfb_count)
                             ELSE NULL END AS ttfb_p95,
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
                    WHERE provider != '*' AND model != '*' AND server_ip = '*'
                      AND granularity = '10s'
                      AND timestamp >= ? AND timestamp < ?
                    GROUP BY provider, model
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
                    provider: row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    model: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_count: row
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
                    ttfb_avg: row
                        .get(9)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    ttfb_p95: row
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

    async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage> {
        const VALID_SORT_FIELDS: &[&str] = &[
            "request_time",
            "status_code",
            "ttfb_ms",
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

            if !query.filter.providers.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .providers
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("provider IN ({})", list.join(", ")));
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
                "SELECT id, stream_id, epoch_ms(request_time), provider, model, status_code, is_stream, \
                 finish_reason, ttfb_ms, e2e_latency_ms, input_tokens, output_tokens \
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
                    stream_id: row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    request_time: row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    provider: row
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
                    ttfb_ms: row
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
                    id, stream_id,
                    epoch_ms(request_time),
                    epoch_ms(response_time),
                    epoch_ms(complete_time),
                    provider, model, api_type, is_stream, request_path,
                    status_code, finish_reason,
                    input_tokens, output_tokens, total_tokens,
                    ttfb_ms, e2e_latency_ms,
                    response_id, tenant_id,
                    client_ip, client_port, server_ip, server_port,
                    request_body, response_body,
                    request_headers, response_headers
                FROM llm_calls
                WHERE id = ?
            ";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare call_by_id query: {e}"))
            })?;

            let result = stmt.query_row(duckdb::params![id], |row| {
                Ok(CallDetail {
                    id: row.get(0)?,
                    stream_id: row.get(1)?,
                    request_time: row.get(2)?,
                    response_time: row.get(3)?,
                    complete_time: row.get(4)?,
                    provider: row.get(5)?,
                    model: row.get(6)?,
                    api_type: row.get(7)?,
                    is_stream: row.get(8)?,
                    request_path: row.get(9)?,
                    status_code: row.get(10)?,
                    finish_reason: row.get(11)?,
                    input_tokens: row.get(12)?,
                    output_tokens: row.get(13)?,
                    total_tokens: row.get(14)?,
                    ttfb_ms: row.get(15)?,
                    e2e_latency_ms: row.get(16)?,
                    response_id: row.get(17)?,
                    tenant_id: row.get(18)?,
                    client_ip: row.get(19)?,
                    client_port: row.get(20)?,
                    server_ip: row.get(21)?,
                    server_port: row.get(22)?,
                    request_body: row.get(23)?,
                    response_body: row.get(24)?,
                    request_headers: row.get(25)?,
                    response_headers: row.get(26)?,
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

            if !query.filter.providers.is_empty() {
                let list: Vec<String> = query
                    .filter
                    .providers
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("provider IN ({})", list.join(", ")));
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
            if !query.client_kinds.is_empty() {
                let list: Vec<String> = query
                    .client_kinds
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "''")))
                    .collect();
                where_parts.push(format!("client_kind IN ({})", list.join(", ")));
            }

            let where_sql = where_parts.join(" AND ");
            let sort_by = &query.sort_by;

            let count_sql = format!("SELECT COUNT(*) FROM llm_turns WHERE {where_sql}");
            let mut count_stmt = conn
                .prepare(&count_sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare count query: {e}")))?;
            let total: u64 = count_stmt
                .query_row(duckdb::params![start_ts, end_ts], |row| row.get(0))
                .map_err(|e| AppError::Storage(format!("failed to execute count query: {e}")))?;

            let offset = (query.page.saturating_sub(1)) as u64 * query.page_size as u64;
            let limit = query.page_size;
            let items_sql = format!(
                "SELECT turn_id, stream_id, session_id, \
                 epoch_ms(start_time), epoch_ms(end_time), duration_ms, \
                 provider, client_kind, models_used, call_count, \
                 total_input_tokens, total_output_tokens, status, \
                 final_finish_reason, user_input_preview, final_answer_preview \
                 FROM llm_turns WHERE {where_sql} \
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
                    stream_id: row
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
                    provider: row
                        .get(6)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    client_kind: row
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
                    turn_id, stream_id, session_id, tenant_id, provider, client_kind,
                    epoch_ms(start_time), epoch_ms(end_time), duration_ms, call_count,
                    models_used, subagents_used,
                    total_input_tokens, total_output_tokens,
                    total_cache_read_input_tokens, total_cache_creation_input_tokens,
                    total_cost_usd, status, final_finish_reason,
                    user_input_preview, user_call_id,
                    final_answer_preview, final_call_id,
                    call_ids, metadata
                FROM llm_turns
                WHERE turn_id = ?
            ";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare turn_by_id query: {e}"))
            })?;

            #[allow(clippy::type_complexity)]
            let result = stmt.query_row(duckdb::params![turn_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,          // turn_id
                    row.get::<_, String>(1)?,          // stream_id
                    row.get::<_, String>(2)?,          // session_id
                    row.get::<_, Option<String>>(3)?,  // tenant_id
                    row.get::<_, String>(4)?,          // provider
                    row.get::<_, String>(5)?,          // client_kind
                    row.get::<_, i64>(6)?,             // start_time
                    row.get::<_, i64>(7)?,             // end_time
                    row.get::<_, u64>(8)?,             // duration_ms
                    row.get::<_, u32>(9)?,             // call_count
                    row.get::<_, Option<String>>(10)?, // models_used
                    row.get::<_, Option<String>>(11)?, // subagents_used
                    row.get::<_, u64>(12)?,            // total_input_tokens
                    row.get::<_, u64>(13)?,            // total_output_tokens
                    row.get::<_, u64>(14)?,            // total_cache_read_input_tokens
                    row.get::<_, u64>(15)?,            // total_cache_creation_input_tokens
                    row.get::<_, Option<f64>>(16)?,    // total_cost_usd
                    row.get::<_, String>(17)?,         // status
                    row.get::<_, Option<String>>(18)?, // final_finish_reason
                    row.get::<_, Option<String>>(19)?, // user_input_preview
                    row.get::<_, Option<String>>(20)?, // user_call_id
                    row.get::<_, Option<String>>(21)?, // final_answer_preview
                    row.get::<_, Option<String>>(22)?, // final_call_id
                    row.get::<_, Option<String>>(23)?, // call_ids (JSON)
                    row.get::<_, Option<String>>(24)?, // metadata
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
                stream_id,
                session_id,
                tenant_id,
                provider,
                client_kind,
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
                    &client_kind,
                    user_call_id.as_deref(),
                    ExtractKind::User,
                )
                .or_else(|| user_input_preview.clone()),
            };
            let final_answer = match final_answer_preview.as_deref() {
                Some(p) if !p.ends_with('…') => final_answer_preview.clone(),
                _ => extract_full_text(
                    &conn,
                    &client_kind,
                    final_call_id.as_deref(),
                    ExtractKind::Assistant,
                )
                .or_else(|| final_answer_preview.clone()),
            };

            Ok(Some(TurnDetail {
                turn_id,
                stream_id,
                session_id,
                tenant_id,
                provider,
                client_kind,
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
            let sql = "
                SELECT
                    c.id,
                    epoch_ms(c.request_time),
                    epoch_ms(c.response_time),
                    epoch_ms(c.complete_time),
                    c.provider, c.model, c.status_code, c.is_stream,
                    c.finish_reason, c.ttfb_ms, c.e2e_latency_ms,
                    c.input_tokens, c.output_tokens
                FROM llm_calls c
                JOIN (SELECT UNNEST(json_extract_string(call_ids, '$[*]')) AS cid
                      FROM llm_turns WHERE turn_id = ?) ids ON c.id = ids.cid
                ORDER BY c.request_time ASC, c.complete_time ASC
            ";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare turn_calls query: {e}"))
            })?;

            let mut rows = stmt.query(duckdb::params![turn_id]).map_err(|e| {
                AppError::Storage(format!("failed to execute turn_calls query: {e}"))
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
                    provider: row
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
                    ttfb_ms: row
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
                });
            }

            Ok(items)
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    async fn query_distinct_providers(&self) -> Result<Vec<String>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT provider FROM llm_metrics WHERE provider != '*' ORDER BY provider"
            ).map_err(|e| AppError::Storage(format!("failed to prepare distinct_providers query: {e}")))?;
            let mut rows = stmt.query([])
                .map_err(|e| AppError::Storage(format!("failed to execute distinct_providers query: {e}")))?;
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

    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        let calls_conn = self.write_calls_conn.clone();
        let turns_conn = self.write_turns_conn.clone();
        let metrics_conn = self.write_metrics_conn.clone();

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

            if let Some(cutoff) = policy.turns_before {
                let ts = timestamp_value(cutoff)?;
                let conn = turns_conn
                    .lock()
                    .map_err(|e| AppError::Storage(format!("failed to lock turns writer: {e}")))?;
                let n = conn
                    .execute(
                        "DELETE FROM llm_turns WHERE end_time < ?1",
                        duckdb::params![ts],
                    )
                    .map_err(|e| AppError::Storage(format!("failed to delete llm_turns: {e}")))?;
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
    use ts_llm::model::{ApiType, FinishReason, ProviderFormat};

    fn in_memory_backend() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
    }

    fn sample_call() -> LlmCall {
        LlmCall {
            stream_id: String::new(),
            id: "01912345-6789-7abc-def0-123456789abc".to_string(),
            provider: ProviderFormat::OpenAI,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            tenant_id: Some("tenant-abc".to_string()),
            request_time: 1_700_000_000_000_000,
            response_time: Some(1_700_000_000_500_000),
            complete_time: Some(1_700_000_001_000_000),
            request_path: "/v1/chat/completions".to_string(),
            is_stream: true,
            request_body: Some(r#"{"model":"gpt-4"}"#.to_string()),
            status_code: Some(200),
            finish_reason: Some(FinishReason::Complete),
            response_body: Some(r#"{"choices":[...]}"#.to_string()),
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: Some(500.0),
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
            stream_id: String::new(),
            granularity: "1m",
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            server_ip: "10.0.0.2".to_string(),
            request_count: 42,
            stream_count: 30,
            non_stream_count: 12,
            // concurrency avg 3.5 → sum 147 across 42 samples.
            concurrency_sum: 147,
            concurrency_sample_count: 42,
            concurrency_max: 8,
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
            finish_complete_count: 35,
            finish_length_count: 3,
            finish_tool_use_count: 2,
            finish_error_count: 1,
            finish_cancelled_count: 1,
            // ttfb_avg 150 × 42 = 6300.
            ttfb_sum: 6300.0,
            ttfb_count: 42,
            ttfb_p50: Some(120.0),
            ttfb_p95: Some(350.0),
            ttfb_p99: Some(500.0),
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
            .prepare("SELECT granularity, model, request_count, ttfb_p50 FROM llm_metrics")
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
                "SELECT total_output_tokens, output_token_count, tpot_sum, tpot_count, \
                 finish_complete_count, finish_length_count, finish_tool_use_count, \
                 finish_error_count, finish_cancelled_count \
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
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, u64>(7)?,
                    row.get::<_, u64>(8)?,
                ))
            })
            .unwrap();
        assert_eq!(row.0, 5000);
        assert_eq!(row.1, 42);
        // tpot_sum 666 / tpot_count 30 = 22.2
        assert!((row.2 - 666.0).abs() < 1e-6);
        assert_eq!(row.3, 30);
        assert_eq!(row.4, 35);
        assert_eq!(row.5, 3);
        assert_eq!(row.6, 2);
        assert_eq!(row.7, 1);
        assert_eq!(row.8, 1);
    }

    // ===== Task 3: query_distinct_* tests =====

    #[tokio::test]
    async fn test_query_distinct_providers() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        // Write metrics with providers "openai", "anthropic", and "*"
        let mut m1 = sample_metric();
        m1.provider = "openai".to_string();
        m1.model = "gpt-4".to_string();
        m1.server_ip = "10.0.0.1".to_string();

        let mut m2 = sample_metric();
        m2.provider = "anthropic".to_string();
        m2.model = "claude-3".to_string();
        m2.server_ip = "10.0.0.1".to_string();

        let mut m3 = sample_metric();
        m3.provider = "*".to_string();
        m3.model = "*".to_string();
        m3.server_ip = "*".to_string();

        backend.write_metrics(vec![m1, m2, m3]).await.unwrap();

        let providers = backend.query_distinct_providers().await.unwrap();
        assert_eq!(providers, vec!["anthropic", "openai"]);
    }

    #[tokio::test]
    async fn test_query_distinct_models() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let mut m1 = sample_metric();
        m1.provider = "openai".to_string();
        m1.model = "gpt-4".to_string();
        m1.server_ip = "10.0.0.1".to_string();

        let mut m2 = sample_metric();
        m2.provider = "openai".to_string();
        m2.model = "gpt-3.5".to_string();
        m2.server_ip = "10.0.0.1".to_string();

        let mut m3 = sample_metric();
        m3.provider = "*".to_string();
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
        m1.provider = "openai".to_string();
        m1.model = "gpt-4".to_string();
        m1.server_ip = "10.0.0.1".to_string();

        let mut m2 = sample_metric();
        m2.provider = "openai".to_string();
        m2.model = "gpt-4".to_string();
        m2.server_ip = "10.0.0.2".to_string();

        let mut m3 = sample_metric();
        m3.provider = "*".to_string();
        m3.model = "*".to_string();
        m3.server_ip = "*".to_string();

        backend.write_metrics(vec![m1, m2, m3]).await.unwrap();

        let server_ips = backend.query_distinct_server_ips().await.unwrap();
        assert_eq!(server_ips, vec!["10.0.0.1", "10.0.0.2"]);
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
        m1.provider = "*".to_string();
        m1.model = "*".to_string();
        m1.server_ip = "*".to_string();
        m1.ttfb_p50 = Some(100.0);
        m1.ttfb_p95 = Some(200.0);

        let mut m2 = sample_metric();
        m2.timestamp_us = 1_700_000_060_000_000; // +60s
        m2.granularity = "1m";
        m2.provider = "*".to_string();
        m2.model = "*".to_string();
        m2.server_ip = "*".to_string();
        m2.ttfb_p50 = Some(150.0);
        m2.ttfb_p95 = Some(300.0);

        backend.write_metrics(vec![m1, m2]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: 1_700_000_000_000_000,
                end_us: 1_700_000_120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["ttfb_p50".to_string(), "ttfb_p95".to_string()],
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
    async fn test_query_metrics_timeseries_group_by_provider() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        // Per-model rows: (provider, model, server_ip='*')
        // These are what the aggregator actually produces. group_by=provider
        // should SUM across models within each provider.
        let mut m = sample_metric();
        m.timestamp_us = ts;
        m.granularity = "1m";
        m.server_ip = "*".to_string();

        m.provider = "openai".to_string();
        m.model = "gpt-4".to_string();
        m.request_count = 200;
        backend.write_metrics(vec![m.clone()]).await.unwrap();

        m.model = "gpt-3.5".to_string();
        m.request_count = 100;
        backend.write_metrics(vec![m.clone()]).await.unwrap();

        m.provider = "anthropic".to_string();
        m.model = "claude-3".to_string();
        m.request_count = 50;
        backend.write_metrics(vec![m]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["request_count".to_string()],
            group_by: Some("provider".to_string()),
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        // Should have 2 rows: anthropic and openai (aggregated across models)
        assert_eq!(rows.len(), 2);
        let anthropic_row = rows
            .iter()
            .find(|r| r.group.as_deref() == Some("anthropic"))
            .unwrap();
        let openai_row = rows
            .iter()
            .find(|r| r.group.as_deref() == Some("openai"))
            .unwrap();
        assert_eq!(anthropic_row.values[0], Some(50.0));
        assert_eq!(openai_row.values[0], Some(300.0)); // 200 + 100
    }

    // With per-stream aggregators, the sink receives one row per (stream_id,
    // ts, dim). The ungrouped timeseries query MUST GROUP BY timestamp so
    // the caller sees one point per timestamp (request_count summed, ttfb
    // weighted-averaged by request_count). Before this fix the branch had
    // no GROUP BY and returned N overlapping rows per timestamp.
    #[tokio::test]
    async fn test_multi_stream_ungrouped_timeseries_merges() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut stream0 = sample_metric();
        stream0.timestamp_us = ts;
        stream0.stream_id = "s0".into();
        stream0.granularity = "1m";
        stream0.provider = "*".into();
        stream0.model = "*".into();
        stream0.server_ip = "*".into();
        stream0.request_count = 10;
        stream0.ttfb_count = 10;
        stream0.ttfb_p50 = Some(100.0);
        stream0.error_count = 1;

        let mut stream1 = sample_metric();
        stream1.timestamp_us = ts;
        stream1.stream_id = "s1".into();
        stream1.granularity = "1m";
        stream1.provider = "*".into();
        stream1.model = "*".into();
        stream1.server_ip = "*".into();
        stream1.request_count = 30;
        stream1.ttfb_count = 30;
        stream1.ttfb_p50 = Some(200.0);
        stream1.error_count = 3;

        backend.write_metrics(vec![stream0, stream1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec![
                "request_count".to_string(),
                "ttfb_p50".to_string(),
                "error_count".to_string(),
            ],
            group_by: None,
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "ungrouped query must return 1 row per timestamp across streams, got {}",
            rows.len()
        );
        assert_eq!(rows[0].values[0], Some(40.0), "request_count SUM = 10 + 30");
        // weighted avg by ttfb_count: (100*10 + 200*30) / 40 = 175
        let p50 = rows[0].values[1].unwrap();
        assert!((p50 - 175.0).abs() < 0.01, "weighted p50 ≈ 175, got {p50}");
        assert_eq!(rows[0].values[2], Some(4.0), "error_count SUM = 1 + 3");
    }

    #[tokio::test]
    async fn test_multi_stream_grouped_timeseries_merges() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut s0 = sample_metric();
        s0.timestamp_us = ts;
        s0.stream_id = "s0".into();
        s0.granularity = "1m";
        s0.provider = "openai".into();
        s0.model = "gpt-4".into();
        s0.server_ip = "*".into();
        s0.request_count = 10;

        let mut s1 = sample_metric();
        s1.timestamp_us = ts;
        s1.stream_id = "s1".into();
        s1.granularity = "1m";
        s1.provider = "openai".into();
        s1.model = "gpt-4".into();
        s1.server_ip = "*".into();
        s1.request_count = 40;

        backend.write_metrics(vec![s0, s1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["request_count".to_string()],
            group_by: Some("provider".to_string()),
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group.as_deref(), Some("openai"));
        assert_eq!(rows[0].values[0], Some(50.0), "grouped SUM across streams");
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
        m1.provider = "*".to_string();
        m1.model = "*".to_string();
        m1.server_ip = "*".to_string();
        m1.request_count = 100;
        m1.stream_count = 80;
        m1.error_count = 5;
        m1.error_4xx_count = 3;
        m1.error_429_count = 1;
        m1.error_5xx_count = 2;
        m1.total_input_tokens = 10_000;
        m1.total_output_tokens = 5_000;
        // ttfb avg 100 over 100 samples → sum 10_000
        m1.ttfb_sum = 10_000.0;
        m1.ttfb_count = 100;
        m1.e2e_sum = 50_000.0;
        m1.e2e_count = 100;
        // tpot avg 40 over 80 streaming samples → sum 3200
        m1.tpot_sum = 3_200.0;
        m1.tpot_count = 80;

        let mut m2 = sample_metric();
        m2.timestamp_us = ts2;
        m2.granularity = "10s";
        m2.provider = "*".to_string();
        m2.model = "*".to_string();
        m2.server_ip = "*".to_string();
        m2.request_count = 200;
        m2.stream_count = 160;
        m2.error_count = 10;
        m2.error_4xx_count = 6;
        m2.error_429_count = 2;
        m2.error_5xx_count = 4;
        m2.total_input_tokens = 20_000;
        m2.total_output_tokens = 10_000;
        // ttfb avg 200 over 200 samples → sum 40_000
        m2.ttfb_sum = 40_000.0;
        m2.ttfb_count = 200;
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
        assert_eq!(summary.request_count, 300);
        assert_eq!(summary.error_count, 15);
        assert_eq!(summary.error_4xx_count, 9);
        assert_eq!(summary.error_429_count, 3);
        assert_eq!(summary.error_5xx_count, 6);
        assert_eq!(summary.total_input_tokens, 30_000);
        assert_eq!(summary.total_output_tokens, 15_000);
        // Exact avg via sum+count: (10000 + 40000) / 300 = 166.666...
        let ttfb_avg = summary.ttfb_avg.unwrap();
        assert!(
            (ttfb_avg - 500.0 / 3.0).abs() < 0.01,
            "expected ~166.67, got {ttfb_avg}"
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
        m_gpt4.provider = "openai".to_string();
        m_gpt4.model = "gpt-4".to_string();
        m_gpt4.server_ip = "*".to_string();
        m_gpt4.request_count = 100;
        m_gpt4.stream_count = 80;
        // ttfb avg 150 over 100 → sum 15000
        m_gpt4.ttfb_sum = 15_000.0;
        m_gpt4.ttfb_count = 100;
        m_gpt4.ttfb_p95 = Some(400.0);
        m_gpt4.e2e_sum = 100_000.0;
        m_gpt4.e2e_count = 100;
        m_gpt4.e2e_p95 = Some(3000.0);
        // tpot avg 20 over 80 → sum 1600
        m_gpt4.tpot_sum = 1_600.0;
        m_gpt4.tpot_count = 80;

        let mut m_claude = sample_metric();
        m_claude.timestamp_us = ts;
        m_claude.granularity = "10s";
        m_claude.provider = "anthropic".to_string();
        m_claude.model = "claude-3".to_string();
        m_claude.server_ip = "*".to_string();
        m_claude.request_count = 200;
        m_claude.stream_count = 150;
        // ttfb avg 120 over 200 → sum 24000
        m_claude.ttfb_sum = 24_000.0;
        m_claude.ttfb_count = 200;
        m_claude.ttfb_p95 = Some(300.0);
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
            sort_by: "request_count".to_string(),
            sort_order: "DESC".to_string(),
            limit: 10,
        };

        let rows = backend.query_metrics_models(&query).await.unwrap();
        assert_eq!(rows.len(), 2);
        // claude-3 should come first (200 > 100)
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[0].model, "claude-3");
        assert_eq!(rows[0].request_count, 200);
        assert_eq!(rows[1].provider, "openai");
        assert_eq!(rows[1].model, "gpt-4");
        assert_eq!(rows[1].request_count, 100);
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
        assert_eq!(detail.provider, "openai");
        assert_eq!(detail.status_code, Some(200));
        assert_eq!(detail.input_tokens, Some(100));
        assert_eq!(detail.output_tokens, Some(50));
        assert_eq!(detail.total_tokens, Some(150));
        assert!(detail.request_body.is_some());
        assert!(detail.response_body.is_some());
        assert!(detail.request_headers.is_some());
        assert!(detail.response_headers.is_some());
        assert_eq!(detail.tenant_id.as_deref(), Some("tenant-abc"));

        // Query nonexistent id
        let not_found = backend.query_call_by_id("does-not-exist").await.unwrap();
        assert!(not_found.is_none());
    }
}

#[cfg(test)]
mod turn_tests {
    use super::*;
    use std::net::IpAddr;
    use ts_llm::model::{ApiType, FinishReason, ProviderFormat};
    use ts_turn::{LlmTurn, TurnStatus};

    fn sample_turn(
        turn_id: &str,
        session_id: &str,
        provider: &str,
        models_used: Vec<&str>,
        start_us: i64,
        duration_ms: u64,
        call_count: u32,
        call_ids: Vec<&str>,
        status: TurnStatus,
    ) -> LlmTurn {
        LlmTurn {
            stream_id: String::new(),
            turn_id: turn_id.into(),
            session_id: session_id.into(),
            tenant_id: None,
            provider: provider.into(),
            client_kind: "claude-cli".into(),
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
            stream_id: String::new(),
            id: id.into(),
            provider: ProviderFormat::OpenAI,
            model: "gpt-4".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: request_time_us,
            response_time: Some(request_time_us + 100_000),
            complete_time: Some(request_time_us + 500_000),
            request_path: "/v1/chat/completions".into(),
            is_stream: false,
            request_body: None,
            status_code: Some(200),
            finish_reason: Some(FinishReason::Complete),
            response_body: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: Some(100.0),
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
            "anthropic",
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
            client_kinds: vec![],
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
                "openai",
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
                "anthropic",
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
                "openai",
                vec!["gpt-4o"],
                base + 3_000_000,
                300,
                3,
                vec!["c4"],
                TurnStatus::Length,
            ),
            sample_turn(
                "t4",
                "s3",
                "openai",
                vec!["gpt-4", "gpt-4o"],
                base + 4_000_000,
                400,
                4,
                vec!["c5"],
                TurnStatus::Failed,
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

        // Provider filter
        let mut q = base_turns_query();
        q.filter.providers = vec!["anthropic".into()];
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

        // Status filter (TurnStatus Display: length)
        let mut q = base_turns_query();
        q.statuses = vec!["length".into()];
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
            "anthropic",
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
        user_call.provider = ProviderFormat::Anthropic;
        user_call.request_body =
            Some(r#"{"messages":[{"role":"user","content":"DB-USER-FULL"}]}"#.into());
        let mut asst_call = mk_call_with_time("c-asst", base + 2_000);
        asst_call.provider = ProviderFormat::Anthropic;
        asst_call.response_body =
            Some(r#"{"content":[{"type":"text","text":"DB-ASSISTANT-FULL"}]}"#.into());
        backend
            .write_calls(vec![user_call, asst_call])
            .await
            .unwrap();

        let mut turn = sample_turn(
            "t-short",
            "s-short",
            "anthropic",
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
        user_call.provider = ProviderFormat::Anthropic;
        user_call.request_body = Some(
            serde_json::json!({
                "messages": [{ "role": "user", "content": &full_user }]
            })
            .to_string(),
        );
        let mut asst_call = mk_call_with_time("c-asst", base + 2_000);
        asst_call.provider = ProviderFormat::Anthropic;
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
            "anthropic",
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
            "openai",
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
}

#[cfg(test)]
mod concurrent_tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use ts_llm::model::{ApiType, FinishReason, ProviderFormat};
    use ts_metrics::model::LlmMetric;
    use ts_turn::{LlmTurn, TurnStatus};

    fn mk_call(i: usize) -> LlmCall {
        LlmCall {
            stream_id: String::new(),
            id: format!("call-{i:08}"),
            provider: ProviderFormat::OpenAI,
            model: "gpt-4".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 1_700_000_000_000_000 + i as i64,
            response_time: None,
            complete_time: None,
            request_path: "/v1/chat/completions".into(),
            is_stream: false,
            request_body: None,
            status_code: Some(200),
            finish_reason: Some(FinishReason::Complete),
            response_body: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: None,
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

    fn mk_turn(i: usize) -> LlmTurn {
        LlmTurn {
            stream_id: String::new(),
            turn_id: format!("turn-{i:08}"),
            session_id: format!("session-{}", i % 10),
            tenant_id: None,
            provider: "openai".into(),
            client_kind: "test".into(),
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
            stream_id: String::new(),
            granularity: "10s",
            provider: "openai".into(),
            model: "gpt-4".into(),
            server_ip: "10.0.0.2".into(),
            request_count: 1,
            stream_count: 0,
            non_stream_count: 1,
            concurrency_sum: 1,
            concurrency_sample_count: 1,
            concurrency_max: 1,
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
            finish_complete_count: 1,
            finish_length_count: 0,
            finish_tool_use_count: 0,
            finish_error_count: 0,
            finish_cancelled_count: 0,
            ttfb_sum: 0.0,
            ttfb_count: 0,
            ttfb_p50: None,
            ttfb_p95: None,
            ttfb_p99: None,
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
            .query_row("SELECT COUNT(*) FROM llm_turns", [], |r| r.get(0))
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
    use ts_llm::model::{ApiType, FinishReason, ProviderFormat};
    use ts_metrics::model::LlmMetric;
    use ts_turn::{LlmTurn, TurnStatus};

    fn mk_call(id: &str, request_time_us: i64) -> LlmCall {
        LlmCall {
            stream_id: String::new(),
            id: id.into(),
            provider: ProviderFormat::OpenAI,
            model: "gpt-4".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: request_time_us,
            response_time: Some(request_time_us + 100_000),
            complete_time: Some(request_time_us + 500_000),
            request_path: "/v1/chat/completions".into(),
            is_stream: false,
            request_body: None,
            status_code: Some(200),
            finish_reason: Some(FinishReason::Complete),
            response_body: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: Some(100.0),
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

    fn mk_turn(id: &str, start_us: i64, duration_ms: u64) -> LlmTurn {
        LlmTurn {
            stream_id: String::new(),
            turn_id: id.into(),
            session_id: "s".into(),
            tenant_id: None,
            provider: "openai".into(),
            client_kind: "claude-cli".into(),
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

    fn mk_metric(granularity: &'static str, ts_us: i64) -> LlmMetric {
        LlmMetric {
            timestamp_us: ts_us,
            stream_id: String::new(),
            granularity,
            provider: "openai".into(),
            model: "gpt-4".into(),
            server_ip: "10.0.0.2".into(),
            request_count: 1,
            stream_count: 0,
            non_stream_count: 1,
            concurrency_sum: 1,
            concurrency_sample_count: 1,
            concurrency_max: 1,
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
            finish_complete_count: 1,
            finish_length_count: 0,
            finish_tool_use_count: 0,
            finish_error_count: 0,
            finish_cancelled_count: 0,
            ttfb_sum: 0.0,
            ttfb_count: 0,
            ttfb_p50: None,
            ttfb_p95: None,
            ttfb_p99: None,
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

        let policy = RetentionPolicy {
            calls_before: Some(now - Duration::from_secs(7 * 86_400)),
            turns_before: Some(now - Duration::from_secs(14 * 86_400)),
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
            .query_row("SELECT COUNT(*) FROM llm_turns", [], |r| r.get(0))
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
