use std::collections::HashMap;

use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_llm::model::{LlmCall, LlmCallStart, LlmEvent};

use crate::bucket::WindowBucket;
use crate::model::LlmMetric;

struct GranularityConfig {
    label: &'static str,
    window_secs: i64,
}

const GRANULARITIES: &[GranularityConfig] = &[
    GranularityConfig {
        label: "10s",
        window_secs: 10,
    },
    GranularityConfig {
        label: "1m",
        window_secs: 60,
    },
    GranularityConfig {
        label: "5m",
        window_secs: 300,
    },
    GranularityConfig {
        label: "1h",
        window_secs: 3600,
    },
];

/// Composite key for a metrics bucket: (granularity, window_start, dimension).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BucketKey {
    stream_id: String,
    granularity_idx: usize,
    window_start_us: i64,
    provider: String,
    model: String,
    server_ip: String,
}

/// Dimension key for concurrency tracking (no window or granularity).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DimensionKey {
    stream_id: String,
    provider: String,
    model: String,
    server_ip: String,
}

/// Aggregates LlmEvents into time-windowed LlmMetric records.
///
/// Uses event timestamps (not wall clock) for window boundaries,
/// so pcap replay produces correct results.
///
/// Supports 4 granularities (10s, 1m, 5m, 1h) and 4 dimension roll-ups
/// per event, producing up to 16 bucket entries per event.
///
/// One aggregator owns one **stream** — an independent event-time watermark.
/// The composition root instantiates one per capture source so that
/// inter-source clock skew cannot re-open already-flushed windows. Every
/// flushed `LlmMetric` carries this stream_id so the storage layer can
/// distinguish per-stream rows that share the same (ts, dim).
pub struct MetricsAggregator {
    buckets: HashMap<BucketKey, WindowBucket>,
    concurrency: HashMap<DimensionKey, i64>,
    latest_ts: HashMap<String, i64>,
    metrics: MetricsWorker,
}

impl MetricsAggregator {
    pub fn new(metrics: MetricsWorker) -> Self {
        Self {
            buckets: HashMap::new(),
            concurrency: HashMap::new(),
            latest_ts: HashMap::new(),
            metrics,
        }
    }

    /// Process an LlmEvent. Returns any metrics flushed due to window expiration.
    pub fn process(&mut self, event: &LlmEvent) -> Vec<LlmMetric> {
        self.metrics.counter(Metric::MetricsEventsReceived).inc();
        match event {
            LlmEvent::Start(start) => {
                self.on_call_start(start);
                Vec::new()
            }
            LlmEvent::Complete { call, .. } => {
                self.on_call_complete(call);
                let entry = self.latest_ts.entry(call.stream_id.clone()).or_insert(0);
                *entry = (*entry).max(call.request_time);
                self.check_windows(&call.stream_id)
            }
            LlmEvent::Heartbeat { ts, ref stream_id } => self.advance_time(*ts, stream_id),
        }
    }

    pub fn advance_time(&mut self, ts: i64, stream_id: &str) -> Vec<LlmMetric> {
        let entry = self.latest_ts.entry(stream_id.to_string()).or_insert(0);
        *entry = (*entry).max(ts);
        self.check_windows(stream_id)
    }

    /// Flush all remaining buckets. Call at end of capture.
    pub fn flush_all(&mut self) -> Vec<LlmMetric> {
        let keys: Vec<_> = self.buckets.keys().cloned().collect();
        let mut metrics = Vec::new();
        for key in keys {
            if let Some(mut bucket) = self.buckets.remove(&key) {
                if bucket.request_count > 0 {
                    metrics.push(bucket.flush(
                        key.window_start_us,
                        &key.stream_id,
                        GRANULARITIES[key.granularity_idx].label,
                        key.provider,
                        key.model,
                        key.server_ip,
                    ));
                    self.metrics.counter(Metric::MetricsWindowsFlushed).inc();
                }
            }
        }
        // Sort by granularity then timestamp then dimensions for consistent output.
        metrics.sort_by(|a, b| {
            a.granularity
                .cmp(b.granularity)
                .then(a.timestamp_us.cmp(&b.timestamp_us))
                .then(a.provider.cmp(&b.provider))
                .then(a.model.cmp(&b.model))
                .then(a.server_ip.cmp(&b.server_ip))
        });
        metrics
    }

    fn on_call_start(&mut self, start: &LlmCallStart) {
        let dim_keys = dimension_keys(
            &start.stream_id,
            &start.provider.to_string(),
            &start.model,
            &start.server_ip.to_string(),
        );

        let mut concurrency_values = [0u32; 4];
        for (i, dk) in dim_keys.iter().enumerate() {
            let entry = self.concurrency.entry(dk.clone()).or_insert(0);
            *entry += 1;
            concurrency_values[i] = (*entry).max(0) as u32;
        }

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let ws = window_start(start.timestamp_us, gran.window_secs);
            for (di, dk) in dim_keys.iter().enumerate() {
                let bk = BucketKey {
                    stream_id: start.stream_id.clone(),
                    granularity_idx: gi,
                    window_start_us: ws,
                    provider: dk.provider.clone(),
                    model: dk.model.clone(),
                    server_ip: dk.server_ip.clone(),
                };
                let bucket = self.buckets.entry(bk).or_insert_with(WindowBucket::new);
                bucket.on_call_start(start.is_stream);
                bucket.sample_concurrency(concurrency_values[di]);
            }
        }
    }

    fn on_call_complete(&mut self, call: &LlmCall) {
        let dim_keys = dimension_keys(
            &call.stream_id,
            &call.provider.to_string(),
            &call.model,
            &call.server_ip.to_string(),
        );

        for dk in &dim_keys {
            let entry = self.concurrency.entry(dk.clone()).or_insert(0);
            *entry = (*entry - 1).max(0);
        }

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let ws = window_start(call.request_time, gran.window_secs);
            for dk in &dim_keys {
                let bk = BucketKey {
                    stream_id: call.stream_id.clone(),
                    granularity_idx: gi,
                    window_start_us: ws,
                    provider: dk.provider.clone(),
                    model: dk.model.clone(),
                    server_ip: dk.server_ip.clone(),
                };
                let bucket = self.buckets.entry(bk).or_insert_with(WindowBucket::new);
                bucket.on_call_complete(call);
            }
        }
    }

    /// Flush windows that are strictly older than the current window
    /// for each granularity.
    fn check_windows(&mut self, stream_id: &str) -> Vec<LlmMetric> {
        let watermark = match self.latest_ts.get(stream_id) {
            Some(&ts) => ts,
            None => return Vec::new(),
        };
        let mut metrics = Vec::new();

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let current_window = window_start(watermark, gran.window_secs);
            let expired_keys: Vec<_> = self
                .buckets
                .keys()
                .filter(|k| {
                    k.stream_id == stream_id
                        && k.granularity_idx == gi
                        && k.window_start_us < current_window
                })
                .cloned()
                .collect();

            for key in expired_keys {
                if let Some(mut bucket) = self.buckets.remove(&key) {
                    if bucket.request_count > 0 {
                        metrics.push(bucket.flush(
                            key.window_start_us,
                            &key.stream_id,
                            gran.label,
                            key.provider,
                            key.model,
                            key.server_ip,
                        ));
                        self.metrics.counter(Metric::MetricsWindowsFlushed).inc();
                    }
                }
            }
        }

        metrics
    }
}

/// Compute the window start timestamp for a given event time and window size.
fn window_start(timestamp_us: i64, window_secs: i64) -> i64 {
    let window_us = window_secs * 1_000_000;
    (timestamp_us / window_us) * window_us
}

/// Generate the 4 dimension keys for a single event.
fn dimension_keys(
    stream_id: &str,
    provider: &str,
    model: &str,
    server_ip: &str,
) -> [DimensionKey; 4] {
    [
        DimensionKey {
            stream_id: stream_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            server_ip: server_ip.to_string(),
        },
        DimensionKey {
            stream_id: stream_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            server_ip: "*".to_string(),
        },
        DimensionKey {
            stream_id: stream_id.to_string(),
            provider: "*".to_string(),
            model: "*".to_string(),
            server_ip: server_ip.to_string(),
        },
        DimensionKey {
            stream_id: stream_id.to_string(),
            provider: "*".to_string(),
            model: "*".to_string(),
            server_ip: "*".to_string(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_llm::model::{ApiType, FinishReason, LlmCallStart, ProviderFormat};

    fn test_metrics() -> MetricsWorker {
        use ts_common::internal_metrics::MetricsSystem;
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[Metric::MetricsEventsReceived, Metric::MetricsWindowsFlushed],
        );
        let _svc = sys.start();
        w
    }

    fn make_start(ts_us: i64, model: &str, is_stream: bool) -> LlmEvent {
        LlmEvent::Start(LlmCallStart {
            stream_id: String::new(),
            provider: ProviderFormat::OpenAI,
            model: model.to_string(),
            is_stream,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            timestamp_us: ts_us,
        })
    }

    fn make_complete(request_time: i64, complete_time: i64, model: &str) -> LlmEvent {
        LlmEvent::Complete {
            call: Arc::new(LlmCall {
                stream_id: String::new(),
                id: "test".to_string(),
                provider: ProviderFormat::OpenAI,
                model: model.to_string(),
                api_type: ApiType::Chat,
                tenant_id: None,
                request_time,
                response_time: Some(request_time + 100_000),
                complete_time: Some(complete_time),
                request_path: "/v1/chat/completions".to_string(),
                is_stream: true,
                request_body: None,
                status_code: Some(200),
                finish_reason: Some(FinishReason::Complete),
                response_body: None,
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                ttfb_ms: Some(100.0),
                e2e_latency_ms: Some(500.0),
                client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
                client_port: 12345,
                server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
                server_port: 443,
                response_id: None,
                request_headers: vec![],
                response_headers: vec![],
            }),
            identity: None,
        }
    }

    #[test]
    fn test_window_start() {
        // 10s granularity
        let ts = 1_700_000_005_000_000i64; // 5s into a 10s window
        let ws = window_start(ts, 10);
        assert_eq!(ws, 1_700_000_000_000_000);
        assert_eq!(ws % 10_000_000, 0);
        assert!(ws <= ts);
        assert!(ws + 10_000_000 > ts);

        // 1m granularity
        let ts = 1_700_000_030_000_000i64; // 30s into a minute
        let ws = window_start(ts, 60);
        let expected = (ts / 60_000_000) * 60_000_000;
        assert_eq!(ws, expected);
        assert_eq!(ws % 60_000_000, 0);

        // 5m granularity
        let ts = 1_700_000_100_000_000i64; // 100s into a 300s window
        let ws = window_start(ts, 300);
        assert_eq!(ws % 300_000_000, 0);
        assert!(ws <= ts);
        assert!(ws + 300_000_000 > ts);

        // 1h granularity
        let ts = 1_700_001_800_000_000i64; // 1800s into a 3600s window
        let ws = window_start(ts, 3600);
        assert_eq!(ws % 3_600_000_000, 0);
        assert!(ws <= ts);
        assert!(ws + 3_600_000_000 > ts);
    }

    #[test]
    fn test_multi_granularity_10s_flushes_before_1m() {
        let mut agg = MetricsAggregator::new(test_metrics());
        // t0: aligned to all granularities
        let t0 = 1_700_000_000_000_000i64;

        // Event at t0
        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));

        // Event at t0 + 15s (same 1m window, different 10s window)
        let t1 = t0 + 15_000_000;
        agg.process(&make_start(t1, "gpt-4", true));
        let flushed = agg.process(&make_complete(t1, t1 + 500_000, "gpt-4"));

        // 10s windows for t0 should have expired (4 dimensions).
        let flushed_10s: Vec<_> = flushed.iter().filter(|m| m.granularity == "10s").collect();
        assert_eq!(
            flushed_10s.len(),
            4,
            "should flush 4 dimension combos for 10s granularity"
        );

        // 1m should NOT have flushed (both events in same 1m window).
        let flushed_1m: Vec<_> = flushed.iter().filter(|m| m.granularity == "1m").collect();
        assert_eq!(flushed_1m.len(), 0, "1m window should not flush yet");
    }

    #[test]
    fn test_concurrency_cross_window() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        // 3 Starts at t0
        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_start(t0 + 1_000, "gpt-4", true));
        agg.process(&make_start(t0 + 2_000, "gpt-4", true));

        // All complete at t0 + 15s (same 10s window? no, 15s > 10s)
        let t1 = t0 + 15_000_000;
        agg.process(&make_complete(t0, t1, "gpt-4"));
        agg.process(&make_complete(t0 + 1_000, t1, "gpt-4"));
        agg.process(&make_complete(t0 + 2_000, t1, "gpt-4"));

        // Trigger flush at t0 + 25s
        let t2 = t0 + 25_000_000;
        agg.process(&make_start(t2, "gpt-4", true));
        let flushed = agg.process(&make_complete(t2, t2 + 500_000, "gpt-4"));

        // Find the global (*,*,*) 10s metric for the first window
        let global_10s: Vec<_> = flushed
            .iter()
            .filter(|m| {
                m.granularity == "10s" && m.provider == "*" && m.model == "*" && m.server_ip == "*"
            })
            .collect();

        assert!(
            !global_10s.is_empty(),
            "should have flushed global 10s metric"
        );
        let first_window_metric = global_10s
            .iter()
            .find(|m| m.timestamp_us == t0)
            .expect("should have metric for t0 window");
        assert!(
            first_window_metric.concurrency_max >= 3,
            "concurrency_max should be >= 3, got {}",
            first_window_metric.concurrency_max
        );
    }

    #[test]
    fn test_flush_all_emits_all_granularities() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));

        let metrics = agg.flush_all();
        // 4 granularities x 4 dimensions = 16
        assert_eq!(
            metrics.len(),
            16,
            "flush_all should return 16 metrics, got {}",
            metrics.len()
        );

        // Verify each granularity has 4 metrics.
        for label in ["10s", "1m", "5m", "1h"] {
            let count = metrics.iter().filter(|m| m.granularity == label).count();
            assert_eq!(
                count, 4,
                "granularity {} should have 4 metrics, got {}",
                label, count
            );
        }
    }

    fn make_start_with_stream(ts_us: i64, model: &str, is_stream: bool, sid: &str) -> LlmEvent {
        LlmEvent::Start(LlmCallStart {
            stream_id: sid.to_string(),
            provider: ProviderFormat::OpenAI,
            model: model.to_string(),
            is_stream,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            timestamp_us: ts_us,
        })
    }

    fn make_complete_with_stream(
        request_time: i64,
        complete_time: i64,
        model: &str,
        sid: &str,
    ) -> LlmEvent {
        LlmEvent::Complete {
            call: Arc::new(LlmCall {
                stream_id: sid.to_string(),
                id: "test".to_string(),
                provider: ProviderFormat::OpenAI,
                model: model.to_string(),
                api_type: ApiType::Chat,
                tenant_id: None,
                request_time,
                response_time: Some(request_time + 100_000),
                complete_time: Some(complete_time),
                request_path: "/v1/chat/completions".to_string(),
                is_stream: true,
                request_body: None,
                status_code: Some(200),
                finish_reason: Some(FinishReason::Complete),
                response_body: None,
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                ttfb_ms: Some(100.0),
                e2e_latency_ms: Some(500.0),
                client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
                client_port: 12345,
                server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
                server_port: 443,
                response_id: None,
                request_headers: vec![],
                response_headers: vec![],
            }),
            identity: None,
        }
    }

    #[test]
    fn test_multi_stream_independent_watermarks() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;
        let t1 = t0 + 15_000_000;

        // Stream "s0" advances past 10s window.
        agg.process(&make_start_with_stream(t0, "gpt-4", true, "s0"));
        agg.process(&make_complete_with_stream(t0, t0 + 500_000, "gpt-4", "s0"));
        agg.process(&make_start_with_stream(t1, "gpt-4", true, "s0"));
        let s0_flushed = agg.process(&make_complete_with_stream(t1, t1 + 500_000, "gpt-4", "s0"));

        let s0_10s: Vec<_> = s0_flushed
            .iter()
            .filter(|m| m.granularity == "10s" && m.stream_id == "s0")
            .collect();
        assert_eq!(s0_10s.len(), 4, "s0 should flush 4 dims for t0");

        // Stream "s1" still at t0 — its window must NOT be flushed by s0's watermark.
        agg.process(&make_start_with_stream(t0, "gpt-4", true, "s1"));
        let s1_flushed = agg.process(&make_complete_with_stream(t0, t0 + 500_000, "gpt-4", "s1"));
        assert_eq!(
            s1_flushed
                .iter()
                .filter(|m| m.stream_id == "s1" && m.granularity == "10s")
                .count(),
            0,
            "s1 window should not flush yet"
        );
    }

    #[test]
    fn test_dimension_expansion() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));

        let metrics = agg.flush_all();

        // Filter to 10s granularity only.
        let metrics_10s: Vec<_> = metrics.iter().filter(|m| m.granularity == "10s").collect();
        assert_eq!(metrics_10s.len(), 4);

        // Check all 4 dimension combos exist.
        assert!(
            metrics_10s
                .iter()
                .any(|m| m.provider == "openai" && m.model == "gpt-4" && m.server_ip == "10.0.0.1"),
            "should have finest dimension (openai, gpt-4, 10.0.0.1)"
        );
        assert!(
            metrics_10s
                .iter()
                .any(|m| m.provider == "openai" && m.model == "gpt-4" && m.server_ip == "*"),
            "should have per-model dimension (openai, gpt-4, *)"
        );
        assert!(
            metrics_10s
                .iter()
                .any(|m| m.provider == "*" && m.model == "*" && m.server_ip == "10.0.0.1"),
            "should have per-server dimension (*, *, 10.0.0.1)"
        );
        assert!(
            metrics_10s
                .iter()
                .any(|m| m.provider == "*" && m.model == "*" && m.server_ip == "*"),
            "should have global dimension (*, *, *)"
        );
    }
}
