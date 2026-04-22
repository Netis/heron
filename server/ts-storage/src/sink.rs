//! Storage sink stage: three tasks (one per entity type) each consume the
//! upstream pipeline channel directly, batch via `WriteBuffer`, and call the
//! backend. Returns a JoinHandle so main.rs can await the final drain.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use ts_llm::model::LlmCall;
use ts_metrics::model::LlmMetric;
use ts_protocol::HttpExchange;
use ts_turn::AgentTurn;

use ts_common::internal_metrics::{Metric, MetricsWorker};

use crate::backend::StorageBackend;
use crate::buffer::{BufferMetrics, WriteBuffer};

#[derive(Debug, Clone)]
pub struct StorageSinkConfig {
    pub batch_size: usize,
    pub flush_interval_ms: u64,
}

impl Default for StorageSinkConfig {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            flush_interval_ms: 1000,
        }
    }
}

/// Spawn the storage sink. Returns a JoinHandle that completes once every
/// input channel is closed and every batched record is flushed.
pub fn spawn_storage_sink_stage(
    config: StorageSinkConfig,
    calls_rx: mpsc::Receiver<Arc<LlmCall>>,
    turns_rx: mpsc::Receiver<AgentTurn>,
    metrics_rx: mpsc::Receiver<LlmMetric>,
    http_exchanges_rx: mpsc::Receiver<HttpExchange>,
    backend: Arc<dyn StorageBackend>,
    metrics: MetricsWorker,
) -> JoinHandle<()> {
    let flush_interval = Duration::from_millis(config.flush_interval_ms);

    let buf_metrics = BufferMetrics {
        buffered: metrics.counter(Metric::StorageRecordsBuffered).clone(),
        flushed: metrics.counter(Metric::StorageRecordsFlushed).clone(),
        errors: metrics.counter(Metric::StorageFlushErrors).clone(),
    };

    // calls_rx carries Arc<LlmCall> so turn aggregation can share the data.
    // At sink time we unwrap to owned LlmCall — cheap when we hold the last
    // Arc, a deep clone otherwise. The choice of where to do that is here:
    // before batching (per-item, on the hot path) or inside the flush
    // closure (per-batch, off the hot path). We do it per-item here so the
    // WriteBuffer can treat all entities uniformly as owned values.
    let (owned_tx, owned_rx) = mpsc::channel::<LlmCall>(calls_rx.max_capacity());
    let calls_unwrap = {
        let mut rx = calls_rx;
        tokio::spawn(async move {
            let reason = 'main: loop {
                let arc = match rx.recv().await {
                    Some(a) => a,
                    None => break 'main "upstream_eof",
                };
                if owned_tx.send(Arc::unwrap_or_clone(arc)).await.is_err() {
                    break 'main "downstream_closed";
                }
            };
            match reason {
                "upstream_eof" => {
                    tracing::debug!("storage calls_unwrap stopping: upstream EOF");
                }
                r => {
                    tracing::warn!(
                        reason = r,
                        "storage calls_unwrap stopping: downstream closed"
                    );
                }
            }
        })
    };

    let calls_storage = backend.clone();
    let calls_buffer = WriteBuffer::new(
        "calls",
        owned_rx,
        config.batch_size,
        flush_interval,
        Some(buf_metrics.clone()),
    );
    let calls_task = tokio::spawn(async move {
        calls_buffer
            .run(move |batch| {
                let b = calls_storage.clone();
                async move { b.write_calls(batch).await }
            })
            .await;
    });

    let turns_storage = backend.clone();
    let turns_buffer = WriteBuffer::new(
        "turns",
        turns_rx,
        config.batch_size,
        flush_interval,
        Some(buf_metrics.clone()),
    );
    let turns_task = tokio::spawn(async move {
        turns_buffer
            .run(move |batch| {
                let b = turns_storage.clone();
                async move { b.write_turns(batch).await }
            })
            .await;
    });

    let metrics_storage = backend.clone();
    let metrics_buffer = WriteBuffer::new(
        "metrics",
        metrics_rx,
        config.batch_size,
        flush_interval,
        Some(buf_metrics.clone()),
    );
    let metrics_task = tokio::spawn(async move {
        metrics_buffer
            .run(move |batch| {
                let b = metrics_storage.clone();
                async move { b.write_metrics(batch).await }
            })
            .await;
    });

    let exch_storage = backend.clone();
    let exch_buffer = WriteBuffer::new(
        "http_exchanges",
        http_exchanges_rx,
        config.batch_size,
        flush_interval,
        Some(buf_metrics),
    );
    let exch_task = tokio::spawn(async move {
        exch_buffer
            .run(move |batch| {
                let b = exch_storage.clone();
                async move { b.write_exchanges(batch).await }
            })
            .await;
    });

    tokio::spawn(async move {
        // Propagate inner-task panics by unwrapping join errors — otherwise
        // the outer task would exit cleanly and hide the failure from
        // supervise().
        let (ru, rc, rt, rm, re) = tokio::join!(
            calls_unwrap,
            calls_task,
            turns_task,
            metrics_task,
            exch_task
        );
        ru.expect("storage_sink: calls unwrap task panicked");
        rc.expect("storage_sink: calls writer panicked");
        rt.expect("storage_sink: turns writer panicked");
        rm.expect("storage_sink: metrics writer panicked");
        re.expect("storage_sink: exchanges writer panicked");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{
        CallDetail, CallsPage, CallsQuery, HttpExchangeDetail, HttpExchangesPage,
        HttpExchangesQuery, MetricsModelRow, MetricsModelsQuery, MetricsSummaryQuery,
        MetricsSummaryRow, MetricsTimeseriesQuery, MetricsTimeseriesRow, TurnCallItem, TurnDetail,
        TurnsPage, TurnsQuery,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use ts_common::error::Result;
    use ts_common::internal_metrics::MetricsSystem;

    struct CountingBackend {
        calls: Arc<AtomicUsize>,
        turns: Arc<AtomicUsize>,
        metrics: Arc<AtomicUsize>,
        exchanges: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageBackend for CountingBackend {
        async fn init(&self) -> Result<()> {
            Ok(())
        }
        async fn write_calls(&self, batch: Vec<LlmCall>) -> Result<()> {
            self.calls.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
        async fn write_turns(&self, batch: Vec<AgentTurn>) -> Result<()> {
            self.turns.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
        async fn write_metrics(&self, batch: Vec<LlmMetric>) -> Result<()> {
            self.metrics.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
        async fn write_exchanges(&self, batch: Vec<HttpExchange>) -> Result<()> {
            self.exchanges.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
        async fn query_http_exchange_by_id(&self, _id: &str) -> Result<Option<HttpExchangeDetail>> {
            Ok(None)
        }
        async fn query_http_exchanges(
            &self,
            _query: &HttpExchangesQuery,
        ) -> Result<HttpExchangesPage> {
            Ok(HttpExchangesPage {
                total: 0,
                items: vec![],
            })
        }
        async fn query_metrics_timeseries(
            &self,
            _query: &MetricsTimeseriesQuery,
        ) -> Result<Vec<MetricsTimeseriesRow>> {
            Ok(vec![])
        }
        async fn query_metrics_summary(
            &self,
            _query: &MetricsSummaryQuery,
        ) -> Result<MetricsSummaryRow> {
            Ok(MetricsSummaryRow {
                request_count: 0,
                error_count: 0,
                error_4xx_count: 0,
                error_429_count: 0,
                error_5xx_count: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                ttfb_avg: None,
                e2e_avg: None,
                tpot_avg: None,
            })
        }
        async fn query_metrics_models(
            &self,
            _query: &MetricsModelsQuery,
        ) -> Result<Vec<MetricsModelRow>> {
            Ok(vec![])
        }
        async fn query_calls(&self, _query: &CallsQuery) -> Result<CallsPage> {
            Ok(CallsPage {
                total: 0,
                items: vec![],
            })
        }
        async fn query_call_by_id(&self, _id: &str) -> Result<Option<CallDetail>> {
            Ok(None)
        }
        async fn query_turns(&self, _query: &TurnsQuery) -> Result<TurnsPage> {
            Ok(TurnsPage {
                total: 0,
                items: vec![],
            })
        }
        async fn query_turn_by_id(&self, _turn_id: &str) -> Result<Option<TurnDetail>> {
            Ok(None)
        }
        async fn query_turn_calls(&self, _turn_id: &str) -> Result<Vec<TurnCallItem>> {
            Ok(vec![])
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
        async fn apply_retention(
            &self,
            _policy: crate::retention::RetentionPolicy,
        ) -> Result<crate::retention::RetentionReport> {
            Ok(crate::retention::RetentionReport::default())
        }
    }

    #[tokio::test]
    async fn sink_drains_all_channels_and_flushes() {
        let counts = CountingBackend {
            calls: Arc::new(AtomicUsize::new(0)),
            turns: Arc::new(AtomicUsize::new(0)),
            metrics: Arc::new(AtomicUsize::new(0)),
            exchanges: Arc::new(AtomicUsize::new(0)),
        };
        let (calls_count, turns_count, metrics_count, exchanges_count) = (
            counts.calls.clone(),
            counts.turns.clone(),
            counts.metrics.clone(),
            counts.exchanges.clone(),
        );
        let backend: Arc<dyn StorageBackend> = Arc::new(counts);

        let (calls_tx, calls_rx) = mpsc::channel::<Arc<LlmCall>>(16);
        let (turns_tx, turns_rx) = mpsc::channel::<AgentTurn>(16);
        let (metrics_tx, metrics_rx) = mpsc::channel::<LlmMetric>(16);
        let (exch_tx, exch_rx) = mpsc::channel::<HttpExchange>(16);

        let cfg = StorageSinkConfig {
            batch_size: 2,
            flush_interval_ms: 50,
        };
        let mut metrics_sys = MetricsSystem::new();
        let storage_metrics = metrics_sys.register_worker(
            "storage_sink",
            &[
                Metric::StorageRecordsBuffered,
                Metric::StorageRecordsFlushed,
                Metric::StorageFlushErrors,
            ],
        );
        let _svc = metrics_sys.start();
        let handle = spawn_storage_sink_stage(
            cfg,
            calls_rx,
            turns_rx,
            metrics_rx,
            exch_rx,
            backend,
            storage_metrics,
        );

        for i in 0..3 {
            calls_tx.send(Arc::new(dummy_call(i))).await.unwrap();
            turns_tx.send(dummy_turn(i)).await.unwrap();
            metrics_tx.send(dummy_metric(i)).await.unwrap();
            exch_tx.send(dummy_exchange(i)).await.unwrap();
        }
        drop(calls_tx);
        drop(turns_tx);
        drop(metrics_tx);
        drop(exch_tx);

        handle.await.unwrap();
        assert_eq!(calls_count.load(Ordering::SeqCst), 3);
        assert_eq!(turns_count.load(Ordering::SeqCst), 3);
        assert_eq!(metrics_count.load(Ordering::SeqCst), 3);
        assert_eq!(exchanges_count.load(Ordering::SeqCst), 3);
    }

    fn dummy_exchange(i: usize) -> HttpExchange {
        use bytes::Bytes;
        use std::net::IpAddr;
        use std::sync::Arc;
        use ts_protocol::model::{HttpRequestData, HttpResponseData};
        use ts_protocol::net::FlowKey;
        let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let server_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let request = Arc::new(HttpRequestData {
            flow_key: FlowKey::new(String::new(), client_ip, 1000, server_ip, 8080),
            client_addr: (client_ip, 1000),
            server_addr: (server_ip, 8080),
            method: "GET".into(),
            uri: "/health".into(),
            version: 1,
            headers: vec![],
            body: Bytes::new(),
            timestamp_us: 0,
        });
        let response = Arc::new(HttpResponseData {
            flow_key: request.flow_key.clone(),
            client_addr: request.client_addr,
            server_addr: request.server_addr,
            status: 200,
            version: 1,
            headers: vec![],
            body: Bytes::from_static(b"ok"),
            first_byte_timestamp_us: 100,
            complete_timestamp_us: 200,
        });
        HttpExchange {
            id: format!("x-{i}"),
            request,
            response,
        }
    }

    fn dummy_call(i: usize) -> LlmCall {
        use std::net::IpAddr;
        use ts_llm::model::ApiType;
        use ts_llm::wire_apis as wa;
        LlmCall {
            stream_id: String::new(),
            id: format!("c-{i}"),
            wire_api: wa::OPENAI_CHAT,
            model: "m".into(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/".into(),
            is_stream: false,
            request_body: None,
            status_code: None,
            finish_reason: None,
            response_body: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttfb_ms: None,
            e2e_latency_ms: None,
            client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            client_port: 0,
            server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
            server_port: 0,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        }
    }

    fn dummy_turn(i: usize) -> AgentTurn {
        AgentTurn {
            stream_id: String::new(),
            turn_id: format!("t-{i}"),
            session_id: "s".into(),
            tenant_id: None,
            wire_api: ts_llm::wire_apis::OPENAI_CHAT.into(),
            agent_kind: "x".into(),
            start_time_us: 0,
            end_time_us: 0,
            duration_ms: 0,
            call_count: 1,
            models_used: vec![],
            subagents_used: vec![],
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_cost_usd: None,
            status: ts_turn::TurnStatus::Complete,
            final_finish_reason: None,
            user_input_preview: None,
            user_call_id: None,
            final_answer_preview: None,
            final_call_id: None,
            call_ids: vec![format!("c-{i}")],
            metadata: serde_json::json!({}),
        }
    }

    fn dummy_metric(i: usize) -> LlmMetric {
        LlmMetric {
            timestamp_us: i as i64,
            stream_id: String::new(),
            granularity: "10s",
            wire_api: ts_llm::wire_apis::OPENAI_CHAT.into(),
            model: "m".into(),
            server_ip: "*".into(),
            request_count: 1,
            stream_count: 0,
            non_stream_count: 1,
            concurrency_sum: 0,
            concurrency_sample_count: 0,
            concurrency_max: 0,
            total_input_tokens: 0,
            input_token_count: 0,
            total_output_tokens: 0,
            output_token_count: 0,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            error_count: 0,
            error_4xx_count: 0,
            error_429_count: 0,
            error_5xx_count: 0,
            finish_complete_count: 0,
            finish_length_count: 0,
            finish_tool_use_count: 0,
            finish_error_count: 0,
            finish_cancelled_count: 0,
            ttfb_sum: 0.0,
            ttfb_count: 0,
            ttfb_p50: None,
            ttfb_p95: None,
            ttfb_p99: None,
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
        }
    }
}
