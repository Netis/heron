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

/// Composite key for a metrics bucket: (stream, granularity, window_start, dim).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BucketKey {
    stream_id: String,
    granularity_idx: usize,
    window_start_us: i64,
    wire_api: String,
    model: String,
    server_ip: String,
}

/// Dimension key for concurrency tracking (no window or granularity).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DimensionKey {
    stream_id: String,
    wire_api: String,
    model: String,
    server_ip: String,
}

/// Aggregates LlmEvents into time-windowed LlmMetric records.
///
/// Each `(stream, granularity, window_start(request_time), dim)` key maps
/// to one `WindowBucket` that receives writes from both `LlmEvent::Start`
/// (traffic/concurrency) and `LlmEvent::Complete` (tokens/errors/latency).
/// The bucket is drained on a per-`(stream, granularity)` cadence aligned
/// to the window boundary (first drain at `window_start + window_secs`,
/// subsequent drains every `window_secs` of event-time) and then dropped.
/// A late Complete whose bucket was already drained opens a fresh bucket for
/// the same window and produces another row at the next cadence — the
/// query layer SUMs rows with the same key to assemble the full window.
///
/// Event-time exclusive: watermark is advanced by every event (Start:
/// `timestamp_us`, Complete: `complete_time.unwrap_or(request_time)`,
/// Heartbeat: `ts`). Pcap replay therefore produces deterministic output.
pub struct MetricsAggregator {
    buckets: HashMap<BucketKey, WindowBucket>,
    concurrency: HashMap<DimensionKey, i64>,
    /// Per-stream event-time watermark, advanced by every event.
    latest_ts: HashMap<String, i64>,
    /// Per-(stream, granularity_idx) cadence anchor for bucket drain.
    /// Initialized on first write to the bucket with `window_start(ts, gran)`
    /// so the first drain fires at `window_end` instead of emitting a
    /// single-sample row the moment the first event arrives mid-window.
    last_flush_ts: HashMap<(String, usize), i64>,
    metrics: MetricsWorker,
}

impl MetricsAggregator {
    pub fn new(metrics: MetricsWorker) -> Self {
        Self {
            buckets: HashMap::new(),
            concurrency: HashMap::new(),
            latest_ts: HashMap::new(),
            last_flush_ts: HashMap::new(),
            metrics,
        }
    }

    /// Process an LlmEvent. Returns any metric rows emitted by cadence drain.
    pub fn process(&mut self, event: &LlmEvent) -> Vec<LlmMetric> {
        self.metrics.counter(Metric::MetricsEventsReceived).inc();

        let (stream_id, ts) = event_clock(event);

        match event {
            LlmEvent::Start(start) => self.on_call_start(start),
            LlmEvent::Complete { call, .. } => self.on_call_complete(call),
            LlmEvent::Heartbeat { .. } => {}
        }

        let watermark = {
            let entry = self.latest_ts.entry(stream_id.clone()).or_insert(0);
            *entry = (*entry).max(ts);
            *entry
        };

        self.maybe_drain(&stream_id, watermark)
    }

    /// Flush all remaining buckets. Call at end of capture.
    pub fn flush_all(&mut self) -> Vec<LlmMetric> {
        let keys: Vec<_> = self.buckets.keys().cloned().collect();
        let mut metrics = Vec::new();
        for key in keys {
            if let Some(mut bucket) = self.buckets.remove(&key) {
                if bucket.has_data() {
                    metrics.push(bucket.flush(
                        key.window_start_us,
                        &key.stream_id,
                        GRANULARITIES[key.granularity_idx].label,
                        key.wire_api,
                        key.model,
                        key.server_ip,
                    ));
                    self.metrics.counter(Metric::MetricsWindowsFlushed).inc();
                }
            }
        }
        metrics.sort_by(|a, b| {
            a.granularity
                .cmp(b.granularity)
                .then(a.timestamp_us.cmp(&b.timestamp_us))
                .then(a.wire_api.cmp(&b.wire_api))
                .then(a.model.cmp(&b.model))
                .then(a.server_ip.cmp(&b.server_ip))
        });
        metrics
    }

    fn on_call_start(&mut self, start: &LlmCallStart) {
        let dim_keys = dimension_keys(
            &start.stream_id,
            &start.wire_api.to_string(),
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
            self.last_flush_ts
                .entry((start.stream_id.clone(), gi))
                .or_insert(ws);
            for (di, dk) in dim_keys.iter().enumerate() {
                let bk = BucketKey {
                    stream_id: start.stream_id.clone(),
                    granularity_idx: gi,
                    window_start_us: ws,
                    wire_api: dk.wire_api.clone(),
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
            &call.wire_api.to_string(),
            &call.model,
            &call.server_ip.to_string(),
        );

        for dk in &dim_keys {
            let entry = self.concurrency.entry(dk.clone()).or_insert(0);
            *entry = (*entry - 1).max(0);
        }

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let ws = window_start(call.request_time, gran.window_secs);
            // Same cadence-anchor init as on_call_start — covers captures
            // that start mid-flow and see Complete without a prior Start.
            self.last_flush_ts
                .entry((call.stream_id.clone(), gi))
                .or_insert(ws);
            for dk in &dim_keys {
                let bk = BucketKey {
                    stream_id: call.stream_id.clone(),
                    granularity_idx: gi,
                    window_start_us: ws,
                    wire_api: dk.wire_api.clone(),
                    model: dk.model.clone(),
                    server_ip: dk.server_ip.clone(),
                };
                let bucket = self.buckets.entry(bk).or_insert_with(WindowBucket::new);
                bucket.on_call_complete(call);
            }
        }
    }

    /// Per-granularity cadence drain. For each `(stream, gran)` pair where
    /// at least `gran.window_secs` of event-time have elapsed since the
    /// anchor, emit and clear all dirty buckets for that pair.
    fn maybe_drain(&mut self, stream_id: &str, now_ts: i64) -> Vec<LlmMetric> {
        let mut metrics = Vec::new();

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let interval_us = gran.window_secs * 1_000_000;
            let last = match self.last_flush_ts.get(&(stream_id.to_string(), gi)) {
                Some(&t) => t,
                None => continue,
            };
            if now_ts - last < interval_us {
                continue;
            }
            self.last_flush_ts
                .insert((stream_id.to_string(), gi), now_ts);

            let dirty_keys: Vec<_> = self
                .buckets
                .keys()
                .filter(|k| k.stream_id == stream_id && k.granularity_idx == gi)
                .cloned()
                .collect();

            for key in dirty_keys {
                if let Some(mut bucket) = self.buckets.remove(&key) {
                    if bucket.has_data() {
                        metrics.push(bucket.flush(
                            key.window_start_us,
                            &key.stream_id,
                            gran.label,
                            key.wire_api,
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

/// Extract `(stream_id, event_time_us)` from an event. Event time:
/// * `Start` — `timestamp_us` (packet time of the request).
/// * `Complete` — `complete_time.unwrap_or(request_time)` (latest real
///   event-time we have for this call; `request_time` is only a fallback
///   and does not meaningfully advance the watermark on its own).
/// * `Heartbeat` — `ts` (synthetic packet time from capture).
fn event_clock(event: &LlmEvent) -> (String, i64) {
    match event {
        LlmEvent::Start(s) => (s.stream_id.clone(), s.timestamp_us),
        LlmEvent::Complete { call, .. } => (
            call.stream_id.clone(),
            call.complete_time.unwrap_or(call.request_time),
        ),
        LlmEvent::Heartbeat { ts, stream_id } => (stream_id.clone(), *ts),
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
    wire_api: &str,
    model: &str,
    server_ip: &str,
) -> [DimensionKey; 4] {
    [
        DimensionKey {
            stream_id: stream_id.to_string(),
            wire_api: wire_api.to_string(),
            model: model.to_string(),
            server_ip: server_ip.to_string(),
        },
        DimensionKey {
            stream_id: stream_id.to_string(),
            wire_api: wire_api.to_string(),
            model: model.to_string(),
            server_ip: "*".to_string(),
        },
        DimensionKey {
            stream_id: stream_id.to_string(),
            wire_api: "*".to_string(),
            model: "*".to_string(),
            server_ip: server_ip.to_string(),
        },
        DimensionKey {
            stream_id: stream_id.to_string(),
            wire_api: "*".to_string(),
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
    use ts_llm::model::{ApiType, FinishReason, LlmCall, LlmCallStart};
    use ts_llm::wire_apis as wa;

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
        make_start_with_stream(ts_us, model, is_stream, "")
    }

    fn make_start_with_stream(ts_us: i64, model: &str, is_stream: bool, sid: &str) -> LlmEvent {
        LlmEvent::Start(LlmCallStart {
            stream_id: sid.to_string(),
            wire_api: wa::OPENAI_CHAT,
            model: model.to_string(),
            is_stream,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            timestamp_us: ts_us,
        })
    }

    fn make_complete(request_time: i64, complete_time: i64, model: &str) -> LlmEvent {
        make_complete_with_stream(request_time, complete_time, model, "")
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
                id: format!("c-{request_time}-{complete_time}"),
                wire_api: wa::OPENAI_CHAT,
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

    fn make_heartbeat(ts_us: i64, sid: &str) -> LlmEvent {
        LlmEvent::Heartbeat {
            ts: ts_us,
            stream_id: sid.to_string(),
        }
    }

    #[test]
    fn window_start_alignment() {
        let ts = 1_700_000_005_000_000i64;
        let ws = window_start(ts, 10);
        assert_eq!(ws, 1_700_000_000_000_000);
        assert!(ws <= ts && ws + 10_000_000 > ts);
    }

    #[test]
    fn fast_call_emits_one_merged_row_per_dim() {
        // Common path: Start and Complete land inside the same cadence slice
        // and merge into a single row per dimension.
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));
        let flushed = agg.process(&make_heartbeat(t0 + 10_000_000, ""));

        let rows_10s: Vec<_> = flushed
            .iter()
            .filter(|m| m.granularity == "10s" && m.timestamp_us == t0)
            .collect();
        assert_eq!(rows_10s.len(), 4, "one merged row per dim");
        let finest = rows_10s
            .iter()
            .find(|m| m.model == "gpt-4" && m.server_ip != "*")
            .expect("finest dim present");
        assert_eq!(finest.request_count, 1);
        assert_eq!(finest.total_input_tokens, 100);
        assert_eq!(finest.ttfb_count, 1);
        assert!(finest.ttfb_sum > 0.0);
        assert_eq!(finest.finish_complete_count, 1);
    }

    #[test]
    fn slow_response_lands_in_request_time_window() {
        // Start at t0, heartbeats push the watermark past t0+10s before the
        // Complete returns. The Complete (with request_time=t0) must still
        // produce a row keyed to the t0 window.
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        let mut all = Vec::new();
        all.extend(agg.process(&make_start(t0, "gpt-4", true)));
        all.extend(agg.process(&make_heartbeat(t0 + 15_000_000, "")));
        // Start row already emitted (request_count=1, tokens=0).
        let start_row = all
            .iter()
            .find(|m| {
                m.granularity == "10s"
                    && m.timestamp_us == t0
                    && m.model == "gpt-4"
                    && m.server_ip != "*"
            })
            .expect("start row for t0");
        assert_eq!(start_row.request_count, 1);
        assert_eq!(start_row.total_input_tokens, 0);
        assert_eq!(start_row.ttfb_count, 0);
        assert_eq!(start_row.ttfb_sum, 0.0);

        // Complete returns late.
        all.extend(agg.process(&make_complete(t0, t0 + 35_000_000, "gpt-4")));
        // Trigger next cadence drain.
        all.extend(agg.process(&make_heartbeat(t0 + 45_000_000, "")));

        // Second row for the same window carries Complete payload.
        let complete_rows: Vec<_> = all
            .iter()
            .filter(|m| {
                m.granularity == "10s"
                    && m.timestamp_us == t0
                    && m.model == "gpt-4"
                    && m.server_ip != "*"
            })
            .collect();
        assert_eq!(
            complete_rows.len(),
            2,
            "two rows for same window: start + late complete"
        );
        let late = complete_rows
            .iter()
            .find(|m| m.total_input_tokens > 0)
            .expect("late complete row");
        assert_eq!(late.request_count, 0, "late complete row has zero traffic");
        assert_eq!(late.total_input_tokens, 100);
        assert_eq!(late.ttfb_count, 1);
        assert!(late.ttfb_sum > 0.0);
    }

    #[test]
    fn sum_traffic_across_rows_is_not_double_counted() {
        // Fast call + slow call in same window: we expect traffic SUM over
        // all rows for that window to equal 2 (not inflated by Complete
        // rows that carry request_count=0).
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        let mut all = Vec::new();
        // Fast call.
        all.extend(agg.process(&make_start(t0, "gpt-4", true)));
        all.extend(agg.process(&make_complete(t0, t0 + 500_000, "gpt-4")));
        // Slow call — Start arrives, Complete returns after window close.
        all.extend(agg.process(&make_start(t0 + 1_000_000, "gpt-4", true)));
        // First cadence drain.
        all.extend(agg.process(&make_heartbeat(t0 + 10_000_000, "")));
        // Slow Complete.
        all.extend(agg.process(&make_complete(t0 + 1_000_000, t0 + 30_000_000, "gpt-4")));
        // Second cadence drain.
        all.extend(agg.process(&make_heartbeat(t0 + 40_000_000, "")));

        let traffic: u64 = all
            .iter()
            .filter(|m| {
                m.granularity == "10s"
                    && m.timestamp_us == t0
                    && m.model == "gpt-4"
                    && m.server_ip != "*"
            })
            .map(|m| m.request_count)
            .sum();
        assert_eq!(
            traffic, 2,
            "two starts in [t0, t0+10s), late complete adds 0"
        );
    }

    #[test]
    fn complete_side_cadence_per_gran() {
        // 10s cadence fires; 1m cadence does not — each granularity uses
        // its own drain interval.
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));
        let flushed = agg.process(&make_heartbeat(t0 + 10_000_000, ""));

        assert!(flushed.iter().any(|m| m.granularity == "10s"));
        assert!(
            !flushed.iter().any(|m| m.granularity == "1m"),
            "1m cadence must not fire at 10s"
        );
    }

    #[test]
    fn drain_by_own_complete_event_without_heartbeat() {
        // A stream that never sees heartbeats still drains itself via its
        // own Complete events crossing the cadence boundary.
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));
        // Next Complete's complete_time pushes watermark past 10s boundary.
        let flushed = agg.process(&make_complete(t0, t0 + 15_000_000, "gpt-4"));

        let rows_10s: Vec<_> = flushed
            .iter()
            .filter(|m| m.granularity == "10s" && m.timestamp_us == t0)
            .collect();
        assert_eq!(rows_10s.len(), 4, "Complete event alone triggers drain");
    }

    #[test]
    fn first_flush_aligned_to_window_start() {
        // A single Complete at t0+5s must not immediately emit a single-point
        // row; the cadence anchor is window_start(t0) so first fire is at
        // watermark ≥ t0 + 10s.
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        let immediate = agg.process(&make_complete(t0, t0 + 5_000_000, "gpt-4"));
        assert!(
            !immediate.iter().any(|m| m.granularity == "10s"),
            "first Complete inside the window should not trigger an immediate emit"
        );
    }

    #[test]
    fn flush_all_emits_one_row_per_dim_per_gran() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));

        let metrics = agg.flush_all();
        assert_eq!(metrics.len(), 16, "4 grans × 4 dims, one merged row each");
        for gran in ["10s", "1m", "5m", "1h"] {
            assert_eq!(metrics.iter().filter(|m| m.granularity == gran).count(), 4);
        }
    }

    #[test]
    fn slow_response_emits_multiple_rows_same_window() {
        // Start at t0 then two slow Completes straddling cadence boundaries
        // produce multiple rows at the same window timestamp.
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        let mut all = Vec::new();
        all.extend(agg.process(&make_start(t0, "gpt-4", true)));
        all.extend(agg.process(&make_complete(t0, t0 + 25_000_000, "gpt-4")));
        all.extend(agg.process(&make_heartbeat(t0 + 35_000_000, "")));
        all.extend(agg.process(&make_complete(t0, t0 + 45_000_000, "gpt-4")));
        all.extend(agg.process(&make_heartbeat(t0 + 55_000_000, "")));

        let rows_t0: Vec<_> = all
            .iter()
            .filter(|m| {
                m.granularity == "10s"
                    && m.timestamp_us == t0
                    && m.model == "gpt-4"
                    && m.server_ip != "*"
            })
            .collect();
        assert!(
            rows_t0.len() >= 2,
            "expected ≥2 rows for same window, got {}",
            rows_t0.len()
        );
    }

    #[test]
    fn multi_stream_independent_watermarks() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;
        let t1 = t0 + 15_000_000;

        // Stream s0 advances past 10s boundary.
        agg.process(&make_start_with_stream(t0, "gpt-4", true, "s0"));
        agg.process(&make_complete_with_stream(t0, t0 + 500_000, "gpt-4", "s0"));
        let s0_flushed = agg.process(&make_start_with_stream(t1, "gpt-4", true, "s0"));

        let s0_rows: Vec<_> = s0_flushed
            .iter()
            .filter(|m| m.granularity == "10s" && m.stream_id == "s0" && m.timestamp_us == t0)
            .collect();
        assert_eq!(s0_rows.len(), 4, "s0 drains for t0");

        // Stream s1 still inside [t0, t0+10s) — its window must not drain.
        agg.process(&make_start_with_stream(t0, "gpt-4", true, "s1"));
        let s1_flushed = agg.process(&make_complete_with_stream(t0, t0 + 500_000, "gpt-4", "s1"));
        assert_eq!(
            s1_flushed
                .iter()
                .filter(|m| m.granularity == "10s" && m.stream_id == "s1")
                .count(),
            0
        );
    }

    #[test]
    fn concurrency_tracked_in_merged_row() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        let mut all = Vec::new();
        all.extend(agg.process(&make_start(t0, "gpt-4", true)));
        all.extend(agg.process(&make_start(t0 + 1_000, "gpt-4", true)));
        all.extend(agg.process(&make_start(t0 + 2_000, "gpt-4", true)));

        let t1 = t0 + 15_000_000;
        all.extend(agg.process(&make_complete(t0, t1, "gpt-4")));
        all.extend(agg.process(&make_complete(t0 + 1_000, t1, "gpt-4")));
        all.extend(agg.process(&make_complete(t0 + 2_000, t1, "gpt-4")));

        // Drain fires at t1 (watermark = t0+15s, interval = 10s, anchor = t0).
        let global = all
            .iter()
            .find(|m| {
                m.granularity == "10s"
                    && m.timestamp_us == t0
                    && m.wire_api == "*"
                    && m.model == "*"
                    && m.server_ip == "*"
            })
            .expect("global 10s row for t0");
        assert!(
            global.concurrency_max >= 3,
            "concurrency_max should be >= 3, got {}",
            global.concurrency_max
        );
        assert_eq!(global.request_count, 3);
    }

    #[test]
    fn dimension_expansion_on_flush_all() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));

        let metrics = agg.flush_all();
        let rows_10s: Vec<_> = metrics.iter().filter(|m| m.granularity == "10s").collect();
        assert_eq!(rows_10s.len(), 4);
        assert!(rows_10s
            .iter()
            .any(|m| m.wire_api == wa::OPENAI_CHAT && m.model == "gpt-4" && m.server_ip == "10.0.0.1"));
        assert!(rows_10s
            .iter()
            .any(|m| m.wire_api == wa::OPENAI_CHAT && m.model == "gpt-4" && m.server_ip == "*"));
        assert!(rows_10s
            .iter()
            .any(|m| m.wire_api == "*" && m.model == "*" && m.server_ip == "10.0.0.1"));
        assert!(rows_10s
            .iter()
            .any(|m| m.wire_api == "*" && m.model == "*" && m.server_ip == "*"));
    }
}
