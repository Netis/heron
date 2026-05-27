use async_trait::async_trait;
use ts_llm::model::LlmCall;
use ts_metrics::model::{LlmFinishMetric, LlmMetric};
use ts_protocol::HttpExchange;
use ts_turn::{AgentTurn, PairCandidate};

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

    /// Batch-write LlmFinishMetric records into the long-format
    /// `llm_finish_metrics` table.
    async fn write_finish_metrics(&self, metrics: Vec<LlmFinishMetric>) -> Result<()>;

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

    /// Aggregate `llm_calls` by `(server_ip, server_port)` to produce
    /// one row per LLM-serving endpoint. Used by the Services page.
    ///
    /// Not served off the pre-aggregated `llm_metrics` table because
    /// that schema's grouping sets stop at `server_ip` — different
    /// vLLM instances on the same host (port 8000 / 9000) would
    /// collapse into one row. Worst-case this scans `llm_calls` rows
    /// in the time window; the user's typical 7-day window has tens of
    /// thousands of rows and the query completes in well under a
    /// second.
    async fn query_services(&self, query: &ServicesQuery) -> Result<Vec<ServiceRow>>;

    /// Build the service→service directed graph used by the Services
    /// page's Path view. Pulls the same per-endpoint node set as
    /// `query_services` (just call counts; perf stats aren't needed for
    /// a graph view), then adds two edge kinds:
    ///   * `proxy` — pair_sweeper-confirmed proxy hops between two
    ///     real endpoints. Definitive (we observed both legs of the
    ///     same turn group on the wire).
    ///   * `client` — synthetic edges from a `__clients__` super-node
    ///     into every service that has at least one non-proxy-out
    ///     turn in the window (i.e., the service receives traffic
    ///     directly, not just from another service). Lets us draw a
    ///     complete graph even when no proxy hop was observed.
    async fn query_services_topology(
        &self,
        query: &ServicesTopologyQuery,
    ) -> Result<ServicesTopology>;

    /// Aggregate `agent_turns` by `agent_kind` over the given window.
    /// Powers the Overview "agent distribution" horizontal-bar chart.
    async fn query_agent_summary(&self, query: &AgentSummaryQuery)
        -> Result<Vec<AgentKindSummary>>;

    /// Per-bucket agent_turn counts split by `agent_kind`. Powers
    /// the Overview "agent activity" stacked time-series chart.
    async fn query_agent_activity(
        &self,
        query: &AgentActivityQuery,
    ) -> Result<Vec<AgentActivityPoint>>;

    /// Per-bucket finish-reason counts in the requested time range. One series
    /// per distinct raw `finish_reason` observed. The `wire_api`/`model`
    /// filters select a specific dimension; `None` rolls up across all values
    /// via the pre-aggregated `*` tier in `llm_finish_metrics`.
    async fn query_finish_reasons(
        &self,
        query: &FinishReasonsQuery,
    ) -> Result<Vec<FinishReasonTimeseries>>;
    async fn query_calls(&self, query: &CallsQuery) -> Result<CallsPage>;
    async fn query_call_by_id(&self, id: &str) -> Result<Option<CallDetail>>;
    async fn query_turns(&self, query: &TurnsQuery) -> Result<TurnsPage>;
    async fn query_turn_by_id(&self, turn_id: &str) -> Result<Option<TurnDetail>>;
    /// `include_bodies = false` makes the four heavy fields
    /// (`request_body`, `response_body`, `request_headers`,
    /// `response_headers`) come back as `None`. On mega-turns (878
    /// agentic iterations × ~190 KB request_body each ≈ 168 MB JSON)
    /// the body-bearing response freezes browsers; lite mode keeps the
    /// summary < 1 MB. `tokens_estimated` cannot be derived without
    /// the response body and defaults to `false` in lite mode.
    async fn query_turn_calls(
        &self,
        turn_id: &str,
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>>;
    /// Sister of `query_turn_calls` for in-progress turns: the API
    /// already knows the call_ids (from the in-memory active-turn
    /// registry) and only needs Step 2 of the join. Returns the same
    /// `TurnCallItem` shape so the frontend's calls panel renders
    /// identically whether the turn is still in progress or finalized.
    /// Calls not yet flushed from `WriteBuffer` to `llm_calls` are
    /// silently skipped — they appear on the next refresh.
    async fn query_calls_by_ids(
        &self,
        call_ids: &[String],
        include_bodies: bool,
    ) -> Result<Vec<TurnCallItem>>;

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
    async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<SessionTurnsPage>;
    async fn query_distinct_wire_apis(&self) -> Result<Vec<String>>;
    async fn query_distinct_models(&self) -> Result<Vec<String>>;
    async fn query_distinct_server_ips(&self) -> Result<Vec<String>>;
    async fn query_distinct_agent_kinds(
        &self,
        query: &DistinctAgentKindsQuery,
    ) -> Result<Vec<String>>;

    /// Distinct `(wire_api, finish_reason)` pairs observed in
    /// `llm_finish_metrics`. Excludes the `*` rollup tiers. Used by the calls
    /// page filter dropdown to populate options dynamically — values are raw
    /// provider strings, grouped on the frontend by `wire_api`.
    async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>>;

    /// Delete rows older than the cutoffs in `policy`. `None` cutoffs are
    /// skipped. Returns per-table / per-granularity row counts.
    async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport>;

    // ---- llmproxy pair-detection support ----
    //
    // The sweeper polls a sliding window of recently-finalized turns,
    // skipping those that already carry `metadata.proxy.role`, and runs
    // `ts_turn::pair_all` over the projection. For each pair it writes a
    // JSON patch into both turns' `metadata` field.

    /// Return a minimal projection of `agent_turns` rows in
    /// `[start_us, end_us)` suitable for `ts_turn::pair_all`. Rows whose
    /// `metadata` already carries `proxy.role` are excluded — once a turn
    /// is paired we don't want to re-pair it on the next sweep.
    /// Default: empty (in-memory mock backends used by tests have no
    /// pairing to surface).
    async fn query_pair_candidates(
        &self,
        _start_us: i64,
        _end_us: i64,
    ) -> Result<Vec<PairCandidate>> {
        Ok(Vec::new())
    }

    /// Merge `patch` into the existing `metadata` JSON of `turn_id`
    /// (top-level shallow merge — only keys present in `patch` are
    /// replaced). Used by the pair sweeper to write `proxy.role` /
    /// `proxy.pair_id` / `proxy.peer_turn_id` back to both legs.
    /// Returns `Ok(())` even if `turn_id` doesn't exist — the sweeper
    /// races finalization and a turn may briefly be unwritten when the
    /// patch arrives.
    async fn update_turn_metadata(&self, _turn_id: &str, _patch: serde_json::Value) -> Result<()> {
        Ok(())
    }

    /// Compact pending MVCC tombstones on the agent_turns writer.
    /// Called by the pair sweeper after each batch of
    /// `update_turn_metadata` so the version chain stays short —
    /// high-frequency UPDATEs on an indexed table (PRIMARY KEY on
    /// `turn_id`) without checkpoints can hit a "Failed to delete all
    /// rows from index" FATAL inside DuckDB that poisons the entire
    /// process's connection. Default no-op for mock backends.
    async fn checkpoint_turns_writer(&self) -> Result<()> {
        Ok(())
    }

    /// Replace **every** connection the backend holds — all writer
    /// mutexes and every reader-pool entry — with freshly-opened
    /// handles to the on-disk database. Called by the pair sweeper
    /// after a sweep failure that looks like the DuckDB "database has
    /// been invalidated" FATAL.
    ///
    /// Reopening only one connection isn't enough: DuckDB's
    /// in-process invalidation propagates to every `try_clone()`d
    /// handle, so any reader still cloned from the original anchor
    /// will keep returning the same FATAL on the next query. Until
    /// the broken anchor has no remaining handles, the process is
    /// poisoned and every API endpoint that hits the read pool keeps
    /// returning HTTP 500 — even after the turns writer alone has
    /// been swapped. The on-disk file is intact; only the in-process
    /// MVCC/index state is corrupted, so opening a fresh anchor from
    /// the same path recovers cleanly.
    ///
    /// Default no-op for mock backends.
    async fn reopen_all_connections(&self) -> Result<()> {
        Ok(())
    }
}
