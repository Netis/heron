//! Retention sweeper — deletes rows older than per-table cutoffs.

use ts_common::error::{AppError, Result};
use ts_storage::retention::{RetentionPolicy, RetentionReport};

use crate::util::timestamp_value;
use crate::DuckDbBackend;

impl DuckDbBackend {
    pub(crate) async fn apply_retention(&self, policy: RetentionPolicy) -> Result<RetentionReport> {
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

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use std::net::IpAddr;
    use std::time::{Duration, SystemTime};
    use ts_llm::model::{ApiType, LlmCall};
    use ts_llm::wire_apis as wa;
    use ts_metrics::model::{LlmFinishMetric, LlmMetric};
    use ts_storage::retention::RetentionPolicy;
    use ts_storage::StorageBackend;
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
            is_agent_request: false,
            tool_surface: None,
            agent_topology: None,
            tool_call_count: 0,
            tool_names: vec![],
        }
    }

    fn mk_turn(id: &str, start_us: i64, duration_ms: u64) -> AgentTurn {
        AgentTurn {
            source_id: String::new(),
            turn_id: id.into(),
            session_id: "s".into(),
            wire_api: wa::OPENAI_CHAT.into(),
            agent_kind: "claude-cli".into(),
            client_ip: "127.0.0.1".parse().unwrap(),
            server_ip: "127.0.0.1".parse().unwrap(),
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
            tool_surfaces: vec![],
            tool_call_total: 0,
            agent_topology: None,
            suspicious_skills: vec![],
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
            ttft_stream_sum: 0.0,
            ttft_stream_count: 0,
            ttft_stream_p50: None,
            ttft_stream_p95: None,
            ttft_stream_p99: None,
            ttft_nonstream_sum: 0.0,
            ttft_nonstream_count: 0,
            ttft_nonstream_p50: None,
            ttft_nonstream_p95: None,
            ttft_nonstream_p99: None,
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
            tool_surface: None,
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
