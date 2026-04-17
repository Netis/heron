use std::fmt;

/// A pre-aggregated metrics record for one time window + dimension combination.
#[derive(Debug, Clone)]
pub struct LlmMetric {
    /// Window start timestamp (microseconds since epoch).
    pub timestamp_us: i64,
    /// Per-source dimension: one stream == one independent aggregator. Today
    /// corresponds 1:1 with a configured capture source index, but the metrics
    /// data model treats it as a stable `stream` abstraction so captures and
    /// streams can diverge later without schema churn.
    pub stream_id: String,
    /// Aggregation granularity.
    pub granularity: &'static str,
    /// Dimension values ("*" = wildcard / all).
    pub provider: String,
    pub model: String,
    pub server_ip: String,

    // Traffic
    pub request_count: u64,
    pub stream_count: u64,
    pub non_stream_count: u64,
    pub concurrency_avg: f64,
    pub concurrency_max: u32,

    // Tokens
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub input_tokens_avg: Option<f64>,
    pub output_tokens_avg: Option<f64>,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,

    // Errors
    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,

    // Finish reason counts
    pub finish_complete_count: u64,
    pub finish_length_count: u64,
    pub finish_tool_use_count: u64,
    pub finish_error_count: u64,
    pub finish_cancelled_count: u64,

    // TTFB distribution (milliseconds)
    pub ttfb_avg: Option<f64>,
    pub ttfb_p50: Option<f64>,
    pub ttfb_p95: Option<f64>,
    pub ttfb_p99: Option<f64>,

    // E2E latency distribution (milliseconds)
    pub e2e_avg: Option<f64>,
    pub e2e_p50: Option<f64>,
    pub e2e_p95: Option<f64>,
    pub e2e_p99: Option<f64>,

    // TPOT distribution (ms/token) — streaming requests only
    pub tpot_avg: Option<f64>,
    pub tpot_p50: Option<f64>,
    pub tpot_p95: Option<f64>,
    pub tpot_p99: Option<f64>,
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
            "[Metric] {} | {} | stream={} | {} / {} / {}",
            self.granularity,
            format_ts(self.timestamp_us),
            self.stream_id,
            self.provider,
            self.model,
            self.server_ip,
        )?;
        writeln!(
            f,
            "  requests={} (stream={} non_stream={}) errors={} (4xx={} 429={} 5xx={}) concurrency avg={:.1} max={}",
            self.request_count,
            self.stream_count,
            self.non_stream_count,
            self.error_count,
            self.error_4xx_count,
            self.error_429_count,
            self.error_5xx_count,
            self.concurrency_avg,
            self.concurrency_max,
        )?;
        writeln!(
            f,
            "  tokens: in={} out={} cache_read={} cache_create={} | finish: ok={} len={} tool={} err={} cancel={}",
            self.total_input_tokens,
            self.total_output_tokens,
            self.total_cache_read_input_tokens,
            self.total_cache_creation_input_tokens,
            self.finish_complete_count,
            self.finish_length_count,
            self.finish_tool_use_count,
            self.finish_error_count,
            self.finish_cancelled_count,
        )?;
        writeln!(
            f,
            "  ttfb: avg={} p50={} p95={} p99={}",
            fmt_opt(self.ttfb_avg, "ms"),
            fmt_opt(self.ttfb_p50, "ms"),
            fmt_opt(self.ttfb_p95, "ms"),
            fmt_opt(self.ttfb_p99, "ms"),
        )?;
        writeln!(
            f,
            "  e2e:  avg={} p50={} p95={} p99={}",
            fmt_opt(self.e2e_avg, "ms"),
            fmt_opt(self.e2e_p50, "ms"),
            fmt_opt(self.e2e_p95, "ms"),
            fmt_opt(self.e2e_p99, "ms"),
        )?;
        write!(
            f,
            "  tpot: avg={} p50={} p95={} p99={}",
            fmt_opt(self.tpot_avg, "ms/t"),
            fmt_opt(self.tpot_p50, "ms/t"),
            fmt_opt(self.tpot_p95, "ms/t"),
            fmt_opt(self.tpot_p99, "ms/t"),
        )
    }
}
