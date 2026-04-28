use std::collections::BTreeMap;

use tdigest::TDigest;
use ts_llm::model::LlmCall;

use crate::model::{LlmFinishMetric, LlmMetric, LlmMetricsBatch};

const BUFFER_CAPACITY: usize = 500;

/// Streaming percentile tracker backed by t-digest plus exact running
/// `sum` / `count`.
///
/// Values are buffered locally and batch-merged into the digest when the
/// buffer reaches `BUFFER_CAPACITY` or when `quantile()` is called. `sum`
/// and `count` are tracked exactly so query-time SUM over multiple rows
/// yields a true average; the digest's own mean is not used (it drifts
/// once values are compacted).
struct DistributionDigest {
    digest: TDigest,
    buffer: Vec<f64>,
    sum: f64,
    count: u64,
}

impl DistributionDigest {
    fn new() -> Self {
        Self {
            digest: TDigest::new_with_size(100),
            buffer: Vec::new(),
            sum: 0.0,
            count: 0,
        }
    }

    fn add(&mut self, value: f64) {
        self.buffer.push(value);
        self.sum += value;
        self.count += 1;
        if self.buffer.len() >= BUFFER_CAPACITY {
            self.compact();
        }
    }

    fn compact(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let values = std::mem::take(&mut self.buffer);
        self.digest = self.digest.merge_unsorted(values);
    }

    fn count(&self) -> u64 {
        self.count
    }

    fn sum(&self) -> f64 {
        self.sum
    }

    fn quantile(&mut self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        self.compact();
        Some(self.digest.estimate_quantile(q))
    }
}

/// One (source, granularity, window_start, dim) bucket. Accepts writes from
/// both `LlmEvent::Start` (traffic/active-calls) and `LlmEvent::Complete`
/// (tokens/errors/latency) over the life of a single cadence slice, then is
/// drained and dropped. Subsequent late-arriving Completes for the same window
/// land in a fresh bucket and produce additional rows.
pub struct WindowBucket {
    // Start-side: populated by on_call_start + sample_active_calls.
    pub call_count: u64,
    pub stream_count: u64,
    pub non_stream_count: u64,
    active_calls_sum: u64,
    active_calls_sample_count: u64,
    active_calls_max: u32,

    // Complete-side: populated by on_call_complete.
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    input_token_count: u64,
    output_token_count: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,

    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,

    /// Per-raw-string finish_reason counts. Keys are the verbatim provider
    /// values (`"end_turn"`, `"stop"`, `"tool_use"`, `"tool_calls"`, ...).
    pub finish_counts: BTreeMap<String, u64>,

    // Latency distributions (milliseconds).
    ttft: DistributionDigest,
    e2e: DistributionDigest,
    // TPOT distribution (ms/token) — streaming requests only.
    tpot: DistributionDigest,

    /// Running count of Complete events written since bucket creation.
    /// Combined with `call_count` forms the `has_data` check.
    complete_count: u64,
}

impl WindowBucket {
    pub fn new() -> Self {
        Self {
            call_count: 0,
            stream_count: 0,
            non_stream_count: 0,
            active_calls_sum: 0,
            active_calls_sample_count: 0,
            active_calls_max: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            input_token_count: 0,
            output_token_count: 0,
            total_cache_read_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            error_count: 0,
            error_4xx_count: 0,
            error_429_count: 0,
            error_5xx_count: 0,
            finish_counts: BTreeMap::new(),
            ttft: DistributionDigest::new(),
            e2e: DistributionDigest::new(),
            tpot: DistributionDigest::new(),
            complete_count: 0,
        }
    }

    /// Called when a new LLM request is detected.
    pub fn on_call_start(&mut self, is_stream: bool) {
        self.call_count += 1;
        if is_stream {
            self.stream_count += 1;
        } else {
            self.non_stream_count += 1;
        }
    }

    /// Record an active-calls sample (called by the aggregator).
    pub fn sample_active_calls(&mut self, current: u32) {
        self.active_calls_sum += current as u64;
        self.active_calls_sample_count += 1;
        if current > self.active_calls_max {
            self.active_calls_max = current;
        }
    }

    /// Called when an LLM call completes (response fully received).
    pub fn on_call_complete(&mut self, call: &LlmCall) {
        self.complete_count += 1;

        if let Some(it) = call.input_tokens {
            self.total_input_tokens += it as u64;
            self.input_token_count += 1;
        }
        if let Some(ot) = call.output_tokens {
            self.total_output_tokens += ot as u64;
            self.output_token_count += 1;
        }
        if let Some(t) = call.cache_read_input_tokens {
            self.total_cache_read_input_tokens += t as u64;
        }
        if let Some(t) = call.cache_creation_input_tokens {
            self.total_cache_creation_input_tokens += t as u64;
        }

        if let Some(status) = call.status_code {
            if status >= 400 {
                self.error_count += 1;
                if status >= 500 {
                    self.error_5xx_count += 1;
                } else if status == 429 {
                    self.error_429_count += 1;
                    self.error_4xx_count += 1;
                } else {
                    self.error_4xx_count += 1;
                }
            }
        }

        if let Some(reason) = call.finish_reason.as_deref() {
            *self.finish_counts.entry(reason.to_string()).or_insert(0) += 1;
        }

        if let Some(ttft) = call.ttft_ms {
            self.ttft.add(ttft);
        }
        if let Some(e2e) = call.e2e_latency_ms {
            self.e2e.add(e2e);
        }

        if call.is_stream {
            if let (Some(ot), Some(resp_time), Some(comp_time)) =
                (call.output_tokens, call.response_time, call.complete_time)
            {
                let duration_us = comp_time - resp_time;
                if duration_us > 0 && ot > 0 {
                    let duration_ms = duration_us as f64 / 1_000.0;
                    let ms_per_token = duration_ms / ot as f64;
                    self.tpot.add(ms_per_token);
                }
            }
        }
    }

    pub fn has_data(&self) -> bool {
        self.call_count > 0 || self.complete_count > 0
    }

    /// Flush this bucket into an `LlmMetricsBatch`: one wide `LlmMetric` row
    /// plus a long-format `LlmFinishMetric` per raw provider `finish_reason`
    /// observed. Non-populated fields stay at their neutral default (0 for
    /// counters, `None` for averages / percentiles) — query-time SUM over
    /// such rows yields the correct total for additive fields.
    pub fn flush(
        &mut self,
        timestamp_us: i64,
        source_id: &str,
        granularity: &'static str,
        wire_api: String,
        model: String,
        server_ip: String,
    ) -> LlmMetricsBatch {
        let finish_metrics: Vec<LlmFinishMetric> = self
            .finish_counts
            .iter()
            .map(|(reason, count)| LlmFinishMetric {
                timestamp_us,
                source_id: source_id.to_string(),
                granularity: granularity.to_string(),
                wire_api: wire_api.clone(),
                model: model.clone(),
                server_ip: server_ip.clone(),
                finish_reason: reason.clone(),
                count: *count,
            })
            .collect();

        let metric = LlmMetric {
            timestamp_us,
            source_id: source_id.to_string(),
            granularity,
            wire_api,
            model,
            server_ip,
            call_count: self.call_count,
            stream_count: self.stream_count,
            non_stream_count: self.non_stream_count,
            active_calls_sum: self.active_calls_sum,
            active_calls_sample_count: self.active_calls_sample_count,
            active_calls_max: self.active_calls_max,
            total_input_tokens: self.total_input_tokens,
            input_token_count: self.input_token_count,
            total_output_tokens: self.total_output_tokens,
            output_token_count: self.output_token_count,
            total_cache_read_input_tokens: self.total_cache_read_input_tokens,
            total_cache_creation_input_tokens: self.total_cache_creation_input_tokens,
            error_count: self.error_count,
            error_4xx_count: self.error_4xx_count,
            error_429_count: self.error_429_count,
            error_5xx_count: self.error_5xx_count,
            ttft_sum: self.ttft.sum(),
            ttft_count: self.ttft.count(),
            ttft_p50: self.ttft.quantile(0.5),
            ttft_p95: self.ttft.quantile(0.95),
            ttft_p99: self.ttft.quantile(0.99),
            e2e_sum: self.e2e.sum(),
            e2e_count: self.e2e.count(),
            e2e_p50: self.e2e.quantile(0.5),
            e2e_p95: self.e2e.quantile(0.95),
            e2e_p99: self.e2e.quantile(0.99),
            tpot_sum: self.tpot.sum(),
            tpot_count: self.tpot.count(),
            tpot_p50: self.tpot.quantile(0.5),
            tpot_p95: self.tpot.quantile(0.95),
            tpot_p99: self.tpot.quantile(0.99),
        };

        LlmMetricsBatch {
            metric,
            finish_metrics,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use ts_llm::model::{ApiType, LlmCall};
    use ts_llm::wire_apis as wa;

    #[test]
    fn digest_empty() {
        let mut d = DistributionDigest::new();
        assert_eq!(d.count(), 0);
        assert_eq!(d.sum(), 0.0);
        assert_eq!(d.quantile(0.5), None);
    }

    #[test]
    fn digest_single_value() {
        let mut d = DistributionDigest::new();
        d.add(42.0);
        assert_eq!(d.count(), 1);
        assert_eq!(d.sum(), 42.0);
        assert_eq!(d.quantile(0.5), Some(42.0));
    }

    #[test]
    fn digest_multiple_values() {
        let mut d = DistributionDigest::new();
        for v in [10.0, 20.0, 30.0, 40.0, 50.0] {
            d.add(v);
        }
        assert_eq!(d.count(), 5);
        // Exact sum (not digest mean).
        assert!((d.sum() - 150.0).abs() < 1e-9);
        let p50 = d.quantile(0.5).unwrap();
        assert!((p50 - 30.0).abs() < 5.0);
    }

    #[test]
    fn digest_compact_trigger() {
        let mut d = DistributionDigest::new();
        for i in 0..600 {
            d.add(i as f64);
        }
        assert_eq!(d.count(), 600);
        assert_eq!(d.buffer.len(), 100);
        // Sum of 0..600 = 179700, exact — digest compaction doesn't touch it.
        assert_eq!(d.sum(), 179_700.0);
    }

    fn test_call() -> LlmCall {
        LlmCall {
            source_id: String::new(),
            id: "test".to_string(),
            wire_api: wa::OPENAI_CHAT,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            request_time: 1_000_000,
            response_time: Some(1_100_000),
            complete_time: Some(2_000_000),
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
            e2e_latency_ms: Some(1000.0),
            client_ip: IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)),
            client_port: 12345,
            server_ip: IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            server_port: 443,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
        }
    }

    #[test]
    fn on_call_start_counts() {
        let mut b = WindowBucket::new();
        b.on_call_start(true);
        b.on_call_start(false);
        b.on_call_start(true);
        assert_eq!(b.call_count, 3);
        assert_eq!(b.stream_count, 2);
        assert_eq!(b.non_stream_count, 1);
    }

    #[test]
    fn sample_active_calls_tracks_sum_and_max() {
        let mut b = WindowBucket::new();
        b.sample_active_calls(3);
        b.sample_active_calls(5);
        b.sample_active_calls(2);
        assert_eq!(b.active_calls_max, 5);
        assert_eq!(b.active_calls_sample_count, 3);
        assert_eq!(b.active_calls_sum, 10);
    }

    #[test]
    fn tpot_streaming_only() {
        let mut b = WindowBucket::new();
        let mut call = test_call();
        call.output_tokens = Some(100);
        call.response_time = Some(1_000_000);
        call.complete_time = Some(3_000_000);
        call.is_stream = true;
        b.on_call_complete(&call);
        let tpot_avg = b.tpot.sum() / b.tpot.count() as f64;
        assert!((tpot_avg - 20.0).abs() < 0.01);

        let mut b2 = WindowBucket::new();
        let mut call2 = call.clone();
        call2.is_stream = false;
        b2.on_call_complete(&call2);
        assert_eq!(b2.tpot.count(), 0);
    }

    #[test]
    fn error_counting() {
        let mut b = WindowBucket::new();
        let mut call = test_call();

        call.status_code = Some(429);
        b.on_call_complete(&call);
        assert_eq!(b.error_count, 1);
        assert_eq!(b.error_4xx_count, 1);
        assert_eq!(b.error_429_count, 1);
        assert_eq!(b.error_5xx_count, 0);

        call.status_code = Some(500);
        b.on_call_complete(&call);
        assert_eq!(b.error_count, 2);
        assert_eq!(b.error_5xx_count, 1);
    }

    #[test]
    fn flush_merged_row_carries_both_sides() {
        let mut b = WindowBucket::new();
        b.on_call_start(true);
        b.sample_active_calls(1);
        b.on_call_complete(&test_call());

        let batch = b.flush(
            1_000_000,
            "s",
            "10s",
            wa::OPENAI_CHAT.to_string(),
            "gpt-4".to_string(),
            "10.0.0.1".to_string(),
        );
        let m = &batch.metric;
        // Start-side
        assert_eq!(m.call_count, 1);
        assert_eq!(m.stream_count, 1);
        assert_eq!(m.active_calls_max, 1);
        assert_eq!(m.active_calls_sample_count, 1);
        assert_eq!(m.active_calls_sum, 1);
        // Complete-side
        assert_eq!(m.total_input_tokens, 100);
        assert_eq!(m.input_token_count, 1);
        assert_eq!(m.total_output_tokens, 50);
        assert_eq!(m.output_token_count, 1);
        assert_eq!(m.ttft_count, 1);
        assert!(m.ttft_sum > 0.0);
        assert_eq!(m.e2e_count, 1);
        assert_eq!(m.tpot_count, 1);
        assert!(batch
            .finish_metrics
            .iter()
            .any(|f| f.finish_reason == "stop" && f.count == 1));
        // Per-row averages derived from sum/count.
        assert!(m.ttft_avg().is_some());
        assert!(m.e2e_avg().is_some());
        assert!(m.tpot_avg().is_some());
    }

    #[test]
    fn flush_complete_only_leaves_start_side_zeroed() {
        // Late-arriving Complete that opens a fresh bucket (previous drain
        // removed the Start-side row). Start-side stays at zero so
        // `SUM(call_count)` across rows counts traffic exactly once.
        let mut b = WindowBucket::new();
        b.on_call_complete(&test_call());
        let batch = b.flush(
            0,
            "s",
            "10s",
            wa::OPENAI_CHAT.to_string(),
            "gpt-4".to_string(),
            "10.0.0.1".to_string(),
        );
        let m = &batch.metric;
        assert_eq!(m.call_count, 0);
        assert_eq!(m.stream_count, 0);
        assert_eq!(m.active_calls_max, 0);
        assert_eq!(m.active_calls_sample_count, 0);
        // Complete-side populated — sum/count pair carries latency across SUM.
        assert_eq!(m.ttft_count, 1);
        assert!(m.ttft_sum > 0.0);
        assert_eq!(m.total_input_tokens, 100);
        assert_eq!(m.input_token_count, 1);
    }

    #[test]
    fn finish_reason_counting() {
        let mut b = WindowBucket::new();
        let mut call = test_call();

        call.finish_reason = Some("stop".to_string());
        b.on_call_complete(&call);
        b.on_call_complete(&call);

        call.finish_reason = Some("length".to_string());
        b.on_call_complete(&call);

        call.finish_reason = Some("tool_calls".to_string());
        b.on_call_complete(&call);

        call.finish_reason = Some("content_filter".to_string());
        b.on_call_complete(&call);

        call.finish_reason = None;
        b.on_call_complete(&call);

        assert_eq!(b.finish_counts.get("stop").copied(), Some(2));
        assert_eq!(b.finish_counts.get("length").copied(), Some(1));
        assert_eq!(b.finish_counts.get("tool_calls").copied(), Some(1));
        assert_eq!(b.finish_counts.get("content_filter").copied(), Some(1));
        // None should not have produced any entry.
        assert_eq!(b.finish_counts.values().sum::<u64>(), 5);
    }

    #[test]
    fn has_data_detects_either_side() {
        let mut b = WindowBucket::new();
        assert!(!b.has_data());
        b.on_call_start(true);
        assert!(b.has_data());

        let mut b2 = WindowBucket::new();
        b2.on_call_complete(&test_call());
        assert!(b2.has_data());
    }
}
