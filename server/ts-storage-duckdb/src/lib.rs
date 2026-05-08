mod calls;
#[cfg(test)]
mod concurrent_tests;
mod distincts;
mod exchanges;
mod metrics;
mod pool;
mod retention;
mod schema;
mod sessions;
mod turns;
mod util;

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use duckdb::Connection;
use tracing::info;
use ts_common::error::{AppError, Result};
use ts_llm::model::LlmCall;
#[cfg(test)]
use ts_llm::wire_apis as wa;
use ts_metrics::model::{LlmFinishMetric, LlmMetric};
use ts_protocol::HttpExchange;
use ts_turn::AgentTurn;

use ts_storage::query::*;
use ts_storage::retention::{RetentionPolicy, RetentionReport};
use ts_storage::StorageBackend;

use pool::ReadPool;
#[cfg(test)]
use util::{build_dimension_where, build_dimension_where_for_group};

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
    pub(crate) write_calls_conn: Arc<StdMutex<Connection>>,
    pub(crate) write_turns_conn: Arc<StdMutex<Connection>>,
    pub(crate) write_metrics_conn: Arc<StdMutex<Connection>>,
    pub(crate) write_exchanges_conn: Arc<StdMutex<Connection>>,
    pub(crate) read_pool: ReadPool,
}

impl DuckDbBackend {
    /// Open a DuckDB database at the given path with a default-sized read pool.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_with_pool(path, DEFAULT_READ_POOL_SIZE)
    }

    pub fn open_with_pool(path: &str, read_pool_size: usize) -> Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    AppError::Storage(format!(
                        "failed to create duckdb parent dir {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        }
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

#[async_trait]
impl StorageBackend for DuckDbBackend {
    async fn init(&self) -> Result<()> {
        schema::init(self).await
    }

    async fn write_calls(&self, calls: Vec<LlmCall>) -> Result<()> {
        DuckDbBackend::write_calls(self, calls).await
    }

    async fn write_exchanges(&self, exchanges: Vec<HttpExchange>) -> Result<()> {
        DuckDbBackend::write_exchanges(self, exchanges).await
    }

    async fn query_http_exchange_by_id(&self, id: &str) -> Result<Option<HttpExchangeDetail>> {
        DuckDbBackend::query_http_exchange_by_id(self, id).await
    }

    async fn query_http_exchanges(&self, query: &HttpExchangesQuery) -> Result<HttpExchangesPage> {
        DuckDbBackend::query_http_exchanges(self, query).await
    }

    async fn write_metrics(&self, metrics: Vec<LlmMetric>) -> Result<()> {
        DuckDbBackend::write_metrics(self, metrics).await
    }

    async fn write_finish_metrics(&self, metrics: Vec<LlmFinishMetric>) -> Result<()> {
        DuckDbBackend::write_finish_metrics(self, metrics).await
    }

    async fn write_turns(&self, turns: Vec<AgentTurn>) -> Result<()> {
        DuckDbBackend::write_turns(self, turns).await
    }

    async fn query_metrics_timeseries(
        &self,
        query: &MetricsTimeseriesQuery,
    ) -> Result<Vec<MetricsTimeseriesRow>> {
        DuckDbBackend::query_metrics_timeseries(self, query).await
    }

    async fn query_metrics_summary(
        &self,
        query: &MetricsSummaryQuery,
    ) -> Result<MetricsSummaryRow> {
        DuckDbBackend::query_metrics_summary(self, query).await
    }

    async fn query_metrics_models(
        &self,
        query: &MetricsModelsQuery,
    ) -> Result<Vec<MetricsModelRow>> {
        DuckDbBackend::query_metrics_models(self, query).await
    }

    async fn query_finish_reasons(
        &self,
        query: &FinishReasonsQuery,
    ) -> Result<Vec<FinishReasonTimeseries>> {
        DuckDbBackend::query_finish_reasons(self, query).await
    }

    async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage> {
        DuckDbBackend::query_calls(self, query).await
    }

    async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>> {
        DuckDbBackend::query_call_by_id(self, id).await
    }

    async fn query_turns(&self, query: &TurnsQuery) -> Result<TurnsPage> {
        DuckDbBackend::query_turns(self, query).await
    }

    async fn query_turn_by_id(&self, turn_id: &str) -> Result<Option<TurnDetail>> {
        DuckDbBackend::query_turn_by_id(self, turn_id).await
    }

    async fn query_turn_calls(&self, turn_id: &str) -> Result<Vec<TurnCallItem>> {
        DuckDbBackend::query_turn_calls(self, turn_id).await
    }

    async fn query_calls_by_ids(&self, call_ids: &[String]) -> Result<Vec<TurnCallItem>> {
        DuckDbBackend::query_calls_by_ids(self, call_ids).await
    }

    async fn query_sessions(&self, query: &SessionListQuery) -> Result<SessionsPage> {
        DuckDbBackend::query_sessions(self, query).await
    }

    async fn query_session_by_id(
        &self,
        source_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionDetail>> {
        DuckDbBackend::query_session_by_id(self, source_id, session_id).await
    }

    async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<SessionTurnsPage> {
        DuckDbBackend::query_session_turns(self, query).await
    }

    async fn query_distinct_wire_apis(&self) -> Result<Vec<String>> {
        DuckDbBackend::query_distinct_wire_apis(self).await
    }

    async fn query_distinct_models(&self) -> Result<Vec<String>> {
        DuckDbBackend::query_distinct_models(self).await
    }

    async fn query_distinct_server_ips(&self) -> Result<Vec<String>> {
        DuckDbBackend::query_distinct_server_ips(self).await
    }

    async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
        DuckDbBackend::query_distinct_finish_reasons(self).await
    }

    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        DuckDbBackend::apply_retention(self, policy).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts_storage::StorageBackend;
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

    /// Peak fields (`*_max`) must MAX, not SUM, across rows at the same
    /// timestamp — otherwise per-source local peaks stack into an inflated
    /// global peak.
    #[tokio::test]
    async fn test_active_calls_max_uses_max_across_sources() {
        let backend = in_memory_backend();
        backend.init().await.unwrap();

        let ts = 1_700_000_000_000_000i64;

        let mut s0 = sample_metric();
        s0.timestamp_us = ts;
        s0.source_id = "s0".into();
        s0.granularity = "1m";
        s0.wire_api = "*".into();
        s0.model = "*".into();
        s0.server_ip = "*".into();
        s0.active_calls_max = 5;

        let mut s1 = sample_metric();
        s1.timestamp_us = ts;
        s1.source_id = "s1".into();
        s1.granularity = "1m";
        s1.wire_api = "*".into();
        s1.model = "*".into();
        s1.server_ip = "*".into();
        s1.active_calls_max = 7;

        backend.write_metrics(vec![s0, s1]).await.unwrap();

        let query = MetricsTimeseriesQuery {
            time_range: TimeRange {
                start_us: ts,
                end_us: ts + 120_000_000,
            },
            granularity: "1m".to_string(),
            filter: DimensionFilter::default(),
            fields: vec!["active_calls_max".to_string()],
            group_by: None,
        };

        let rows = backend.query_metrics_timeseries(&query).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values[0],
            Some(7.0),
            "active_calls_max must MAX(5, 7) = 7, not SUM = 12"
        );
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
