use async_trait::async_trait;
use ts_llm::model::LlmCall;
use ts_metrics::model::LlmMetric;
use ts_protocol::HttpExchange;
use ts_turn::AgentTurn;

use crate::query::*;
use crate::retention::{RetentionPolicy, RetentionReport};
use ts_common::error::Result;

/// Pluggable storage backend for persisting LLM telemetry data.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Create tables/schemas if they don't exist.
    async fn init(&self) -> Result<()>;

    /// Batch-write LlmCall records. Takes ownership so the backend can move
    /// the batch into a blocking task without an extra clone.
    async fn write_calls(&self, calls: Vec<LlmCall>) -> Result<()>;

    /// Batch-write LlmMetric records.
    async fn write_metrics(&self, metrics: Vec<LlmMetric>) -> Result<()>;

    /// Batch-write AgentTurn records.
    async fn write_turns(&self, turns: Vec<AgentTurn>) -> Result<()>;

    /// Batch-write HttpExchange records. Authoritative transport-layer record
    /// for all HTTP traffic (LLM + non-LLM). Soft-FK'd from `llm_calls` via
    /// `llm_calls.http_correlation_id`.
    async fn write_exchanges(&self, exchanges: Vec<HttpExchange>) -> Result<()>;

    /// Fetch a single HTTP exchange by its primary key.
    async fn query_http_exchange_by_id(&self, id: &str) -> Result<Option<HttpExchangeDetail>>;

    /// Paginated, filterable list of HTTP exchanges. Powers the HTTP
    /// Exchanges page and mirrors `query_calls`'s shape.
    async fn query_http_exchanges(&self, query: &HttpExchangesQuery) -> Result<HttpExchangesPage>;

    async fn query_metrics_timeseries(
        &self,
        query: &MetricsTimeseriesQuery,
    ) -> Result<Vec<MetricsTimeseriesRow>>;
    async fn query_metrics_summary(&self, query: &MetricsSummaryQuery)
        -> Result<MetricsSummaryRow>;
    async fn query_metrics_models(
        &self,
        query: &MetricsModelsQuery,
    ) -> Result<Vec<MetricsModelRow>>;
    async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage>;
    async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>>;
    async fn query_turns(&self, query: &TurnsQuery) -> Result<TurnsPage>;
    async fn query_turn_by_id(&self, turn_id: &str) -> Result<Option<TurnDetail>>;
    async fn query_turn_calls(&self, turn_id: &str) -> Result<Vec<TurnCallItem>>;

    /// Paginated session list (view over `agent_turns`; no materialised
    /// session table). A session is included when at least one of its turns
    /// has `end_time` inside `query.time_range`; returned aggregates cover the
    /// session's full lifetime (not just the window). Sorted by
    /// `last_turn_at_in_window DESC` with cursor pagination.
    async fn query_sessions(&self, query: &SessionListQuery) -> Result<SessionsPage>;

    /// Full-lifetime aggregate for a single session. Returns `None` when no
    /// turns exist for `(source_id, session_id)`.
    async fn query_session_by_id(
        &self,
        source_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionDetail>>;

    /// Paginated list of the session's turns, ordered by start_time DESC. Not
    /// time-windowed — the session detail page shows the full history.
    async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<TurnsPage>;
    async fn query_distinct_wire_apis(&self) -> Result<Vec<String>>;
    async fn query_distinct_models(&self) -> Result<Vec<String>>;
    async fn query_distinct_server_ips(&self) -> Result<Vec<String>>;

    /// Delete rows older than the cutoffs in `policy`. `None` cutoffs are
    /// skipped. Returns per-table / per-granularity row counts.
    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport>;
}
