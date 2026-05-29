use std::fmt;

/// A pre-aggregated metrics record for one time window + dimension combination.
///
/// The aggregator drains each `(source, granularity, window_start, dims)`
/// bucket on a fixed per-granularity cadence. One call typically produces
/// one row; a call whose response straddles cadence boundaries produces
/// multiple rows against the same key (each carrying the increment observed
/// within that cadence slice). Query layers SUM rows across the key to
/// assemble the full window.
#[derive(Debug, Clone)]
pub struct LlmMetric {
    /// Window start timestamp (microseconds since epoch). Always derived from
    /// the call's `request_time` so late-arriving Complete data lands in the
    /// same window as the originating Start.
    pub timestamp_us: i64,
    /// Per-source dimension: one source == one independent aggregator. Today
    /// corresponds 1:1 with a configured capture source index; the data
    /// model keeps it as a first-class dimension so the physical capture ↔
    /// logical source mapping can diverge later without schema churn.
    pub source_id: String,
    /// Aggregation granularity.
    pub granularity: &'static str,
    /// Dimension values ("*" = wildcard / all).
    pub wire_api: String,
    pub model: String,
    pub server_ip: String,

    // Traffic
    pub call_count: u64,
    pub stream_count: u64,
    pub non_stream_count: u64,
    /// Running sum of per-call active-calls samples in this slice.
    /// Paired with `active_calls_sample_count` so the query layer can compute
    /// `SUM(active_calls_sum) / SUM(active_calls_sample_count)` as a true
    /// average across any set of rows.
    pub active_calls_sum: u64,
    pub active_calls_sample_count: u64,
    pub active_calls_max: u32,

    // Tokens. `total_input_tokens` pairs with `input_token_count` for
    // query-time avg; same pattern for output tokens.
    pub total_input_tokens: u64,
    pub input_token_count: u64,
    pub total_output_tokens: u64,
    pub output_token_count: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,

    // Errors
    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,

    // Finish-reason counts moved to the long-format `LlmFinishMetric` /
    // `llm_finish_metrics` table (Phase 4). One row per distinct raw provider
    // value, keyed by `(timestamp, dim..., finish_reason)` instead of fanning
    // each value into its own column on this wide row.

    // TTFT distribution (milliseconds).
    //
    // `*_sum` and `*_count` give exact averages under query-time SUM; the
    // per-row `*_p50/p95/p99` are t-digest estimates over *this row's slice
    // only*, re-weighted by `*_count` across rows at query time (an
    // approximation until sum+count is extended with serialized t-digest
    // bytes in a follow-up schema change).
    pub ttft_sum: f64,
    pub ttft_count: u64,
    pub ttft_p50: Option<f64>,
    pub ttft_p95: Option<f64>,
    pub ttft_p99: Option<f64>,
    // TTFT split by is_stream so the dashboard can render the two
    // distributions separately. Streaming TTFT is genuine "time to first
    // token"; non-streaming TTFT is "time to first response byte" and ≈
    // e2e on most servers. Both still feed `ttft_*` above for any
    // consumer that wants the combined view.
    pub ttft_stream_sum: f64,
    pub ttft_stream_count: u64,
    pub ttft_stream_p50: Option<f64>,
    pub ttft_stream_p95: Option<f64>,
    pub ttft_stream_p99: Option<f64>,
    pub ttft_nonstream_sum: f64,
    pub ttft_nonstream_count: u64,
    pub ttft_nonstream_p50: Option<f64>,
    pub ttft_nonstream_p95: Option<f64>,
    pub ttft_nonstream_p99: Option<f64>,

    // E2E latency distribution (milliseconds)
    pub e2e_sum: f64,
    pub e2e_count: u64,
    pub e2e_p50: Option<f64>,
    pub e2e_p95: Option<f64>,
    pub e2e_p99: Option<f64>,

    // TPOT distribution (ms/token) — streaming requests only
    pub tpot_sum: f64,
    pub tpot_count: u64,
    pub tpot_p50: Option<f64>,
    pub tpot_p95: Option<f64>,
    pub tpot_p99: Option<f64>,

    /// Tool-surface dimension key. None until aggregator dimension wiring (Task 16) lands.
    pub tool_surface: Option<String>,
}

/// One row of finish-reason counts in the long-format `llm_finish_metrics`
/// table. Emitted alongside `LlmMetric` by the bucket finalizer; one row per
/// distinct raw `finish_reason` observed in a given bucket dimension.
#[derive(Debug, Clone)]
pub struct LlmFinishMetric {
    pub timestamp_us: i64,
    pub source_id: String,
    pub granularity: String,
    pub wire_api: String,
    pub model: String,
    pub server_ip: String,
    /// Raw provider value: `end_turn`, `stop`, `pause_turn`, `STOP`, etc.
    pub finish_reason: String,
    pub count: u64,
}

/// One bucket flush emits exactly one `LlmMetric` (the wide row) and zero or
/// more `LlmFinishMetric` rows (one per distinct raw `finish_reason` seen in
/// the bucket). Carrying them together keeps the storage flush transactional
/// per bucket — both kinds land in the same write batch.
#[derive(Debug, Clone)]
pub struct LlmMetricsBatch {
    pub metric: LlmMetric,
    pub finish_metrics: Vec<LlmFinishMetric>,
}

fn safe_avg(sum: f64, count: u64) -> Option<f64> {
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
}

impl LlmMetric {
    /// Per-row average derived from `*_sum / *_count`. Useful for single-row
    /// views; query layer computes aggregated averages via SUM() separately.
    pub fn active_calls_avg(&self) -> f64 {
        if self.active_calls_sample_count == 0 {
            0.0
        } else {
            self.active_calls_sum as f64 / self.active_calls_sample_count as f64
        }
    }

    pub fn input_tokens_avg(&self) -> Option<f64> {
        safe_avg(self.total_input_tokens as f64, self.input_token_count)
    }

    pub fn output_tokens_avg(&self) -> Option<f64> {
        safe_avg(self.total_output_tokens as f64, self.output_token_count)
    }

    pub fn ttft_avg(&self) -> Option<f64> {
        safe_avg(self.ttft_sum, self.ttft_count)
    }

    pub fn ttft_stream_avg(&self) -> Option<f64> {
        safe_avg(self.ttft_stream_sum, self.ttft_stream_count)
    }

    pub fn ttft_nonstream_avg(&self) -> Option<f64> {
        safe_avg(self.ttft_nonstream_sum, self.ttft_nonstream_count)
    }

    pub fn e2e_avg(&self) -> Option<f64> {
        safe_avg(self.e2e_sum, self.e2e_count)
    }

    pub fn tpot_avg(&self) -> Option<f64> {
        safe_avg(self.tpot_sum, self.tpot_count)
    }
}

/// Format a timestamp (microseconds) as a simple datetime string.
fn format_ts(us: i64) -> String {
    let secs = us / 1_000_000;
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, s)
}

fn fmt_opt(v: Option<f64>, suffix: &str) -> String {
    match v {
        Some(val) => format!("{:.1}{}", val, suffix),
        None => "-".to_string(),
    }
}

impl fmt::Display for LlmMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "[Metric] {} | {} | source={} | {} / {} / {}",
            self.granularity,
            format_ts(self.timestamp_us),
            self.source_id,
            self.wire_api,
            self.model,
            self.server_ip,
        )?;
        writeln!(
            f,
            "  calls={} (stream={} non_stream={}) errors={} (4xx={} 429={} 5xx={}) active_calls avg={:.1} max={}",
            self.call_count,
            self.stream_count,
            self.non_stream_count,
            self.error_count,
            self.error_4xx_count,
            self.error_429_count,
            self.error_5xx_count,
            self.active_calls_avg(),
            self.active_calls_max,
        )?;
        writeln!(
            f,
            "  tokens: in={} out={} cache_read={} cache_create={}",
            self.total_input_tokens,
            self.total_output_tokens,
            self.total_cache_read_input_tokens,
            self.total_cache_creation_input_tokens,
        )?;
        writeln!(
            f,
            "  ttft: avg={} p50={} p95={} p99={}",
            fmt_opt(self.ttft_avg(), "ms"),
            fmt_opt(self.ttft_p50, "ms"),
            fmt_opt(self.ttft_p95, "ms"),
            fmt_opt(self.ttft_p99, "ms"),
        )?;
        writeln!(
            f,
            "  e2e:  avg={} p50={} p95={} p99={}",
            fmt_opt(self.e2e_avg(), "ms"),
            fmt_opt(self.e2e_p50, "ms"),
            fmt_opt(self.e2e_p95, "ms"),
            fmt_opt(self.e2e_p99, "ms"),
        )?;
        write!(
            f,
            "  tpot: avg={} p50={} p95={} p99={}",
            fmt_opt(self.tpot_avg(), "ms/t"),
            fmt_opt(self.tpot_p50, "ms/t"),
            fmt_opt(self.tpot_p95, "ms/t"),
            fmt_opt(self.tpot_p99, "ms/t"),
        )
    }
}
