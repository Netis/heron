//! Background pair-detection sweeper.
//!
//! Runs as a long-lived tokio task spawned next to the storage sink.
//! On each tick it asks the storage backend for a window of finalized
//! turns whose `metadata.proxy.role` is unset, runs the in-memory
//! `pair_all` classifier, and writes the resulting pair annotations
//! back via `update_turn_metadata`. The sweep is fully idempotent: a
//! turn that is already paired is excluded by the backend query, so
//! repeat sweeps converge.
//!
//! ### Scheduling
//!
//! Two clocks drive the sweep:
//!
//! * `interval` — how often to wake up.
//! * `lookback` — how far back to scan on each wake. The window starts
//!   at `now - lookback` and ends at `now`. Lookback must comfortably
//!   exceed the worst-case latency between the two legs of a pair
//!   landing in the DB so we never miss a peer. Tracker grace is 1s by
//!   default and the storage sink flush interval is ~100ms, so 5
//!   minutes is generous headroom — keep it well above the slowest
//!   plausible proxy round-trip and finalization-grace combined.
//!
//! ### Why a sweeper (not inline at write-time)
//!
//! When `write_turns` flushes a batch, the peer of a pair may not yet
//! have arrived — it might still be sitting in another shard's buffer
//! waiting for grace. Inline pairing at write time would systematically
//! miss the first of any pair. A sweeper that scans recently-finalized
//! turns naturally absorbs that latency.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use ts_turn::proxy_pair::group_all;

use crate::StorageBackend;

/// Configuration for the pair sweeper.
#[derive(Debug, Clone, Copy)]
pub struct PairSweeperConfig {
    /// Cadence between sweeps. Default 2s — proxy hops only pair after
    /// both legs flush, and the user has no expectation of sub-second
    /// folding on the agent-turn list.
    pub interval: Duration,
    /// How far back to scan on each sweep. The query is indexed on
    /// `start_time`, so a generous lookback is cheap. Default 5 min.
    pub lookback: Duration,
}

impl Default for PairSweeperConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            // 30min — wide enough to catch a turn whose peer arrives
            // anomalously late (slow proxy + slow flush), but short
            // enough that the sweeper's repeat work per tick stays
            // bounded. The metadata.proxy.role IS NULL filter on the
            // query keeps already-paired turns out of every sweep so
            // there's no fan-out from past pairs.
            lookback: Duration::from_secs(1800),
        }
    }
}

/// Spawn the sweeper. Runs until `storage` is dropped from every other
/// holder AND the inner sleep wakes — in practice the task lives for the
/// process lifetime. Errors during a sweep are logged and the sweeper
/// continues; one bad sweep should never take down the pipeline.
pub fn spawn_pair_sweeper(
    cfg: PairSweeperConfig,
    storage: Arc<dyn StorageBackend>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Stagger the first sweep so it doesn't fire the same instant as
        // service start (the DB may still be empty / cold).
        tokio::time::sleep(cfg.interval).await;
        loop {
            if let Err(e) = sweep_once(&storage, cfg.lookback).await {
                tracing::warn!(error = %e, "pair-sweeper: sweep failed; continuing");
            }
            tokio::time::sleep(cfg.interval).await;
        }
    })
}

/// One sweep iteration. Pulled out for testability — callers can drive
/// the sweeper from synthetic data without spawning a task.
pub async fn sweep_once(
    storage: &Arc<dyn StorageBackend>,
    lookback: Duration,
) -> ts_common::error::Result<SweepStats> {
    let now_us = chrono::Utc::now().timestamp_micros();
    let start_us = now_us - (lookback.as_micros() as i64);
    let candidates = storage.query_pair_candidates(start_us, now_us).await?;
    let candidates_scanned = candidates.len();
    let groups = group_all(&candidates);
    let groups_assigned = groups.len();
    let mut turns_tagged = 0usize;
    for g in groups {
        for member in &g.members {
            let patch = g
                .metadata_for(&member.turn_id)
                .expect("member belongs to group it came from");
            storage
                .update_turn_metadata(&member.turn_id, patch)
                .await?;
            turns_tagged += 1;
        }
    }
    Ok(SweepStats {
        candidates_scanned,
        pairs_assigned: groups_assigned,
        turns_tagged,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    pub candidates_scanned: usize,
    /// Number of distinct logical-call groups (each containing 2+
    /// turns). Reads as "how many duplicates folded".
    pub pairs_assigned: usize,
    /// Total per-turn metadata writes (one per group member). Useful
    /// for distinguishing "1 fat 3-leg group" from "3 mirror pairs"
    /// in metrics.
    pub turns_tagged: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use ts_common::error::Result;
    use ts_llm::model::LlmCall;
    use ts_metrics::model::{LlmFinishMetric, LlmMetric};
    use ts_protocol::HttpExchange;
    use crate::query::*;
    use crate::retention::{RetentionPolicy, RetentionReport};

    use ts_turn::proxy_pair::PairCandidate;
    use ts_turn::AgentTurn;

    /// In-memory stub storage that holds the candidates the sweeper will
    /// see and records the metadata patches it writes back. Lets us test
    /// the sweeper without a real DB.
    struct StubStorage {
        candidates: Vec<PairCandidate>,
        updates: StdMutex<HashMap<String, serde_json::Value>>,
    }

    #[async_trait]
    impl StorageBackend for StubStorage {
        async fn init(&self) -> Result<()> {
            Ok(())
        }
        async fn write_calls(&self, _: Vec<LlmCall>) -> Result<()> {
            Ok(())
        }
        async fn write_metrics(&self, _: Vec<LlmMetric>) -> Result<()> {
            Ok(())
        }
        async fn write_finish_metrics(&self, _: Vec<LlmFinishMetric>) -> Result<()> {
            Ok(())
        }
        async fn write_turns(&self, _: Vec<AgentTurn>) -> Result<()> {
            Ok(())
        }
        async fn write_exchanges(&self, _: Vec<HttpExchange>) -> Result<()> {
            Ok(())
        }
        async fn query_http_exchange_by_id(
            &self,
            _: &str,
        ) -> Result<Option<HttpExchangeDetail>> {
            Ok(None)
        }
        async fn query_http_exchanges(
            &self,
            _: &HttpExchangesQuery,
        ) -> Result<HttpExchangesPage> {
            Ok(HttpExchangesPage { total: 0, items: vec![] })
        }
        async fn query_metrics_timeseries(
            &self,
            _: &MetricsTimeseriesQuery,
        ) -> Result<Vec<MetricsTimeseriesRow>> {
            Ok(vec![])
        }
        async fn query_metrics_summary(
            &self,
            _: &MetricsSummaryQuery,
        ) -> Result<MetricsSummaryRow> {
            Ok(MetricsSummaryRow {
                call_count: 0,
                error_count: 0,
                error_4xx_count: 0,
                error_429_count: 0,
                error_5xx_count: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                ttft_avg: None,
                e2e_avg: None,
                tpot_avg: None,
            })
        }
        async fn query_metrics_models(
            &self,
            _: &MetricsModelsQuery,
        ) -> Result<Vec<MetricsModelRow>> {
            Ok(vec![])
        }
        async fn query_finish_reasons(
            &self,
            _: &FinishReasonsQuery,
        ) -> Result<Vec<FinishReasonTimeseries>> {
            Ok(vec![])
        }
        async fn query_calls(&self, _: &CallsQuery) -> Result<CallsPage> {
            Ok(CallsPage { total: 0, items: vec![] })
        }
        async fn query_call_by_id(&self, _: &str) -> Result<Option<CallDetail>> {
            Ok(None)
        }
        async fn query_turns(&self, _: &TurnsQuery) -> Result<TurnsPage> {
            Ok(TurnsPage { total: 0, items: vec![] })
        }
        async fn query_turn_by_id(&self, _: &str) -> Result<Option<TurnDetail>> {
            Ok(None)
        }
        async fn query_turn_calls(
            &self,
            _: &str,
            _include_bodies: bool,
        ) -> Result<Vec<TurnCallItem>> {
            Ok(vec![])
        }
        async fn query_calls_by_ids(
            &self,
            _: &[String],
            _include_bodies: bool,
        ) -> Result<Vec<TurnCallItem>> {
            Ok(vec![])
        }
        async fn query_sessions(&self, _: &SessionListQuery) -> Result<SessionsPage> {
            Ok(SessionsPage { items: vec![], next_cursor: None })
        }
        async fn query_session_by_id(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<SessionDetail>> {
            Ok(None)
        }
        async fn query_session_turns(
            &self,
            _: &SessionTurnsQuery,
        ) -> Result<SessionTurnsPage> {
            Ok(SessionTurnsPage { items: vec![], next_cursor: None })
        }
        async fn query_distinct_wire_apis(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn query_distinct_models(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn query_distinct_server_ips(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn query_distinct_agent_kinds(
            &self,
            _start_us: i64,
            _end_us: i64,
        ) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn query_distinct_finish_reasons(&self) -> Result<Vec<DistinctFinishReason>> {
            Ok(vec![])
        }
        async fn apply_retention(&self, _: RetentionPolicy) -> Result<RetentionReport> {
            Ok(RetentionReport::default())
        }
        async fn query_pair_candidates(
            &self,
            _: i64,
            _: i64,
        ) -> Result<Vec<PairCandidate>> {
            Ok(self.candidates.clone())
        }
        async fn update_turn_metadata(
            &self,
            turn_id: &str,
            patch: serde_json::Value,
        ) -> Result<()> {
            self.updates.lock().unwrap().insert(turn_id.into(), patch);
            Ok(())
        }
    }

    fn cand(id: &str, sid: &str, start: i64, end: i64, view: &str) -> PairCandidate {
        PairCandidate {
            turn_id: id.into(),
            session_id: sid.into(),
            agent_kind: "openclaw".into(),
            wire_api: "openai-chat".into(),
            start_time_us: start,
            end_time_us: end,
            call_count: 1,
            total_input_tokens: 100,
            total_output_tokens: 10,
            final_finish_reason: Some("stop".into()),
            primary_model: Some("GLM-5.1".into()),
            network_view: view.into(),
        }
    }

    #[tokio::test]
    async fn sweep_once_writes_metadata_for_matched_pair() {
        // Real-data shape: outer leg encloses inner, network views differ.
        let stub = Arc::new(StubStorage {
            candidates: vec![
                cand("outer", "S", 100_000, 200_000, "client->host"),
                cand("inner", "S", 102_000, 199_000, "bridge->upstream"),
            ],
            updates: StdMutex::new(HashMap::new()),
        }) as Arc<dyn StorageBackend>;
        let stats = sweep_once(&stub, Duration::from_secs(60)).await.unwrap();
        assert_eq!(stats.pairs_assigned, 1);
        assert_eq!(stats.candidates_scanned, 2);

        // The second test verifies the actual patch contents written
        // through the stub. Here we just assert pair detection ran end
        // to end — the sweeper only counts a pair after BOTH
        // update_turn_metadata calls succeeded.
    }

    #[tokio::test]
    async fn sweep_once_assigns_proxy_in_to_outer_leg() {
        // Recreate the verified wuneng haproxy_glm5 pair shape and
        // check the proxy_in role lands on the outer leg.
        let stub_inner = Arc::new(StubStorage {
            candidates: vec![
                cand("outer", "S", 348_294_000, 350_588_000, "client->host"),
                cand("inner", "S", 348_296_000, 350_587_000, "bridge->upstream"),
            ],
            updates: StdMutex::new(HashMap::new()),
        });
        let stub = stub_inner.clone() as Arc<dyn StorageBackend>;
        let stats = sweep_once(&stub, Duration::from_secs(3600)).await.unwrap();
        assert_eq!(stats.pairs_assigned, 1);
        assert_eq!(stats.turns_tagged, 2);
        let updates = stub_inner.updates.lock().unwrap();
        let outer_patch = updates.get("outer").expect("outer patched");
        let inner_patch = updates.get("inner").expect("inner patched");
        assert_eq!(outer_patch["proxy"]["role"], "proxy_in");
        assert_eq!(inner_patch["proxy"]["role"], "proxy_out");
        // pair_id is shared between members of the same group.
        assert_eq!(outer_patch["proxy"]["pair_id"], inner_patch["proxy"]["pair_id"]);
        // peer_turn_ids cross-references (legacy peer_turn_id matches first peer).
        assert_eq!(outer_patch["proxy"]["peer_turn_id"], "inner");
        assert_eq!(inner_patch["proxy"]["peer_turn_id"], "outer");
        // peer_turn_ids is an array with the right peer for each member.
        assert_eq!(outer_patch["proxy"]["peer_turn_ids"], serde_json::json!(["inner"]));
        assert_eq!(inner_patch["proxy"]["peer_turn_ids"], serde_json::json!(["outer"]));
    }

    #[tokio::test]
    async fn sweep_once_folds_three_leg_haproxy_topology() {
        // The user's actual production scenario: br0 + docker0 mirror
        // pair PLUS the real upstream forward leg, all under the same
        // session. All three must wind up in one group so the default
        // list shows ONE row.
        let stub_inner = Arc::new(StubStorage {
            candidates: vec![
                cand("a_br0", "S", 1_000_000, 3_000_000, "172.16.103.100->172.16.103.81"),
                cand("b_dock0", "S", 1_000_000, 3_000_000, "172.16.103.100->172.17.0.9"),
                cand("c_hop", "S", 1_002_000, 2_999_000, "172.17.0.1->172.17.0.4"),
            ],
            updates: StdMutex::new(HashMap::new()),
        });
        let stub = stub_inner.clone() as Arc<dyn StorageBackend>;
        let stats = sweep_once(&stub, Duration::from_secs(3600)).await.unwrap();
        assert_eq!(stats.pairs_assigned, 1, "all three legs are one logical call");
        assert_eq!(stats.turns_tagged, 3);
        let updates = stub_inner.updates.lock().unwrap();
        let a = updates.get("a_br0").expect("a_br0 patched");
        let b = updates.get("b_dock0").expect("b_dock0 patched");
        let c = updates.get("c_hop").expect("c_hop patched");
        // Canonical (widest span, lex tiebreak) is a_br0.
        assert_eq!(a["proxy"]["role"], "proxy_in");
        assert_eq!(b["proxy"]["role"], "mirror_secondary");
        assert_eq!(c["proxy"]["role"], "proxy_out");
        // All three share the same group/pair id.
        assert_eq!(a["proxy"]["pair_id"], b["proxy"]["pair_id"]);
        assert_eq!(a["proxy"]["pair_id"], c["proxy"]["pair_id"]);
        // Peer lists exclude self and are sorted lex.
        assert_eq!(a["proxy"]["peer_turn_ids"], serde_json::json!(["b_dock0", "c_hop"]));
        assert_eq!(b["proxy"]["peer_turn_ids"], serde_json::json!(["a_br0", "c_hop"]));
        assert_eq!(c["proxy"]["peer_turn_ids"], serde_json::json!(["a_br0", "b_dock0"]));
    }

    #[tokio::test]
    async fn sweep_once_skips_lone_turns() {
        let stub = Arc::new(StubStorage {
            candidates: vec![cand("solo", "S", 0, 1000, "v")],
            updates: StdMutex::new(HashMap::new()),
        }) as Arc<dyn StorageBackend>;
        let stats = sweep_once(&stub, Duration::from_secs(60)).await.unwrap();
        assert_eq!(stats.pairs_assigned, 0);
        assert_eq!(stats.candidates_scanned, 1);
    }
}
