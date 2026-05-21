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
use ts_metrics::model::{LlmFinishMetric, LlmMetric};
use ts_protocol::HttpExchange;
use ts_turn::{AgentTurn, PairCandidate};

use ts_storage::query::*;
use ts_storage::retention::{RetentionPolicy, RetentionReport};
use ts_storage::StorageBackend;

use pool::ReadPool;

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

    async fn query_turn_calls(
        &self,
        turn_id: &str,
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        DuckDbBackend::query_turn_calls(self, turn_id, include_bodies).await
    }

    async fn query_calls_by_ids(
        &self,
        call_ids: &[String],
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        DuckDbBackend::query_calls_by_ids(self, call_ids, include_bodies).await
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

    async fn query_distinct_agent_kinds(
        &self,
        start_us: i64,
        end_us: i64,
    ) -> Result<Vec<String>> {
        DuckDbBackend::query_distinct_agent_kinds(self, start_us, end_us).await
    }

    async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
        DuckDbBackend::query_distinct_finish_reasons(self).await
    }

    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        DuckDbBackend::apply_retention(self, policy).await
    }

    async fn query_pair_candidates(
        &self,
        start_us: i64,
        end_us: i64,
    ) -> Result<Vec<PairCandidate>> {
        DuckDbBackend::query_pair_candidates(self, start_us, end_us).await
    }

    async fn update_turn_metadata(
        &self,
        turn_id: &str,
        patch: serde_json::Value,
    ) -> Result<()> {
        DuckDbBackend::update_turn_metadata(self, turn_id, patch).await
    }
}
