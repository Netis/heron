//! Distinct-value queries used to populate filter dropdowns.

use h_common::error::{AppError, Result};
use h_storage::query::DistinctAgentKindsQuery;

use crate::util::us_to_timestamp;
use crate::DuckDbBackend;

fn sql_string_list(values: &[String]) -> String {
    values
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

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

    pub(crate) async fn query_distinct_agent_kinds(
        &self,
        query: &DistinctAgentKindsQuery,
    ) -> Result<Vec<String>> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();
        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);

            let mut where_parts = vec!["start_time >= ?".to_string(), "start_time < ?".to_string()];

            if !query.filter.wire_apis.is_empty() {
                where_parts.push(format!(
                    "wire_api IN ({})",
                    sql_string_list(&query.filter.wire_apis)
                ));
            }
            if !query.filter.models.is_empty() {
                where_parts.push(format!(
                    "list_has_any(CAST(CAST(models_used AS JSON) AS VARCHAR[]), [{}])",
                    sql_string_list(&query.filter.models)
                ));
            }
            if !query.filter.server_ips.is_empty() {
                where_parts.push(format!(
                    "server_ip IN ({})",
                    sql_string_list(&query.filter.server_ips)
                ));
            }
            if !query.include_proxy_hops {
                where_parts.push(
                    "(json_extract_string(metadata, '$.proxy.role') IS NULL \
                       OR json_extract_string(metadata, '$.proxy.role') \
                          NOT IN ('proxy_out', 'mirror_secondary'))"
                        .to_string(),
                );
            }

            let sql = format!(
                "SELECT DISTINCT agent_kind FROM traces WHERE {} ORDER BY agent_kind",
                where_parts.join(" AND ")
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare distinct_agent_kinds query: {e}"))
            })?;
            let mut rows = stmt.query(duckdb::params![start_ts, end_ts]).map_err(|e| {
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
}

#[cfg(test)]
mod tests {
    use crate::DuckDbBackend;
    use h_llm::wire_apis as wa;
    use h_metrics::model::LlmMetric;
    use h_storage::query::{DimensionFilter, DistinctAgentKindsQuery, TimeRange};
    use h_storage::StorageBackend;
    use h_turn::{AgentTurn, TurnStatus};

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
            tool_surface: None,
        }
    }

    fn sample_turn(
        turn_id: &str,
        agent_kind: &str,
        wire_api: &str,
        models_used: Vec<&str>,
        server_ip: &str,
        start_us: i64,
        metadata: serde_json::Value,
    ) -> AgentTurn {
        AgentTurn {
            source_id: String::new(),
            turn_id: turn_id.into(),
            session_id: "s1".into(),
            wire_api: wire_api.into(),
            agent_kind: agent_kind.into(),
            client_ip: "127.0.0.1".parse().unwrap(),
            server_ip: server_ip.parse().unwrap(),
            start_time_us: start_us,
            end_time_us: start_us + 100_000,
            duration_ms: 100,
            call_count: 1,
            models_used: models_used.into_iter().map(String::from).collect(),
            subagents_used: vec![],
            total_input_tokens: 100,
            total_output_tokens: 50,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_cost_usd: None,
            status: TurnStatus::Complete,
            final_finish_reason: Some("complete".into()),
            user_input_preview: Some("hello".into()),
            user_call_id: None,
            final_answer_preview: Some("world".into()),
            final_call_id: None,
            call_ids: vec!["call-1".into()],
            metadata,
            tool_surfaces: vec![],
            tool_call_total: 0,
            agent_topology: None,
            suspicious_skills: vec![],
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

    #[tokio::test]
    async fn test_query_distinct_agent_kinds_filters_by_turn_window() {
        let backend = in_memory();
        backend.init().await.unwrap();

        let base = 1_700_000_000_000_000_i64;
        backend
            .write_turns(vec![
                sample_turn(
                    "t-openclaw",
                    "openclaw",
                    wa::OPENAI_CHAT,
                    vec!["gpt-4"],
                    "10.0.0.1",
                    base + 1_000_000,
                    serde_json::json!({}),
                ),
                sample_turn(
                    "t-hermes",
                    "hermes",
                    wa::ANTHROPIC,
                    vec!["claude-sonnet"],
                    "10.0.0.2",
                    base + 2_000_000,
                    serde_json::json!({}),
                ),
                sample_turn(
                    "t-old",
                    "codex-cli",
                    wa::OPENAI_CHAT,
                    vec!["gpt-4"],
                    "10.0.0.1",
                    base - 1_000_000,
                    serde_json::json!({}),
                ),
                sample_turn(
                    "t-hidden",
                    "generic",
                    wa::OPENAI_CHAT,
                    vec!["gpt-4"],
                    "10.0.0.1",
                    base + 3_000_000,
                    serde_json::json!({"proxy": {"role": "proxy_out"}}),
                ),
            ])
            .await
            .unwrap();

        let query = DistinctAgentKindsQuery {
            time_range: TimeRange {
                start_us: base,
                end_us: base + 10_000_000,
            },
            filter: DimensionFilter::default(),
            include_proxy_hops: false,
        };

        let agent_kinds = backend.query_distinct_agent_kinds(&query).await.unwrap();
        assert_eq!(agent_kinds, vec!["hermes", "openclaw"]);

        let mut openai_query = query.clone();
        openai_query.filter.wire_apis = vec![wa::OPENAI_CHAT.to_string()];
        let agent_kinds = backend
            .query_distinct_agent_kinds(&openai_query)
            .await
            .unwrap();
        assert_eq!(agent_kinds, vec!["openclaw"]);

        let mut hidden_query = query;
        hidden_query.include_proxy_hops = true;
        let agent_kinds = backend
            .query_distinct_agent_kinds(&hidden_query)
            .await
            .unwrap();
        assert_eq!(agent_kinds, vec!["generic", "hermes", "openclaw"]);
    }
}
