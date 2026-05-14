//! Distinct-value queries used to populate filter dropdowns.

use ts_common::error::{AppError, Result};

use crate::util::us_to_timestamp;
use crate::DuckDbBackend;

impl DuckDbBackend {
    pub(crate) async fn query_distinct_wire_apis(&self) -> Result<Vec<String>> {
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

    pub(crate) async fn query_distinct_models(&self) -> Result<Vec<String>> {
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

    pub(crate) async fn query_distinct_agent_kinds(
        &self,
        start_us: i64,
        end_us: i64,
    ) -> Result<Vec<String>> {
        let conn = self.read_pool.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(start_us);
            let end_ts = us_to_timestamp(end_us);
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT agent_kind FROM agent_turns \
                     WHERE start_time >= ? AND start_time < ? \
                     ORDER BY agent_kind",
                )
                .map_err(|e| {
                    AppError::Storage(format!("failed to prepare distinct_agent_kinds query: {e}"))
                })?;
            let mut rows = stmt
                .query(duckdb::params![start_ts, end_ts])
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute distinct_agent_kinds query: {e}"))
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

    pub(crate) async fn query_distinct_server_ips(&self) -> Result<Vec<String>> {
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
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use std::net::IpAddr;
    use ts_llm::model::{ApiType, LlmCall};
    use ts_llm::wire_apis as wa;
    use ts_metrics::model::LlmMetric;
    use ts_storage::StorageBackend;
    use ts_turn::{AgentTurn, TurnStatus};

    fn in_memory() -> DuckDbBackend {
        DuckDbBackend::open(":memory:").unwrap()
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
            ttft_sum: 6300.0,
            ttft_count: 42,
            ttft_p50: Some(120.0),
            ttft_p95: Some(350.0),
            ttft_p99: Some(500.0),
            e2e_sum: 50_400.0,
            e2e_count: 42,
            e2e_p50: Some(1000.0),
            e2e_p95: Some(2500.0),
            e2e_p99: Some(4000.0),
            tpot_sum: 666.0,
            tpot_count: 30,
            tpot_p50: Some(23.8),
            tpot_p95: Some(12.5),
            tpot_p99: Some(8.3),
        }
    }

    #[tokio::test]
    async fn test_query_distinct_wire_apis() {
        let backend = in_memory();
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
        let backend = in_memory();
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
        let backend = in_memory();
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

    fn agent_turn(turn_id: &str, agent_kind: &str, start_us: i64) -> AgentTurn {
        AgentTurn {
            source_id: String::new(),
            turn_id: turn_id.into(),
            session_id: "s".into(),
            wire_api: wa::OPENAI_CHAT.to_string(),
            agent_kind: agent_kind.into(),
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            start_time_us: start_us,
            end_time_us: start_us + 1_000_000,
            duration_ms: 1_000,
            call_count: 1,
            models_used: vec!["m".into()],
            subagents_used: vec![],
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_cost_usd: None,
            status: TurnStatus::Complete,
            final_finish_reason: None,
            user_input_preview: None,
            user_call_id: None,
            final_answer_preview: None,
            final_call_id: None,
            call_ids: vec!["c".into()],
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn test_query_distinct_agent_kinds_filters_by_time_and_dedupes() {
        let backend = in_memory();
        backend.init().await.unwrap();

        backend
            .write_turns(vec![
                agent_turn("t1", "claude-cli", 1_700_000_000_000_000),
                agent_turn("t2", "openclaw", 1_700_000_010_000_000),
                agent_turn("t3", "claude-cli", 1_700_000_020_000_000), // duplicate kind
                agent_turn("t4", "codex-cli", 1_600_000_000_000_000), // outside range
            ])
            .await
            .unwrap();

        let kinds = backend
            .query_distinct_agent_kinds(1_700_000_000_000_000, 1_700_000_100_000_000)
            .await
            .unwrap();
        assert_eq!(kinds, vec!["claude-cli", "openclaw"]);

        // Empty when range excludes all rows.
        let kinds_empty = backend
            .query_distinct_agent_kinds(2_000_000_000_000_000, 2_100_000_000_000_000)
            .await
            .unwrap();
        assert!(kinds_empty.is_empty());
    }
}
