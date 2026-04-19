use tdigest::TDigest;
use ts_llm::model::{FinishReason, LlmCall};

use crate::model::LlmMetric;

const BUFFER_CAPACITY: usize = 500;

/// Streaming-friendly approximate percentile tracker backed by t-digest.
///
/// Values are buffered locally and batch-merged into the digest when the buffer
/// reaches `BUFFER_CAPACITY` or when a query (`avg` / `quantile`) is requested.
struct DistributionDigest {
    digest: TDigest,
    buffer: Vec<f64>,
}

impl DistributionDigest {
    fn new() -> Self {
        Self {
            digest: TDigest::new_with_size(100),
            buffer: Vec::new(),
        }
    }

    fn add(&mut self, value: f64) {
        self.buffer.push(value);
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
        self.digest.count() as u64 + self.buffer.len() as u64
    }

    fn avg(&mut self) -> Option<f64> {
        if self.count() == 0 {
            return None;
        }
        self.compact();
        Some(self.digest.mean())
    }

    fn quantile(&mut self, q: f64) -> Option<f64> {
        if self.count() == 0 {
            return None;
        }
        self.compact();
        Some(self.digest.estimate_quantile(q))
    }
}

/// One (stream, granularity, window_start, dim) bucket. Accepts writes from
/// both `LlmEvent::Start` (traffic/concurrency) and `LlmEvent::Complete`
/// (tokens/errors/latency) over the life of a single cadence slice, then is
/// drained and dropped. Subsequent late-arriving Completes for the same window
/// land in a fresh bucket and produce additional rows.
pub struct WindowBucket {
    // Start-side: populated by on_call_start + sample_concurrency.
    pub request_count: u64,
    pub stream_count: u64,
    pub non_stream_count: u64,
    concurrency_samples: Vec<u32>,
    concurrency_max: u32,

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

    pub finish_complete_count: u64,
    pub finish_length_count: u64,
    pub finish_tool_use_count: u64,
    pub finish_error_count: u64,
    pub finish_cancelled_count: u64,

    // Latency distributions (milliseconds).
    ttfb: DistributionDigest,
    e2e: DistributionDigest,
    // TPOT distribution (ms/token) — streaming requests only.
    tpot: DistributionDigest,

    /// Running count of Complete events written since bucket creation.
    /// Combined with `request_count` forms the `has_data` check.
    complete_count: u64,
}

impl WindowBucket {
    pub fn new() -> Self {
        Self {
            request_count: 0,
            stream_count: 0,
            non_stream_count: 0,
            concurrency_samples: Vec::new(),
            concurrency_max: 0,
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
            finish_complete_count: 0,
            finish_length_count: 0,
            finish_tool_use_count: 0,
            finish_error_count: 0,
            finish_cancelled_count: 0,
            ttfb: DistributionDigest::new(),
            e2e: DistributionDigest::new(),
            tpot: DistributionDigest::new(),
            complete_count: 0,
        }
    }

    /// Called when a new LLM request is detected.
    pub fn on_call_start(&mut self, is_stream: bool) {
        self.request_count += 1;
        if is_stream {
            self.stream_count += 1;
        } else {
            self.non_stream_count += 1;
        }
    }

    /// Record a concurrency sample (called by the aggregator).
    pub fn sample_concurrency(&mut self, current: u32) {
        self.concurrency_samples.push(current);
        if current > self.concurrency_max {
            self.concurrency_max = current;
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

        if let Some(reason) = call.finish_reason {
            match reason {
                FinishReason::Complete => self.finish_complete_count += 1,
                FinishReason::Length => self.finish_length_count += 1,
                FinishReason::ToolUse => self.finish_tool_use_count += 1,
                FinishReason::Error => self.finish_error_count += 1,
                FinishReason::Cancelled => self.finish_cancelled_count += 1,
            }
        }

        if let Some(ttfb) = call.ttfb_ms {
            self.ttfb.add(ttfb);
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
        self.request_count > 0 || self.complete_count > 0
    }

    /// Flush this bucket into an `LlmMetric` row. Non-populated fields stay
    /// at their neutral default (0 for counters, `None` for averages /
    /// percentiles) — query-time SUM over such rows yields the correct total
    /// for additive fields.
    pub fn flush(
        &mut self,
        timestamp_us: i64,
        stream_id: &str,
        granularity: &'static str,
        provider: String,
        model: String,
        server_ip: String,
    ) -> LlmMetric {
        let concurrency_avg = if self.concurrency_samples.is_empty() {
            0.0
        } else {
            let sum: u64 = self.concurrency_samples.iter().map(|&v| v as u64).sum();
            sum as f64 / self.concurrency_samples.len() as f64
        };

        LlmMetric {
            timestamp_us,
            stream_id: stream_id.to_string(),
            granularity,
            provider,
            model,
            server_ip,
            request_count: self.request_count,
            stream_count: self.stream_count,
            non_stream_count: self.non_stream_count,
            concurrency_avg,
            concurrency_max: self.concurrency_max,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            input_tokens_avg: if self.input_token_count > 0 {
                Some(self.total_input_tokens as f64 / self.input_token_count as f64)
            } else {
                None
            },
            output_tokens_avg: if self.output_token_count > 0 {
                Some(self.total_output_tokens as f64 / self.output_token_count as f64)
            } else {
                None
            },
            total_cache_read_input_tokens: self.total_cache_read_input_tokens,
            total_cache_creation_input_tokens: self.total_cache_creation_input_tokens,
            error_count: self.error_count,
            error_4xx_count: self.error_4xx_count,
            error_429_count: self.error_429_count,
            error_5xx_count: self.error_5xx_count,
            finish_complete_count: self.finish_complete_count,
            finish_length_count: self.finish_length_count,
            finish_tool_use_count: self.finish_tool_use_count,
            finish_error_count: self.finish_error_count,
            finish_cancelled_count: self.finish_cancelled_count,
            ttfb_avg: self.ttfb.avg(),
            ttfb_p50: self.ttfb.quantile(0.5),
            ttfb_p95: self.ttfb.quantile(0.95),
            ttfb_p99: self.ttfb.quantile(0.99),
            e2e_avg: self.e2e.avg(),
            e2e_p50: self.e2e.quantile(0.5),
            e2e_p95: self.e2e.quantile(0.95),
            e2e_p99: self.e2e.quantile(0.99),
            tpot_avg: self.tpot.avg(),
            tpot_p50: self.tpot.quantile(0.5),
            tpot_p95: self.tpot.quantile(0.95),
            tpot_p99: self.tpot.quantile(0.99),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use ts_llm::model::{ApiType, FinishReason, LlmCall, ProviderFormat};

    #[test]
    fn digest_empty() {
        let mut d = DistributionDigest::new();
        assert_eq!(d.count(), 0);
        assert_eq!(d.avg(), None);
        assert_eq!(d.quantile(0.5), None);
    }

    #[test]
    fn digest_single_value() {
        let mut d = DistributionDigest::new();
        d.add(42.0);
        assert_eq!(d.count(), 1);
        assert_eq!(d.avg(), Some(42.0));
        assert_eq!(d.quantile(0.5), Some(42.0));
    }

    #[test]
    fn digest_multiple_values() {
        let mut d = DistributionDigest::new();
        for v in [10.0, 20.0, 30.0, 40.0, 50.0] {
            d.add(v);
        }
        assert_eq!(d.count(), 5);
        let avg = d.avg().unwrap();
        assert!((avg - 30.0).abs() < 0.01);
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
        let avg = d.avg().unwrap();
        assert!((avg - 299.5).abs() < 1.0);
    }

    fn test_call() -> LlmCall {
        LlmCall {
            stream_id: String::new(),
            id: "test".to_string(),
            provider: ProviderFormat::OpenAI,
            model: "gpt-4".to_string(),
            api_type: ApiType::Chat,
            tenant_id: None,
            request_time: 1_000_000,
            response_time: Some(1_100_000),
            complete_time: Some(2_000_000),
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
        assert_eq!(b.request_count, 3);
        assert_eq!(b.stream_count, 2);
        assert_eq!(b.non_stream_count, 1);
    }

    #[test]
    fn sample_concurrency_tracks_max() {
        let mut b = WindowBucket::new();
        b.sample_concurrency(3);
        b.sample_concurrency(5);
        b.sample_concurrency(2);
        assert_eq!(b.concurrency_max, 5);
        assert_eq!(b.concurrency_samples.len(), 3);
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
        let tpot = b.tpot.avg().unwrap();
        assert!((tpot - 20.0).abs() < 0.01);

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
        b.sample_concurrency(1);
        b.on_call_complete(&test_call());

        let m = b.flush(
            1_000_000,
            "s",
            "10s",
            "openai".to_string(),
            "gpt-4".to_string(),
            "10.0.0.1".to_string(),
        );
        // Start-side
        assert_eq!(m.request_count, 1);
        assert_eq!(m.stream_count, 1);
        assert_eq!(m.concurrency_max, 1);
        // Complete-side
        assert_eq!(m.total_input_tokens, 100);
        assert_eq!(m.total_output_tokens, 50);
        assert!(m.ttfb_avg.is_some());
        assert!(m.e2e_avg.is_some());
        assert!(m.tpot_avg.is_some());
        assert_eq!(m.finish_complete_count, 1);
    }

    #[test]
    fn flush_complete_only_leaves_start_side_zeroed() {
        // Late-arriving Complete that opens a fresh bucket (previous drain
        // removed the Start-side row). Start-side stays at zero so
        // `SUM(request_count)` across phases counts traffic exactly once.
        let mut b = WindowBucket::new();
        b.on_call_complete(&test_call());
        let m = b.flush(
            0,
            "s",
            "10s",
            "openai".to_string(),
            "gpt-4".to_string(),
            "10.0.0.1".to_string(),
        );
        assert_eq!(m.request_count, 0);
        assert_eq!(m.stream_count, 0);
        assert_eq!(m.concurrency_max, 0);
        assert!(m.ttfb_avg.is_some());
        assert_eq!(m.total_input_tokens, 100);
    }

    #[test]
    fn finish_reason_counting() {
        let mut b = WindowBucket::new();
        let mut call = test_call();

        call.finish_reason = Some(FinishReason::Complete);
        b.on_call_complete(&call);
        b.on_call_complete(&call);

        call.finish_reason = Some(FinishReason::Length);
        b.on_call_complete(&call);

        call.finish_reason = Some(FinishReason::ToolUse);
        b.on_call_complete(&call);

        call.finish_reason = Some(FinishReason::Cancelled);
        b.on_call_complete(&call);

        call.finish_reason = None;
        b.on_call_complete(&call);

        assert_eq!(b.finish_complete_count, 2);
        assert_eq!(b.finish_length_count, 1);
        assert_eq!(b.finish_tool_use_count, 1);
        assert_eq!(b.finish_error_count, 0);
        assert_eq!(b.finish_cancelled_count, 1);
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
