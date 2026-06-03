//! ClickHouse implementation of [`StorageBackend`].
//!
//! Drop-in alternative to `h-storage-duckdb` for large-scale, high-throughput
//! deployments. Talks to ClickHouse over the HTTP interface via the
//! `clickhouse` crate (async-native, serde RowBinary), so — unlike the DuckDB
//! backend — there is no `spawn_blocking`, no writer-mutex set, and no reader
//! pool: the `Client` is a cheap-to-clone HTTP connection pool.
//!
//! Module layout mirrors `h-storage-duckdb` 1:1 so the two backends diff side
//! by side:
//!   * `schema`   — DDL + `init()` (MergeTree / ReplacingMergeTree + TTL)
//!   * `rows`     — `#[derive(clickhouse::Row)]` insert/select structs
//!   * `calls` / `metrics` / `turns` / `sessions` / `exchanges` / `distincts`
//!     / `retention` — one module per entity, same split as DuckDB.

mod calls;
mod client;
mod distincts;
mod exchanges;
#[cfg(test)]
mod it;
mod metrics;
mod retention;
mod rows;
mod schema;
mod services;
mod sessions;
mod sql;
mod turns;

use async_trait::async_trait;
use clickhouse::Client;

use h_common::config::ClickHouseConfig;
use h_common::error::Result;
use h_llm::model::LlmCall;
use h_metrics::model::{LlmFinishMetric, LlmMetric};
use h_protocol::HttpExchange;
use h_turn::{AgentTurn, PairCandidate};

use h_storage::query::*;
use h_storage::retention::{RetentionPolicy, RetentionReport};
use h_storage::StorageBackend;

/// ClickHouse storage backend. Holds a single shared HTTP `Client` (internally
/// `Arc`-backed) plus the resolved connection params and sweep behaviour.
///
/// The raw `url` / `user` / `password` are kept so `init()` can build a
/// database-unscoped "admin" client for `CREATE DATABASE` — a `?database=heron`
/// connection errors with `UNKNOWN_DATABASE` until the database exists.
pub struct ClickHouseBackend {
    pub(crate) client: Client,
    pub(crate) url: String,
    pub(crate) user: String,
    pub(crate) password: String,
    /// Resolved target database; bound on `client` and created by `init()`.
    pub(crate) database: String,
    /// When true, `apply_retention` follows each DELETE with `OPTIMIZE FINAL`.
    pub(crate) optimize_on_sweep: bool,
}

impl ClickHouseBackend {
    /// Build a backend from config. Construction is cheap and does not perform
    /// any network I/O — the schema is created later by `init()` (driven by the
    /// same startup path that calls `DuckDbBackend`'s `init()`).
    pub fn new(config: &ClickHouseConfig) -> Result<Self> {
        let client = client::build_client(
            &config.url,
            &config.user,
            &config.password,
            Some(&config.database),
        );
        Ok(Self {
            client,
            url: config.url.clone(),
            user: config.user.clone(),
            password: config.password.clone(),
            database: config.database.clone(),
            optimize_on_sweep: config.optimize_on_sweep,
        })
    }
}

#[async_trait]
impl StorageBackend for ClickHouseBackend {
    async fn init(&self) -> Result<()> {
        schema::init(self).await
    }

    async fn write_calls(&self, calls: Vec<LlmCall>) -> Result<()> {
        ClickHouseBackend::write_calls(self, calls).await
    }

    async fn write_metrics(&self, metrics: Vec<LlmMetric>) -> Result<()> {
        ClickHouseBackend::write_metrics(self, metrics).await
    }

    async fn write_finish_metrics(&self, metrics: Vec<LlmFinishMetric>) -> Result<()> {
        ClickHouseBackend::write_finish_metrics(self, metrics).await
    }

    async fn write_turns(&self, turns: Vec<AgentTurn>) -> Result<()> {
        ClickHouseBackend::write_turns(self, turns).await
    }

    async fn write_exchanges(&self, exchanges: Vec<HttpExchange>) -> Result<()> {
        ClickHouseBackend::write_exchanges(self, exchanges).await
    }

    async fn query_http_exchange_by_id(&self, id: &str) -> Result<Option<HttpExchangeDetail>> {
        ClickHouseBackend::query_http_exchange_by_id(self, id).await
    }

    async fn query_http_exchanges(
        &self,
        query: &HttpExchangesQuery,
    ) -> Result<HttpExchangesPage> {
        ClickHouseBackend::query_http_exchanges(self, query).await
    }

    async fn query_metrics_timeseries(
        &self,
        query: &MetricsTimeseriesQuery,
    ) -> Result<Vec<MetricsTimeseriesRow>> {
        ClickHouseBackend::query_metrics_timeseries(self, query).await
    }

    async fn query_metrics_summary(
        &self,
        query: &MetricsSummaryQuery,
    ) -> Result<MetricsSummaryRow> {
        ClickHouseBackend::query_metrics_summary(self, query).await
    }

    async fn query_metrics_models(
        &self,
        query: &MetricsModelsQuery,
    ) -> Result<Vec<MetricsModelRow>> {
        ClickHouseBackend::query_metrics_models(self, query).await
    }

    async fn query_services(&self, query: &ServicesQuery) -> Result<Vec<ServiceRow>> {
        ClickHouseBackend::query_services(self, query).await
    }

    async fn query_services_topology(
        &self,
        query: &ServicesTopologyQuery,
    ) -> Result<ServicesTopology> {
        ClickHouseBackend::query_services_topology(self, query).await
    }

    async fn query_agent_summary(
        &self,
        query: &AgentSummaryQuery,
    ) -> Result<Vec<AgentKindSummary>> {
        ClickHouseBackend::query_agent_summary(self, query).await
    }

    async fn query_agent_activity(
        &self,
        query: &AgentActivityQuery,
    ) -> Result<Vec<AgentActivityPoint>> {
        ClickHouseBackend::query_agent_activity(self, query).await
    }

    async fn query_finish_reasons(
        &self,
        query: &FinishReasonsQuery,
    ) -> Result<Vec<FinishReasonTimeseries>> {
        ClickHouseBackend::query_finish_reasons(self, query).await
    }

    async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage> {
        ClickHouseBackend::query_calls(self, query).await
    }

    async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>> {
        ClickHouseBackend::query_call_by_id(self, id).await
    }

    async fn query_turns(&self, query: &TurnsQuery) -> Result<TurnsPage> {
        ClickHouseBackend::query_turns(self, query).await
    }

    async fn query_turn_by_id(&self, turn_id: &str) -> Result<Option<TurnDetail>> {
        ClickHouseBackend::query_turn_by_id(self, turn_id).await
    }

    async fn query_turn_calls(
        &self,
        turn_id: &str,
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        ClickHouseBackend::query_turn_calls(self, turn_id, include_bodies).await
    }

    async fn query_calls_by_ids(
        &self,
        call_ids: &[String],
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>> {
        ClickHouseBackend::query_calls_by_ids(self, call_ids, include_bodies).await
    }

    async fn query_sessions(&self, query: &SessionListQuery) -> Result<SessionsPage> {
        ClickHouseBackend::query_sessions(self, query).await
    }

    async fn query_session_by_id(
        &self,
        source_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionDetail>> {
        ClickHouseBackend::query_session_by_id(self, source_id, session_id).await
    }

    async fn query_session_turns(
        &self,
        query: &SessionTurnsQuery,
    ) -> Result<SessionTurnsPage> {
        ClickHouseBackend::query_session_turns(self, query).await
    }

    async fn query_distinct_wire_apis(&self) -> Result<Vec<String>> {
        ClickHouseBackend::query_distinct_wire_apis(self).await
    }

    async fn query_distinct_models(&self) -> Result<Vec<String>> {
        ClickHouseBackend::query_distinct_models(self).await
    }

    async fn query_distinct_server_ips(&self) -> Result<Vec<String>> {
        ClickHouseBackend::query_distinct_server_ips(self).await
    }

    async fn query_distinct_agent_kinds(
        &self,
        query: &DistinctAgentKindsQuery,
    ) -> Result<Vec<String>> {
        ClickHouseBackend::query_distinct_agent_kinds(self, query).await
    }

    async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
        ClickHouseBackend::query_distinct_finish_reasons(self).await
    }

    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
        ClickHouseBackend::apply_retention(self, policy).await
    }

    async fn query_pair_candidates(
        &self,
        start_us: i64,
        end_us: i64,
    ) -> Result<Vec<PairCandidate>> {
        ClickHouseBackend::query_pair_candidates(self, start_us, end_us).await
    }

    async fn update_turn_metadata(&self, turn_id: &str, patch: serde_json::Value) -> Result<()> {
        ClickHouseBackend::update_turn_metadata(self, turn_id, patch).await
    }

    // checkpoint_turns_writer / reopen_all_connections use the trait's default
    // no-op impls: the clickhouse `Client` is a cheap HTTP pool with no
    // in-process MVCC/index state to compact or reopen.
}
