use async_trait::async_trait;
use ts_llm::model::LlmCall;
use ts_metrics::model::LlmMetric;
use ts_turn::LlmTurn;

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

    /// Batch-write LlmTurn records.
    async fn write_turns(&self, turns: Vec<LlmTurn>) -> Result<()>;

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
    async fn query_distinct_providers(&self) -> Result<Vec<String>>;
    async fn query_distinct_models(&self) -> Result<Vec<String>>;
    async fn query_distinct_server_ips(&self) -> Result<Vec<String>>;

    /// Delete rows older than the cutoffs in `policy`. `None` cutoffs are
    /// skipped. Returns per-table / per-granularity row counts.
    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport>;
}
