use std::collections::HashMap;

use h_common::agent::ToolSurface;
use h_common::internal_metrics::{Metric, MetricsWorker};
use h_llm::model::{LlmCall, LlmCallStart, LlmEvent};

use crate::bucket::WindowBucket;
use crate::model::LlmMetricsBatch;

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

/// Composite key for a metrics bucket: (source, granularity, window_start, dim).
///
/// `tool_surface` is `None` for `LlmEvent::Start` writes (the surface is only
/// observable once the response body has been parsed in the LLM stage) and
/// carries the call's classified surface for `LlmEvent::Complete` writes. The
/// resulting per-window split is intentional: Start-side counters (call_count,
/// stream_count, active_calls) aggregate untagged while Complete-side
/// counters (tokens, errors, latency, finish_reason) carry the surface tag.
/// Query layers sum across surfaces to recover the pre-Task-16 totals.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BucketKey {
    source_id: String,
    granularity_idx: usize,
    window_start_us: i64,
    wire_api: String,
    model: String,
    server_ip: String,
    tool_surface: Option<ToolSurface>,
}

/// Dimension key for active-calls tracking (no window or granularity).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DimensionKey {
    source_id: String,
    wire_api: String,
    model: String,
    server_ip: String,
}

/// Aggregates LlmEvents into time-windowed LlmMetric records.
///
/// Each `(source, granularity, window_start(request_time), dim)` key maps
/// to one `WindowBucket` that receives writes from both `LlmEvent::Start`
/// (traffic/active-calls) and `LlmEvent::Complete` (tokens/errors/latency).
/// The bucket is drained on a per-`(source, granularity)` cadence aligned
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
    active_calls: HashMap<DimensionKey, i64>,
    /// Per-source event-time watermark, advanced by every event.
    latest_ts: HashMap<String, i64>,
    /// Per-(source, granularity_idx) cadence anchor for bucket drain.
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
            active_calls: HashMap::new(),
            latest_ts: HashMap::new(),
            last_flush_ts: HashMap::new(),
            metrics,
        }
    }

    /// Number of in-window aggregation slots awaiting drain. Sawtooth: rises
    /// as events land in fresh windows, drops as `maybe_drain` removes the
    /// keys whose window boundary has been crossed.
    pub fn open_bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Size of the per-dimension active-call counter map. Entries are never
    /// removed (only clamped to 0 on Complete), so this gauge monotonically
    /// reflects the cumulative cardinality of distinct
    /// `(source_id, wire_api, model, server_ip)` combinations seen — a leak
    /// canary for runaway dimension cardinality.
    pub fn concurrency_table_size(&self) -> usize {
        self.active_calls.len()
    }

    /// Process an LlmEvent. Returns any metric batches emitted by cadence
    /// drain. Each batch carries one wide `LlmMetric` row plus zero or more
    /// long-format `LlmFinishMetric` rows for the same bucket.
    pub fn process(&mut self, event: &LlmEvent) -> Vec<LlmMetricsBatch> {
        let (source_id, ts) = event_clock(event);

        match event {
            LlmEvent::Start(start) => {
                self.metrics.counter(Metric::MetricsLlmEventsStart).inc();
                self.on_call_start(start);
            }
            LlmEvent::Complete { call, .. } => {
                self.metrics.counter(Metric::MetricsLlmEventsComplete).inc();
                self.on_call_complete(call);
            }
            LlmEvent::Heartbeat { .. } => {
                self.metrics
                    .counter(Metric::MetricsHeartbeatsReceived)
                    .inc();
            }
        }

        let watermark = {
            let entry = self.latest_ts.entry(source_id.clone()).or_insert(0);
            *entry = (*entry).max(ts);
            *entry
        };

        self.maybe_drain(&source_id, watermark)
    }

    /// Flush all remaining buckets. Call at end of capture.
    pub fn flush_all(&mut self) -> Vec<LlmMetricsBatch> {
        let keys: Vec<_> = self.buckets.keys().cloned().collect();
        let mut batches = Vec::new();
        for key in keys {
            if let Some(mut bucket) = self.buckets.remove(&key) {
                if bucket.has_data() {
                    batches.push(bucket.flush(
                        key.window_start_us,
                        &key.source_id,
                        GRANULARITIES[key.granularity_idx].label,
                        key.wire_api,
                        key.model,
                        key.server_ip,
                        key.tool_surface,
                    ));
                    self.metrics.counter(Metric::MetricsWindowsEmitted).inc();
                }
            }
        }
        batches.sort_by(|a, b| {
            a.metric
                .granularity
                .cmp(b.metric.granularity)
                .then(a.metric.timestamp_us.cmp(&b.metric.timestamp_us))
                .then(a.metric.wire_api.cmp(&b.metric.wire_api))
                .then(a.metric.model.cmp(&b.metric.model))
                .then(a.metric.server_ip.cmp(&b.metric.server_ip))
                .then(a.metric.tool_surface.cmp(&b.metric.tool_surface))
        });
        batches
    }

    fn on_call_start(&mut self, start: &LlmCallStart) {
        let dim_keys = dimension_keys(
            &start.source_id,
            &start.wire_api.to_string(),
            &start.model,
            &start.server_ip.to_string(),
        );

        let mut active_calls_values = [0u32; 4];
        for (i, dk) in dim_keys.iter().enumerate() {
            let entry = self.active_calls.entry(dk.clone()).or_insert(0);
            *entry += 1;
            active_calls_values[i] = (*entry).max(0) as u32;
        }

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let ws = window_start(start.timestamp_us, gran.window_secs);
            self.last_flush_ts
                .entry((start.source_id.clone(), gi))
                .or_insert(ws);
            for (di, dk) in dim_keys.iter().enumerate() {
                let bk = BucketKey {
                    source_id: start.source_id.clone(),
                    granularity_idx: gi,
                    window_start_us: ws,
                    wire_api: dk.wire_api.clone(),
                    model: dk.model.clone(),
                    server_ip: dk.server_ip.clone(),
                    // Start events don't carry a tool surface — that's
                    // decided when the response body is classified on Complete.
                    tool_surface: None,
                };
                let bucket = self.buckets.entry(bk).or_insert_with(WindowBucket::new);
                bucket.on_call_start(start.is_stream);
                bucket.sample_active_calls(active_calls_values[di]);
            }
        }
    }

    fn on_call_complete(&mut self, call: &LlmCall) {
        let dim_keys = dimension_keys(
            &call.source_id,
            &call.wire_api.to_string(),
            &call.model,
            &call.server_ip.to_string(),
        );

        for dk in &dim_keys {
            let entry = self.active_calls.entry(dk.clone()).or_insert(0);
            *entry = (*entry - 1).max(0);
        }

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let ws = window_start(call.request_time, gran.window_secs);
            // Same cadence-anchor init as on_call_start — covers captures
            // that start mid-flow and see Complete without a prior Start.
            self.last_flush_ts
                .entry((call.source_id.clone(), gi))
                .or_insert(ws);
            for dk in &dim_keys {
                let bk = BucketKey {
                    source_id: call.source_id.clone(),
                    granularity_idx: gi,
                    window_start_us: ws,
                    wire_api: dk.wire_api.clone(),
                    model: dk.model.clone(),
                    server_ip: dk.server_ip.clone(),
                    tool_surface: call.tool_surface,
                };
                let bucket = self.buckets.entry(bk).or_insert_with(WindowBucket::new);
                bucket.on_call_complete(call);
            }
        }
    }

    /// Per-granularity cadence drain. For each `(source, gran)` pair where
    /// at least `gran.window_secs` of event-time have elapsed since the
    /// anchor, emit and clear all dirty buckets for that pair.
    fn maybe_drain(&mut self, source_id: &str, now_ts: i64) -> Vec<LlmMetricsBatch> {
        let mut batches = Vec::new();

        for (gi, gran) in GRANULARITIES.iter().enumerate() {
            let interval_us = gran.window_secs * 1_000_000;
            let last = match self.last_flush_ts.get(&(source_id.to_string(), gi)) {
                Some(&t) => t,
                None => continue,
            };
            if now_ts - last < interval_us {
                continue;
            }
            self.last_flush_ts
                .insert((source_id.to_string(), gi), now_ts);

            let dirty_keys: Vec<_> = self
                .buckets
                .keys()
                .filter(|k| k.source_id == source_id && k.granularity_idx == gi)
                .cloned()
                .collect();

            for key in dirty_keys {
                if let Some(mut bucket) = self.buckets.remove(&key) {
                    if bucket.has_data() {
                        batches.push(bucket.flush(
                            key.window_start_us,
                            &key.source_id,
                            gran.label,
                            key.wire_api,
                            key.model,
                            key.server_ip,
                            key.tool_surface,
                        ));
                        self.metrics.counter(Metric::MetricsWindowsEmitted).inc();
                    }
                }
            }
        }

        batches
    }
}

/// Extract `(source_id, event_time_us)` from an event. Event time:
/// * `Start` — `timestamp_us` (packet time of the request).
/// * `Complete` — `complete_time.unwrap_or(request_time)` (latest real
///   event-time we have for this call; `request_time` is only a fallback
///   and does not meaningfully advance the watermark on its own).
/// * `Heartbeat` — `ts` (synthetic packet time from capture).
fn event_clock(event: &LlmEvent) -> (String, i64) {
    match event {
        LlmEvent::Start(s) => (s.source_id.clone(), s.timestamp_us),
        LlmEvent::Complete { call, .. } => (
            call.source_id.clone(),
            call.complete_time.unwrap_or(call.request_time),
        ),
        LlmEvent::Heartbeat { ts, source_id } => (source_id.clone(), *ts),
    }
}

/// Compute the window start timestamp for a given event time and window size.
fn window_start(timestamp_us: i64, window_secs: i64) -> i64 {
    let window_us = window_secs * 1_000_000;
    (timestamp_us / window_us) * window_us
}

/// Generate the 4 dimension keys for a single event.
fn dimension_keys(
    source_id: &str,
    wire_api: &str,
    model: &str,
    server_ip: &str,
) -> [DimensionKey; 4] {
    [
        DimensionKey {
            source_id: source_id.to_string(),
            wire_api: wire_api.to_string(),
            model: model.to_string(),
            server_ip: server_ip.to_string(),
        },
        DimensionKey {
            source_id: source_id.to_string(),
            wire_api: wire_api.to_string(),
            model: model.to_string(),
            server_ip: "*".to_string(),
        },
        DimensionKey {
            source_id: source_id.to_string(),
            wire_api: "*".to_string(),
            model: "*".to_string(),
            server_ip: server_ip.to_string(),
        },
        DimensionKey {
            source_id: source_id.to_string(),
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
    use h_llm::model::{ApiType, LlmCall, LlmCallStart};
    use h_llm::wire_apis as wa;

    fn test_metrics() -> MetricsWorker {
        use h_common::internal_metrics::MetricsSystem;
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[
                Metric::MetricsLlmEventsStart,
                Metric::MetricsLlmEventsComplete,
                Metric::MetricsHeartbeatsReceived,
                Metric::MetricsWindowsEmitted,
            ],
        );
        let _svc = sys.start();
        w
    }

    fn make_start(ts_us: i64, model: &str, is_stream: bool) -> LlmEvent {
        make_start_with_source(ts_us, model, is_stream, "")
    }

    fn make_start_with_source(ts_us: i64, model: &str, is_stream: bool, sid: &str) -> LlmEvent {
        LlmEvent::Start(LlmCallStart {
            source_id: sid.to_string(),
            wire_api: wa::OPENAI_CHAT,
            model: model.to_string(),
            is_stream,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            timestamp_us: ts_us,
        })
    }

    fn make_complete(request_time: i64, complete_time: i64, model: &str) -> LlmEvent {
        make_complete_with_source(request_time, complete_time, model, "")
    }

    fn make_complete_with_source(
        request_time: i64,
        complete_time: i64,
        model: &str,
        sid: &str,
    ) -> LlmEvent {
        LlmEvent::Complete {
            call: Arc::new(LlmCall {
                source_id: sid.to_string(),
                id: format!("c-{request_time}-{complete_time}"),
                wire_api: wa::OPENAI_CHAT,
                model: model.to_string(),
                api_type: ApiType::Chat,
                request_time,
                response_time: Some(request_time + 100_000),
                complete_time: Some(complete_time),
                request_path: "/v1/chat/completions".to_string(),
                is_stream: true,
                request_body: None,
                status_code: Some(200),
                finish_reason: Some("stop".to_string()),
                response_body: None,
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                ttft_ms: Some(100.0),
                e2e_latency_ms: Some(500.0),
                client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
                client_port: 12345,
                server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
                server_port: 443,
                response_id: None,
                request_headers: vec![],
                response_headers: vec![],
                is_agent_request: false,
                tool_surface: None,
                agent_topology: None,
                tool_call_count: 0,
                tool_names: vec![],
            }),
            agent: None,
        }
    }

    fn make_heartbeat(ts_us: i64, sid: &str) -> LlmEvent {
        LlmEvent::Heartbeat {
            ts: ts_us,
            source_id: sid.to_string(),
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
            .filter(|b| b.metric.granularity == "10s" && b.metric.timestamp_us == t0)
            .collect();
        assert_eq!(rows_10s.len(), 4, "one merged row per dim");
        let finest = rows_10s
            .iter()
            .find(|b| b.metric.model == "gpt-4" && b.metric.server_ip != "*")
            .expect("finest dim present");
        assert_eq!(finest.metric.call_count, 1);
        assert_eq!(finest.metric.total_input_tokens, 100);
        assert_eq!(finest.metric.ttft_count, 1);
        assert!(finest.metric.ttft_sum > 0.0);
        assert!(finest
            .finish_metrics
            .iter()
            .any(|f| f.finish_reason == "stop" && f.count == 1));
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
        // Start row already emitted (call_count=1, tokens=0).
        let start_row = all
            .iter()
            .find(|b| {
                b.metric.granularity == "10s"
                    && b.metric.timestamp_us == t0
                    && b.metric.model == "gpt-4"
                    && b.metric.server_ip != "*"
            })
            .expect("start row for t0");
        assert_eq!(start_row.metric.call_count, 1);
        assert_eq!(start_row.metric.total_input_tokens, 0);
        assert_eq!(start_row.metric.ttft_count, 0);
        assert_eq!(start_row.metric.ttft_sum, 0.0);

        // Complete returns late.
        all.extend(agg.process(&make_complete(t0, t0 + 35_000_000, "gpt-4")));
        // Trigger next cadence drain.
        all.extend(agg.process(&make_heartbeat(t0 + 45_000_000, "")));

        // Second row for the same window carries Complete payload.
        let complete_rows: Vec<_> = all
            .iter()
            .filter(|b| {
                b.metric.granularity == "10s"
                    && b.metric.timestamp_us == t0
                    && b.metric.model == "gpt-4"
                    && b.metric.server_ip != "*"
            })
            .collect();
        assert_eq!(
            complete_rows.len(),
            2,
            "two rows for same window: start + late complete"
        );
        let late = complete_rows
            .iter()
            .find(|b| b.metric.total_input_tokens > 0)
            .expect("late complete row");
        assert_eq!(
            late.metric.call_count, 0,
            "late complete row has zero traffic"
        );
        assert_eq!(late.metric.total_input_tokens, 100);
        assert_eq!(late.metric.ttft_count, 1);
        assert!(late.metric.ttft_sum > 0.0);
    }

    #[test]
    fn sum_traffic_across_rows_is_not_double_counted() {
        // Fast call + slow call in same window: we expect traffic SUM over
        // all rows for that window to equal 2 (not inflated by Complete
        // rows that carry call_count=0).
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
            .filter(|b| {
                b.metric.granularity == "10s"
                    && b.metric.timestamp_us == t0
                    && b.metric.model == "gpt-4"
                    && b.metric.server_ip != "*"
            })
            .map(|b| b.metric.call_count)
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

        assert!(flushed.iter().any(|b| b.metric.granularity == "10s"));
        assert!(
            !flushed.iter().any(|b| b.metric.granularity == "1m"),
            "1m cadence must not fire at 10s"
        );
    }

    #[test]
    fn drain_by_own_complete_event_without_heartbeat() {
        // A source that never sees heartbeats still drains itself via its
        // own Complete events crossing the cadence boundary.
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));
        // Next Complete's complete_time pushes watermark past 10s boundary.
        let flushed = agg.process(&make_complete(t0, t0 + 15_000_000, "gpt-4"));

        let rows_10s: Vec<_> = flushed
            .iter()
            .filter(|b| b.metric.granularity == "10s" && b.metric.timestamp_us == t0)
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
            !immediate.iter().any(|b| b.metric.granularity == "10s"),
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
            assert_eq!(
                metrics
                    .iter()
                    .filter(|b| b.metric.granularity == gran)
                    .count(),
                4
            );
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
            .filter(|b| {
                b.metric.granularity == "10s"
                    && b.metric.timestamp_us == t0
                    && b.metric.model == "gpt-4"
                    && b.metric.server_ip != "*"
            })
            .collect();
        assert!(
            rows_t0.len() >= 2,
            "expected ≥2 rows for same window, got {}",
            rows_t0.len()
        );
    }

    #[test]
    fn multi_source_independent_watermarks() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;
        let t1 = t0 + 15_000_000;

        // Stream s0 advances past 10s boundary.
        agg.process(&make_start_with_source(t0, "gpt-4", true, "s0"));
        agg.process(&make_complete_with_source(t0, t0 + 500_000, "gpt-4", "s0"));
        let s0_flushed = agg.process(&make_start_with_source(t1, "gpt-4", true, "s0"));

        let s0_rows: Vec<_> = s0_flushed
            .iter()
            .filter(|b| {
                b.metric.granularity == "10s"
                    && b.metric.source_id == "s0"
                    && b.metric.timestamp_us == t0
            })
            .collect();
        assert_eq!(s0_rows.len(), 4, "s0 drains for t0");

        // Stream s1 still inside [t0, t0+10s) — its window must not drain.
        agg.process(&make_start_with_source(t0, "gpt-4", true, "s1"));
        let s1_flushed = agg.process(&make_complete_with_source(t0, t0 + 500_000, "gpt-4", "s1"));
        assert_eq!(
            s1_flushed
                .iter()
                .filter(|b| b.metric.granularity == "10s" && b.metric.source_id == "s1")
                .count(),
            0
        );
    }

    #[test]
    fn active_calls_tracked_in_merged_row() {
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
            .find(|b| {
                b.metric.granularity == "10s"
                    && b.metric.timestamp_us == t0
                    && b.metric.wire_api == "*"
                    && b.metric.model == "*"
                    && b.metric.server_ip == "*"
            })
            .expect("global 10s row for t0");
        assert!(
            global.metric.active_calls_max >= 3,
            "active_calls_max should be >= 3, got {}",
            global.metric.active_calls_max
        );
        assert_eq!(global.metric.call_count, 3);
    }

    #[test]
    fn dimension_expansion_on_flush_all() {
        let mut agg = MetricsAggregator::new(test_metrics());
        let t0 = 1_700_000_000_000_000i64;

        agg.process(&make_start(t0, "gpt-4", true));
        agg.process(&make_complete(t0, t0 + 500_000, "gpt-4"));

        let metrics = agg.flush_all();
        let rows_10s: Vec<_> = metrics
            .iter()
            .filter(|b| b.metric.granularity == "10s")
            .collect();
        assert_eq!(rows_10s.len(), 4);
        assert!(rows_10s.iter().any(|b| b.metric.wire_api == wa::OPENAI_CHAT
            && b.metric.model == "gpt-4"
            && b.metric.server_ip == "10.0.0.1"));
        assert!(rows_10s.iter().any(|b| b.metric.wire_api == wa::OPENAI_CHAT
            && b.metric.model == "gpt-4"
            && b.metric.server_ip == "*"));
        assert!(rows_10s.iter().any(|b| b.metric.wire_api == "*"
            && b.metric.model == "*"
            && b.metric.server_ip == "10.0.0.1"));
        assert!(rows_10s.iter().any(|b| b.metric.wire_api == "*"
            && b.metric.model == "*"
            && b.metric.server_ip == "*"));
    }
}
