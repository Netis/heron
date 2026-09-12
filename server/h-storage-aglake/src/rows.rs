//! Domain structs ↔ HEC events.
//!
//! Every event type here is both the writer and the reader: one `#[derive]`
//! pair per entity, so an encode/decode drift is impossible by construction.
//!
//! # Encoding rules
//!
//! * **Timestamps are written twice.** The HEC envelope's `time` drives bucket
//!   routing and pruning, but it round-trips through `f64` seconds and comes
//!   back out of search as `f64` too, which loses microseconds — measured at
//!   3.3% of rows off by 1 µs when truncating. So every event also carries the
//!   integer `ts_us`, and that is the authoritative value for ordering,
//!   cursors, and anything read back into a struct.
//! * **`None` omits the key.** Search output drops null fields entirely, so
//!   there is no way to distinguish an explicit null from an absent key;
//!   omitting is the smaller, equivalent encoding.
//! * **Bools are written twice**, as `true`/`false` and as a 0/1 twin
//!   (`strm`, `err`, `sse`). The integer form makes `sum(strm)` work and gives
//!   an exact posting to match on.
//! * **Range predicates are precomputed.** aglake cannot push down `<`, `>`,
//!   `!=` or `NOT`, so anything a query would need to compare is turned into a
//!   categorical value at write time (`err_class`, `strm`, `dim_tier`).
//! * **Arrays go in as JSON strings** (`*_json`), matching how the ClickHouse
//!   backend stores them, so `h_storage::convert::parse_json_string_list`
//!   decodes both without new code. The one array also written in native form
//!   is `models_used`, which queries need to match against.
//!
//! # Two rules that came out of measurement, not design
//!
//! **aglake re-serializes object events with keys sorted.** An event posted as
//! `{"id":…,"source_id":…}` comes back out of search as
//! `{"err":0,"err_class":…,"id":…}` — byte order, nested objects included.
//! Declaration order therefore survives only for events posted as a
//! **pre-serialized string**, which is what [`raw_envelope`] is for. Bodies go
//! that way for two reasons: it keeps `span_id` at the front where an anchored
//! regex can pull it out without scanning ~320 KiB, and it saves aglake a
//! parse-and-reserialize of that same payload on every write. Metadata events
//! stay objects — reordering costs them nothing because they are read back
//! through serde, which does not care about order, and being objects is what
//! gives them free field extraction.
//!
//! **Every read struct tolerates missing fields** (`#[serde(default)]` on the
//! container). aglake has no DDL and no backfill: events written last month
//! keep whatever shape they had. Adding a field to one of these structs would
//! otherwise make every older event fail to deserialize — turning a schema
//! addition into silent data loss across the retention window. Defaulting
//! degrades that to one zero-valued field on old rows.

use serde::{Deserialize, Serialize};

use h_llm::model::LlmCall;
use h_metrics::model::{LlmFinishMetric, LlmMetric};
use h_protocol::HttpExchange;
use h_turn::Trace;

use crate::spl::epoch_secs;

/// HEC envelope. `host` doubles as the source id so `host=` filtering and the
/// native UI's grouping both work without extra configuration.
#[derive(Serialize)]
pub(crate) struct Envelope<T: Serialize> {
    pub time: String,
    pub host: String,
    pub source: &'static str,
    pub sourcetype: &'static str,
    pub index: String,
    pub event: T,
}

impl<T: Serialize> Envelope<T> {
    pub(crate) fn new(
        ts_us: i64,
        host: &str,
        sourcetype: &'static str,
        index: &str,
        event: T,
    ) -> Self {
        Self {
            time: epoch_secs(ts_us),
            host: host.to_string(),
            source: "heron",
            sourcetype,
            index: index.to_string(),
            event,
        }
    }

    pub(crate) fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Encode an event whose byte layout must survive ingest verbatim.
///
/// Posting the payload as a JSON **string** rather than an object makes aglake
/// store exactly these bytes (verified: a string event round-trips unchanged,
/// while an object event comes back with its keys sorted). Field lookups and
/// full-text search both still work on it — aglake parses the raw JSON at
/// search time — so this costs nothing on the read side.
pub(crate) fn raw_envelope<T: Serialize>(
    ts_us: i64,
    host: &str,
    sourcetype: &'static str,
    index: &str,
    event: &T,
) -> Result<String, serde_json::Error> {
    let payload = serde_json::to_string(event)?;
    Envelope::new(ts_us, host, sourcetype, index, payload).encode()
}

pub(crate) const ST_SPAN: &str = "heron_span";
pub(crate) const ST_BODY: &str = "heron_body";
pub(crate) const ST_TRACE: &str = "heron_trace";
pub(crate) const ST_METRIC: &str = "heron_metric";
pub(crate) const ST_FINISH: &str = "heron_finish";
pub(crate) const ST_HTTP: &str = "heron_http";
pub(crate) const ST_HTTP_BODY: &str = "heron_http_body";

// ---------------------------------------------------------------------------
// spans
// ---------------------------------------------------------------------------

/// Span metadata. Everything here is a scalar that queries filter, sort, or
/// aggregate on — bodies and headers live in [`BodyEvent`].
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct SpanEvent {
    pub id: String,
    pub source_id: String,
    /// Authoritative request timestamp, microseconds.
    pub ts_us: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resp_us: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_us: Option<i64>,
    pub wire_api: String,
    pub model: String,
    pub api_type: String,
    pub is_stream: bool,
    /// 0/1 twin of `is_stream`, so `sum(strm)` counts streams directly.
    pub strm: u8,
    pub request_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// 1 when `status_code >= 400`. Precomputed because `>=` cannot be pushed
    /// down.
    pub err: u8,
    /// `ok` / `4xx` / `429` / `5xx` — the error buckets metrics needs, as
    /// exact-match values rather than range predicates.
    pub err_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// Precomputed so the list and trace-detail paths never have to read a
    /// response body just to label a token count. Both SQL backends derive
    /// this at read time and consequently disagree with each other in
    /// body-less mode; deriving it once at write time makes it correct in
    /// every mode here.
    pub tokens_estimated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2e_latency_ms: Option<f64>,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub is_agent_request: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_topology: Option<String>,
    pub tool_call_count: u32,
    pub tool_names_json: String,
    pub body_bytes_dropped: u64,
    /// Whether a companion body event was written; lets a reader skip the
    /// second lookup instead of discovering emptiness.
    pub has_body: bool,
    /// `Server:` response header, lifted out of the header blob so the
    /// Services page can group on it without touching the bodies index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_header: Option<String>,
    /// Per-span app classification. The SQL backends instead sample a few
    /// bodies per endpoint at read time; see the crate docs for why the two
    /// can differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_comm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_exe: Option<String>,
}

/// Bodies and headers, one event per span.
///
/// `span_id` stays first because this event is posted as a pre-serialized
/// string (see [`raw_envelope`]): the bytes reach disk in this order, so
/// `span_id` can be extracted by a regex anchored at the start of the raw
/// event instead of scanning past a few hundred kilobytes of body.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct BodyEvent {
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<String>,
}

fn err_class_of(status: Option<u16>) -> &'static str {
    match status {
        Some(429) => "429",
        Some(s) if s >= 500 => "5xx",
        Some(s) if s >= 400 => "4xx",
        _ => "ok",
    }
}

/// Split one call into its metadata event and (optionally) its body event.
pub(crate) fn span_events(call: &LlmCall, store_bodies: bool) -> (SpanEvent, Option<BodyEvent>) {
    let status = call.status_code;
    let has_body = store_bodies
        && (call.request_body.is_some()
            || call.response_body.is_some()
            || !call.request_headers.is_empty()
            || !call.response_headers.is_empty());

    let request_headers = (!call.request_headers.is_empty())
        .then(|| h_storage::convert::headers_to_json(&call.request_headers));
    let response_headers = (!call.response_headers.is_empty())
        .then(|| h_storage::convert::headers_to_json(&call.response_headers));
    let server_header = h_storage::classify::extract_server_header(response_headers.as_deref());
    let app_hint = h_storage::classify::classify_app(
        server_header.as_deref(),
        response_headers.as_deref(),
        request_headers.as_deref(),
        std::slice::from_ref(&call.request_path),
        call.finish_reason.as_slice(),
        std::slice::from_ref(&call.model),
        call.request_body.as_deref(),
        call.response_body.as_deref(),
    );

    let meta = SpanEvent {
        id: call.id.clone(),
        source_id: call.source_id.clone(),
        ts_us: call.request_time,
        resp_us: call.response_time,
        done_us: call.complete_time,
        wire_api: call.wire_api.to_string(),
        model: call.model.clone(),
        api_type: call.api_type.to_string(),
        is_stream: call.is_stream,
        strm: u8::from(call.is_stream),
        request_path: call.request_path.clone(),
        status_code: status,
        err: u8::from(status.is_some_and(|s| s >= 400)),
        err_class: err_class_of(status).to_string(),
        finish_reason: call.finish_reason.clone(),
        input_tokens: call.input_tokens,
        output_tokens: call.output_tokens,
        total_tokens: call.total_tokens,
        tokens_estimated: h_storage::convert::derive_tokens_estimated(
            call.input_tokens,
            call.output_tokens,
            call.response_body.as_deref(),
        ),
        cache_read_input_tokens: call.cache_read_input_tokens,
        cache_creation_input_tokens: call.cache_creation_input_tokens,
        ttft_ms: call.ttft_ms,
        e2e_latency_ms: call.e2e_latency_ms,
        client_ip: call.client_ip.to_string(),
        client_port: call.client_port,
        server_ip: call.server_ip.to_string(),
        server_port: call.server_port,
        response_id: call.response_id.clone(),
        is_agent_request: call.is_agent_request,
        tool_surface: call.tool_surface.map(|t| t.to_string()),
        agent_topology: call.agent_topology.map(|t| t.to_string()),
        tool_call_count: call.tool_call_count,
        tool_names_json: serde_json::to_string(&call.tool_names).unwrap_or_else(|_| "[]".into()),
        body_bytes_dropped: call.body_bytes_dropped,
        has_body,
        server_header,
        app_hint,
        process_pid: call.process.as_ref().map(|p| p.pid),
        process_comm: call.process.as_ref().map(|p| p.comm.clone()),
        process_exe: call.process.as_ref().and_then(|p| p.exe.clone()),
    };

    let body = has_body.then(|| BodyEvent {
        span_id: call.id.clone(),
        request_body: call.request_body.clone(),
        response_body: call.response_body.clone(),
        request_headers,
        response_headers,
    });

    (meta, body)
}

// ---------------------------------------------------------------------------
// traces
// ---------------------------------------------------------------------------

/// One agent turn.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct TraceEvent {
    pub turn_id: String,
    pub source_id: String,
    pub session_id: String,
    pub wire_api: String,
    pub agent_kind: String,
    pub client_ip: String,
    pub server_ip: String,
    /// Authoritative window, microseconds. `_time` carries `start_us`.
    pub ts_us: i64,
    pub end_us: i64,
    pub duration_ms: u64,
    pub call_count: u32,
    /// Native array so `models_used="gpt-4"` matches. Order is not guaranteed
    /// to survive as a multivalue field, which is why the `_json` twin below
    /// is what gets read back.
    pub models_used: Vec<String>,
    pub models_used_json: String,
    pub subagents_used_json: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_answer_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_call_id: Option<String>,
    pub span_ids_json: String,
    /// The topology query needs exactly one span per turn. Lifting it out
    /// avoids pulling back a `span_ids_json` that can run to tens of KiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_span_id: Option<String>,
    pub metadata_json: String,
    /// Flattened out of `metadata.proxy` so filtering and topology never have
    /// to parse the blob. Both stay `None` while trace patching is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_pair_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_peer_turn_id: Option<String>,
    /// Every other member of the proxy group. Absent on direct turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_peer_turn_ids_json: Option<String>,
    /// 1 when the pair sweeper folded this turn into a peer, i.e. the traces
    /// list hides it unless `include_proxy_hops` is set. Precomputed because
    /// the predicate it replaces is a negation (`role NOT IN (…)`), and aglake
    /// cannot push those down — nor can it distinguish "role is absent" from
    /// "role is something else" without one.
    pub proxy_hidden: u8,
    pub tool_surfaces_json: String,
    pub tool_call_total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_topology: Option<String>,
    pub suspicious_skills_json: String,
}

/// Pull the `metadata.proxy` block up to top level.
struct Proxy {
    role: Option<String>,
    pair_id: Option<String>,
    peer_turn_id: Option<String>,
    peer_turn_ids: Option<Vec<String>>,
}

fn proxy_fields(metadata: &serde_json::Value) -> Proxy {
    let Some(p) = metadata.get("proxy") else {
        return Proxy {
            role: None,
            pair_id: None,
            peer_turn_id: None,
            peer_turn_ids: None,
        };
    };
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Proxy {
        role: s("role"),
        pair_id: s("pair_id"),
        peer_turn_id: s("peer_turn_id"),
        peer_turn_ids: p.get("peer_turn_ids").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
    }
}

pub(crate) fn trace_event(t: &Trace) -> TraceEvent {
    let proxy = proxy_fields(&t.metadata);
    let tool_surfaces: Vec<String> = t.tool_surfaces.iter().map(|s| s.to_string()).collect();
    TraceEvent {
        turn_id: t.turn_id.clone(),
        source_id: t.source_id.clone(),
        session_id: t.session_id.clone(),
        wire_api: t.wire_api.clone(),
        agent_kind: t.agent_kind.clone(),
        client_ip: t.client_ip.to_string(),
        server_ip: t.server_ip.to_string(),
        ts_us: t.start_time_us,
        end_us: t.end_time_us,
        duration_ms: t.duration_ms,
        call_count: t.call_count,
        models_used: t.models_used.clone(),
        models_used_json: json_list(&t.models_used),
        subagents_used_json: json_list(&t.subagents_used),
        total_input_tokens: t.total_input_tokens,
        total_output_tokens: t.total_output_tokens,
        total_cache_read_input_tokens: t.total_cache_read_input_tokens,
        total_cache_creation_input_tokens: t.total_cache_creation_input_tokens,
        total_cost_usd: t.total_cost_usd,
        status: t.status.to_string(),
        final_finish_reason: t.final_finish_reason.clone(),
        user_input_preview: t.user_input_preview.clone(),
        user_call_id: t.user_call_id.clone(),
        final_answer_preview: t.final_answer_preview.clone(),
        final_call_id: t.final_call_id.clone(),
        span_ids_json: json_list(&t.span_ids),
        first_span_id: t.span_ids.first().cloned(),
        metadata_json: t.metadata.to_string(),
        proxy_hidden: u8::from(matches!(
            proxy.role.as_deref(),
            Some("proxy_out") | Some("mirror_secondary")
        )),
        proxy_role: proxy.role,
        proxy_pair_id: proxy.pair_id,
        proxy_peer_turn_id: proxy.peer_turn_id,
        proxy_peer_turn_ids_json: proxy.peer_turn_ids.as_deref().map(json_list),
        tool_surfaces_json: json_list(&tool_surfaces),
        tool_call_total: t.tool_call_total,
        agent_topology: t.agent_topology.map(|a| a.to_string()),
        suspicious_skills_json: serde_json::to_string(&t.suspicious_skills)
            .unwrap_or_else(|_| "[]".into()),
    }
}

fn json_list(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

/// Which of the aggregator's four materialized rollup tiers a row belongs to.
///
/// The aggregator marks rolled-up dimensions with a literal `'*'`, which SPL
/// reads as a wildcard that matches everything — so a translated
/// `server_ip = '*'` would select the detail rows *and* the rollup row and
/// double every metric, silently. Classifying the tier at write time replaces
/// that whole class of query with an exact-match term.
fn dim_tier(wire_api: &str, model: &str, server_ip: &str) -> &'static str {
    match (wire_api == "*", model == "*", server_ip == "*") {
        (false, false, false) => "wms",
        (false, false, true) => "wm",
        (true, true, false) => "s",
        (true, true, true) => "all",
        // The aggregator materializes only the four combinations above. Anything
        // else would be a new tier, and guessing which query should see it is
        // worse than leaving it visible under a name no tier selector matches.
        _ => "other",
    }
}

/// The wide per-window metrics row.
///
/// Field names track [`LlmMetric`] one-for-one (only `timestamp_us` is renamed
/// to the `ts_us` used everywhere else) so there is no translation table that
/// could drift.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct MetricEvent {
    /// Minted once per batch and reused across retries, so a resend that
    /// duplicates a row is detectable (and dedupable) rather than silently
    /// doubling a SUM.
    pub row_id: String,
    pub ts_us: i64,
    pub source_id: String,
    pub wire_api: String,
    pub model: String,
    pub server_ip: String,
    pub dim_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_surface: Option<String>,

    pub call_count: u64,
    pub stream_count: u64,
    pub non_stream_count: u64,
    pub active_calls_sum: u64,
    pub active_calls_sample_count: u64,
    pub active_calls_max: u32,

    pub total_input_tokens: u64,
    pub input_token_count: u64,
    pub total_output_tokens: u64,
    pub output_token_count: u64,
    pub total_cache_read_input_tokens: u64,
    pub total_cache_creation_input_tokens: u64,

    pub error_count: u64,
    pub error_4xx_count: u64,
    pub error_429_count: u64,
    pub error_5xx_count: u64,

    pub ttft_sum: f64,
    pub ttft_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_p95: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_p99: Option<f64>,

    pub ttft_stream_sum: f64,
    pub ttft_stream_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_stream_p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_stream_p95: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_stream_p99: Option<f64>,

    pub ttft_nonstream_sum: f64,
    pub ttft_nonstream_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_nonstream_p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_nonstream_p95: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_nonstream_p99: Option<f64>,

    pub e2e_sum: f64,
    pub e2e_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2e_p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2e_p95: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2e_p99: Option<f64>,

    pub tpot_sum: f64,
    pub tpot_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpot_p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpot_p95: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpot_p99: Option<f64>,
}

pub(crate) fn metric_event(m: &LlmMetric, row_id: String) -> MetricEvent {
    MetricEvent {
        row_id,
        ts_us: m.timestamp_us,
        source_id: m.source_id.clone(),
        wire_api: m.wire_api.clone(),
        model: m.model.clone(),
        server_ip: m.server_ip.clone(),
        dim_tier: dim_tier(&m.wire_api, &m.model, &m.server_ip).to_string(),
        tool_surface: m.tool_surface.clone(),

        call_count: m.call_count,
        stream_count: m.stream_count,
        non_stream_count: m.non_stream_count,
        active_calls_sum: m.active_calls_sum,
        active_calls_sample_count: m.active_calls_sample_count,
        active_calls_max: m.active_calls_max,

        total_input_tokens: m.total_input_tokens,
        input_token_count: m.input_token_count,
        total_output_tokens: m.total_output_tokens,
        output_token_count: m.output_token_count,
        total_cache_read_input_tokens: m.total_cache_read_input_tokens,
        total_cache_creation_input_tokens: m.total_cache_creation_input_tokens,

        error_count: m.error_count,
        error_4xx_count: m.error_4xx_count,
        error_429_count: m.error_429_count,
        error_5xx_count: m.error_5xx_count,

        ttft_sum: m.ttft_sum,
        ttft_count: m.ttft_count,
        ttft_p50: m.ttft_p50,
        ttft_p95: m.ttft_p95,
        ttft_p99: m.ttft_p99,

        ttft_stream_sum: m.ttft_stream_sum,
        ttft_stream_count: m.ttft_stream_count,
        ttft_stream_p50: m.ttft_stream_p50,
        ttft_stream_p95: m.ttft_stream_p95,
        ttft_stream_p99: m.ttft_stream_p99,

        ttft_nonstream_sum: m.ttft_nonstream_sum,
        ttft_nonstream_count: m.ttft_nonstream_count,
        ttft_nonstream_p50: m.ttft_nonstream_p50,
        ttft_nonstream_p95: m.ttft_nonstream_p95,
        ttft_nonstream_p99: m.ttft_nonstream_p99,

        e2e_sum: m.e2e_sum,
        e2e_count: m.e2e_count,
        e2e_p50: m.e2e_p50,
        e2e_p95: m.e2e_p95,
        e2e_p99: m.e2e_p99,

        tpot_sum: m.tpot_sum,
        tpot_count: m.tpot_count,
        tpot_p50: m.tpot_p50,
        tpot_p95: m.tpot_p95,
        tpot_p99: m.tpot_p99,
    }
}

/// One `(window, dimensions, finish_reason)` count. Kept in its own index
/// rather than sharing with [`MetricEvent`]: the columnar read path is only
/// available when every sourcetype in a bucket indexes the field being read,
/// so two shapes in one index would push both onto the row path.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct FinishEvent {
    pub row_id: String,
    pub ts_us: i64,
    pub source_id: String,
    pub wire_api: String,
    pub model: String,
    pub server_ip: String,
    pub dim_tier: String,
    pub finish_reason: String,
    pub count: u64,
}

pub(crate) fn finish_event(m: &LlmFinishMetric, row_id: String) -> FinishEvent {
    FinishEvent {
        row_id,
        ts_us: m.timestamp_us,
        source_id: m.source_id.clone(),
        wire_api: m.wire_api.clone(),
        model: m.model.clone(),
        server_ip: m.server_ip.clone(),
        dim_tier: dim_tier(&m.wire_api, &m.model, &m.server_ip).to_string(),
        finish_reason: m.finish_reason.clone(),
        count: m.count,
    }
}

// ---------------------------------------------------------------------------
// http exchanges
// ---------------------------------------------------------------------------

/// Raw HTTP exchange metadata. Same metadata/body split as spans.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct HttpEvent {
    pub id: String,
    pub source_id: String,
    pub ts_us: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_byte_us: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_us: Option<i64>,
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    pub method: String,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub err: u8,
    pub err_class: String,
    pub is_sse: bool,
    /// 0/1 twin of `is_sse`, for `sum(sse)`.
    pub sse: u8,
    pub sse_event_count: u32,
    pub sse_data_bytes: u64,
    pub has_body: bool,
}

/// Headers are always written (they are non-optional on the detail type), so
/// an exchange body event exists whenever bodies are being stored at all.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct HttpBodyEvent {
    pub span_id: String,
    pub request_headers: String,
    pub response_headers: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

pub(crate) fn http_events(
    x: &HttpExchange,
    store_bodies: bool,
) -> (HttpEvent, Option<HttpBodyEvent>) {
    let (client_ip, client_port) = x.client_addr();
    let (server_ip, server_port) = x.server_addr();
    let is_sse = x.is_sse();
    let status = x.response.status;

    let meta = HttpEvent {
        id: x.id.clone(),
        source_id: x.request.flow_key.source_id.clone(),
        ts_us: x.request.timestamp_us,
        first_byte_us: Some(x.response.first_byte_timestamp_us),
        done_us: Some(x.response.complete_timestamp_us),
        client_ip: client_ip.to_string(),
        client_port,
        server_ip: server_ip.to_string(),
        server_port,
        method: x.request.method.clone(),
        uri: x.request.uri.clone(),
        status: Some(status),
        err: u8::from(status >= 400),
        err_class: err_class_of(Some(status)).to_string(),
        is_sse,
        sse: u8::from(is_sse),
        sse_event_count: x.sse_event_count,
        sse_data_bytes: x.sse_data_bytes,
        has_body: store_bodies,
    };

    let body = store_bodies.then(|| HttpBodyEvent {
        span_id: x.id.clone(),
        request_headers: h_storage::convert::headers_to_json(&x.request.headers),
        response_headers: h_storage::convert::headers_to_json(&x.response.headers),
        // Post-TLS LLM traffic is plaintext JSON/SSE; a binary body would be
        // stored lossily here, exactly as it is on the SQL backends.
        request_body: (!x.request.body.is_empty())
            .then(|| String::from_utf8_lossy(&x.request.body).into_owned()),
        response_body: x
            .stored_response_body()
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned()),
    });

    (meta, body)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use h_common::agent::{AgentTopology, ToolSurface};
    use h_common::process::ProcessInfo;
    use h_llm::model::ApiType;
    use h_turn::TraceStatus;
    use std::net::{IpAddr, Ipv4Addr};

    /// A fully-populated call. Every `Option` is `Some` and every collection is
    /// non-empty, so encoding covers the widest event shape.
    pub(crate) fn full_call() -> LlmCall {
        LlmCall {
            source_id: "src-0".into(),
            id: "019fc053-5fa2-7770-98aa-dd2014c18a51".into(),
            wire_api: "openai-chat",
            model: "gpt-4".into(),
            api_type: ApiType::Chat,
            request_time: 1_785_638_114_914_200,
            response_time: Some(1_785_638_115_014_200),
            complete_time: Some(1_785_638_115_914_200),
            request_path: "/v1/chat/completions".into(),
            is_stream: true,
            request_body: Some(r#"{"model":"gpt-4"}"#.into()),
            status_code: Some(200),
            finish_reason: Some("stop".into()),
            response_body: Some(r#"{"id":"chatcmpl-x"}"#.into()),
            input_tokens: Some(1234),
            output_tokens: Some(56),
            total_tokens: Some(1290),
            cache_read_input_tokens: Some(7),
            cache_creation_input_tokens: Some(8),
            ttft_ms: Some(123.4),
            e2e_latency_ms: Some(890.1),
            client_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)),
            client_port: 51234,
            server_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            server_port: 8000,
            response_id: Some("chatcmpl-x".into()),
            request_headers: vec![("host".into(), "api.example".into())],
            response_headers: vec![("server".into(), "uvicorn".into())],
            is_agent_request: true,
            tool_surface: Some(ToolSurface::FunctionCall),
            agent_topology: Some(AgentTopology::SingleAgent),
            tool_call_count: 2,
            tool_names: vec!["Bash".into(), "Read".into()],
            body_bytes_dropped: 0,
            process: Some(ProcessInfo {
                pid: 4242,
                comm: "node".into(),
                exe: Some("/usr/bin/node".into()),
            }),
        }
    }

    /// The opposite extreme: every `Option` is `None`, every collection empty.
    /// This is the shape that exposes null-vs-absent bugs.
    pub(crate) fn minimal_call() -> LlmCall {
        LlmCall {
            response_time: None,
            complete_time: None,
            request_body: None,
            status_code: None,
            finish_reason: None,
            response_body: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            response_id: None,
            request_headers: vec![],
            response_headers: vec![],
            tool_surface: None,
            agent_topology: None,
            tool_names: vec![],
            process: None,
            ..full_call()
        }
    }

    pub(crate) fn full_trace() -> Trace {
        Trace {
            source_id: "src-0".into(),
            turn_id: "019fc053-5fa2-7770-98aa-dd2014c18a52".into(),
            session_id: "sess-1".into(),
            wire_api: "anthropic".into(),
            agent_kind: "claude-cli".into(),
            client_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)),
            server_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            start_time_us: 1_785_638_114_914_200,
            end_time_us: 1_785_638_120_914_200,
            duration_ms: 6000,
            call_count: 2,
            models_used: vec!["claude-sonnet".into()],
            subagents_used: vec!["explore".into()],
            total_input_tokens: 1000,
            total_output_tokens: 200,
            total_cache_read_input_tokens: 10,
            total_cache_creation_input_tokens: 20,
            total_cost_usd: Some(0.0123),
            status: TraceStatus::Complete,
            final_finish_reason: Some("end_turn".into()),
            user_input_preview: Some("hello".into()),
            user_call_id: Some("call-a".into()),
            final_answer_preview: Some("hi".into()),
            final_call_id: Some("call-b".into()),
            span_ids: vec!["call-a".into(), "call-b".into()],
            metadata: serde_json::json!({
                "proxy": { "role": "outer", "pair_id": "group-9", "peer_turn_id": "t-peer" }
            }),
            tool_surfaces: vec![ToolSurface::FunctionCall],
            tool_call_total: 3,
            agent_topology: Some(AgentTopology::SingleAgent),
            suspicious_skills: vec![],
        }
    }

    /// A metric on the finest `(W, M, S)` tier.
    pub(crate) fn sample_metric() -> LlmMetric {
        LlmMetric {
            timestamp_us: 1_785_638_110_000_000,
            source_id: "src-0".into(),
            granularity: "10s",
            wire_api: "openai-chat".into(),
            model: "gpt-4".into(),
            server_ip: "10.0.0.1".into(),
            call_count: 10,
            stream_count: 6,
            non_stream_count: 4,
            active_calls_sum: 30,
            active_calls_sample_count: 10,
            active_calls_max: 5,
            total_input_tokens: 1000,
            input_token_count: 10,
            total_output_tokens: 500,
            output_token_count: 10,
            total_cache_read_input_tokens: 7,
            total_cache_creation_input_tokens: 8,
            error_count: 2,
            error_4xx_count: 1,
            error_429_count: 0,
            error_5xx_count: 1,
            ttft_sum: 1234.5,
            ttft_count: 10,
            ttft_p50: Some(100.0),
            ttft_p95: Some(200.0),
            ttft_p99: Some(300.0),
            ttft_stream_sum: 700.0,
            ttft_stream_count: 6,
            ttft_stream_p50: Some(110.0),
            ttft_stream_p95: None,
            ttft_stream_p99: None,
            ttft_nonstream_sum: 534.5,
            ttft_nonstream_count: 4,
            ttft_nonstream_p50: None,
            ttft_nonstream_p95: None,
            ttft_nonstream_p99: None,
            e2e_sum: 9000.0,
            e2e_count: 10,
            e2e_p50: Some(800.0),
            e2e_p95: Some(1500.0),
            e2e_p99: None,
            tpot_sum: 60.0,
            tpot_count: 6,
            tpot_p50: Some(10.0),
            tpot_p95: None,
            tpot_p99: None,
            tool_surface: Some("function_call".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Emit real encoder output for the live round-trip check against aglake.
    /// Writes only when `AGLAKE_EMIT` names a path, so a normal `cargo test`
    /// run touches nothing.
    #[test]
    fn emit_sample_events() {
        let Ok(path) = std::env::var("AGLAKE_EMIT") else {
            return;
        };
        let mut out = String::new();
        for call in [fixtures::full_call(), fixtures::minimal_call()] {
            let (meta, body) = span_events(&call, true);
            out.push_str(
                &Envelope::new(
                    call.request_time,
                    &call.source_id,
                    ST_SPAN,
                    "heron_spans",
                    meta,
                )
                .encode()
                .unwrap(),
            );
            out.push('\n');
            if let Some(body) = body {
                out.push_str(
                    &raw_envelope(
                        call.request_time,
                        &call.source_id,
                        ST_BODY,
                        "heron_bodies",
                        &body,
                    )
                    .unwrap(),
                );
                out.push('\n');
            }
        }
        let t = fixtures::full_trace();
        out.push_str(
            &Envelope::new(
                t.start_time_us,
                &t.source_id,
                ST_TRACE,
                "heron_traces",
                trace_event(&t),
            )
            .encode()
            .unwrap(),
        );
        out.push('\n');
        let m = fixtures::sample_metric();
        out.push_str(
            &Envelope::new(
                m.timestamp_us,
                &m.source_id,
                ST_METRIC,
                "heron_metrics_10s",
                metric_event(&m, "row-1".into()),
            )
            .encode()
            .unwrap(),
        );
        out.push('\n');
        std::fs::write(&path, out).expect("write sample");
    }

    /// The minimal call must not emit a single `null`, and must still carry
    /// every non-optional field.
    #[test]
    fn minimal_call_omits_all_optionals() {
        let (meta, body) = span_events(&fixtures::minimal_call(), true);
        let s = serde_json::to_string(&meta).unwrap();
        assert!(!s.contains("null"), "{s}");
        assert!(!s.contains("status_code"), "{s}");
        assert!(!s.contains("ttft_ms"), "{s}");
        // Non-optional fields survive, including the precomputed ones.
        assert!(s.contains(r#""err_class":"ok""#), "{s}");
        assert!(s.contains(r#""tool_names_json":"[]""#), "{s}");
        assert!(s.contains(r#""has_body":false"#), "{s}");
        // No bodies and no headers ⇒ no body event at all.
        assert!(body.is_none());
    }

    #[test]
    fn full_call_round_trips_every_field() {
        let call = fixtures::full_call();
        let (meta, body) = span_events(&call, true);
        let s = serde_json::to_string(&meta).unwrap();
        // ts_us is the authoritative timestamp and must stay an integer.
        assert!(s.contains(r#""ts_us":1785638114914200"#), "{s}");
        assert!(s.contains(r#""strm":1"#), "{s}");
        assert!(s.contains(r#""err":0"#), "{s}");
        assert!(s.contains(r#""tool_surface":"function_call""#), "{s}");
        assert!(s.contains(r#""agent_topology":"single_agent""#), "{s}");
        assert!(
            s.contains(r#""tool_names_json":"[\"Bash\",\"Read\"]""#),
            "{s}"
        );
        assert!(s.contains(r#""process_pid":4242"#), "{s}");
        assert!(s.contains(r#""has_body":true"#), "{s}");
        // `Server: uvicorn` is lifted out of the header blob at write time.
        assert!(s.contains(r#""server_header":"uvicorn""#), "{s}");

        let b = serde_json::to_string(&body.unwrap()).unwrap();
        assert!(b.starts_with(r#"{"span_id":"019fc053"#), "{b}");
        assert!(b.contains("request_headers"), "{b}");
    }

    /// Everything read back comes from the event JSON, so every event type has
    /// to survive a serialize → deserialize round trip with its values intact.
    #[test]
    fn span_event_round_trips_through_json() {
        let call = fixtures::full_call();
        let (meta, _) = span_events(&call, true);
        let s = serde_json::to_string(&meta).unwrap();
        let back: SpanEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.ts_us, call.request_time);
        assert_eq!(back.done_us, call.complete_time);
        assert_eq!(back.model, call.model);
        assert_eq!(back.status_code, Some(200));
        assert_eq!(back.ttft_ms, Some(123.4));
        assert_eq!(back.process_exe.as_deref(), Some("/usr/bin/node"));
        assert!(back.is_agent_request);

        let (meta, _) = span_events(&fixtures::minimal_call(), true);
        let back: SpanEvent = serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();
        assert_eq!(back.status_code, None);
        assert_eq!(back.ttft_ms, None);
        assert_eq!(back.process_pid, None);
    }

    /// The shape that would break every read at once: a field added to the
    /// struct after events were already on disk. aglake never backfills, so a
    /// missing key has to decode as a default rather than fail the row.
    #[test]
    fn events_tolerate_missing_fields() {
        let back: SpanEvent = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(back.id, "x");
        assert_eq!(back.ts_us, 0);
        assert!(!back.has_body);

        let back: TraceEvent = serde_json::from_str(r#"{"turn_id":"t"}"#).unwrap();
        assert_eq!(back.turn_id, "t");
        assert_eq!(back.call_count, 0);

        let back: MetricEvent = serde_json::from_str(r#"{"ts_us":7}"#).unwrap();
        assert_eq!(back.ts_us, 7);
        assert_eq!(back.call_count, 0);

        let back: HttpEvent = serde_json::from_str("{}").unwrap();
        assert_eq!(back.id, "");
    }

    /// `store_bodies = false` must suppress the body event entirely, and say so
    /// on the metadata event so readers skip the second lookup.
    #[test]
    fn store_bodies_false_suppresses_body_event() {
        let (meta, body) = span_events(&fixtures::full_call(), false);
        assert!(body.is_none());
        assert!(!meta.has_body);
    }

    #[test]
    fn err_class_covers_every_status_band() {
        assert_eq!(err_class_of(None), "ok");
        assert_eq!(err_class_of(Some(200)), "ok");
        assert_eq!(err_class_of(Some(399)), "ok");
        assert_eq!(err_class_of(Some(400)), "4xx");
        assert_eq!(err_class_of(Some(404)), "4xx");
        // 429 is its own bucket and must win over the 4xx arm.
        assert_eq!(err_class_of(Some(429)), "429");
        assert_eq!(err_class_of(Some(499)), "4xx");
        assert_eq!(err_class_of(Some(500)), "5xx");
        assert_eq!(err_class_of(Some(503)), "5xx");
    }

    /// The four tiers the aggregator actually materializes, plus the shape
    /// that must never be silently folded into one of them.
    #[test]
    fn dim_tier_maps_the_materialized_tiers() {
        assert_eq!(dim_tier("openai-chat", "gpt-4", "10.0.0.1"), "wms");
        assert_eq!(dim_tier("openai-chat", "gpt-4", "*"), "wm");
        assert_eq!(dim_tier("*", "*", "10.0.0.1"), "s");
        assert_eq!(dim_tier("*", "*", "*"), "all");
        // Not materialized by the aggregator — must not masquerade as a tier.
        assert_eq!(dim_tier("openai-chat", "*", "*"), "other");
        assert_eq!(dim_tier("*", "gpt-4", "10.0.0.1"), "other");
    }

    /// A rollup row's `'*'` must reach disk as a literal, never as something a
    /// query could confuse with a wildcard match.
    #[test]
    fn rollup_sentinels_are_written_literally_and_tagged() {
        let mut m = fixtures::sample_metric();
        m.wire_api = "*".into();
        m.model = "*".into();
        m.server_ip = "*".into();
        let s = serde_json::to_string(&metric_event(&m, "r".into())).unwrap();
        assert!(s.contains(r#""wire_api":"*""#), "{s}");
        assert!(s.contains(r#""dim_tier":"all""#), "{s}");
    }

    #[test]
    fn trace_event_flattens_proxy_metadata_and_first_span() {
        let e = trace_event(&fixtures::full_trace());
        assert_eq!(e.proxy_role.as_deref(), Some("outer"));
        assert_eq!(e.proxy_pair_id.as_deref(), Some("group-9"));
        assert_eq!(e.proxy_peer_turn_id.as_deref(), Some("t-peer"));
        assert_eq!(e.first_span_id.as_deref(), Some("call-a"));
        assert_eq!(e.span_ids_json, r#"["call-a","call-b"]"#);
        assert_eq!(e.ts_us, 1_785_638_114_914_200);
        assert_eq!(e.status, "complete");

        // Absent proxy metadata must read as None, not as an empty string.
        let mut t = fixtures::full_trace();
        t.metadata = serde_json::json!({});
        let e = trace_event(&t);
        assert_eq!(e.proxy_role, None);
        assert_eq!(e.proxy_pair_id, None);
    }

    #[test]
    fn trace_event_round_trips_through_json() {
        let e = trace_event(&fixtures::full_trace());
        let back: TraceEvent = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.ts_us, 1_785_638_114_914_200);
        assert_eq!(back.end_us, 1_785_638_120_914_200);
        assert_eq!(back.total_cost_usd, Some(0.0123));
        assert_eq!(back.models_used, vec!["claude-sonnet".to_string()]);
        assert_eq!(back.tool_call_total, 3);
    }

    /// The metric row is the one place a lost or doubled value silently
    /// corrupts a chart, so every numeric field has to survive verbatim.
    #[test]
    fn metric_event_round_trips_through_json() {
        let m = fixtures::sample_metric();
        let e = metric_event(&m, "row-1".into());
        let back: MetricEvent = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.row_id, "row-1");
        assert_eq!(back.ts_us, m.timestamp_us);
        assert_eq!(back.call_count, 10);
        assert_eq!(back.ttft_sum, 1234.5);
        assert_eq!(back.ttft_p95, Some(200.0));
        // A `None` percentile must stay None, not become 0.0.
        assert_eq!(back.ttft_stream_p95, None);
        assert_eq!(back.e2e_p99, None);
        assert_eq!(back.dim_tier, "wms");
        assert_eq!(back.tool_surface.as_deref(), Some("function_call"));
    }

    #[test]
    fn finish_event_carries_its_tier() {
        let f = LlmFinishMetric {
            timestamp_us: 1_785_638_110_000_000,
            source_id: "src-0".into(),
            granularity: "10s".into(),
            wire_api: "*".into(),
            model: "*".into(),
            server_ip: "*".into(),
            finish_reason: "end_turn".into(),
            count: 4,
        };
        let e = finish_event(&f, "r".into());
        assert_eq!(e.dim_tier, "all");
        let back: FinishEvent = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.count, 4);
        assert_eq!(back.finish_reason, "end_turn");
    }

    /// A body event has to reach disk byte-for-byte, so it is posted as a
    /// string rather than an object — aglake sorts object keys on ingest.
    #[test]
    fn raw_envelope_posts_the_payload_as_a_string() {
        let b = BodyEvent {
            span_id: "abc-123".into(),
            request_body: Some(r#"{"a":1}"#.into()),
            response_body: None,
            request_headers: None,
            response_headers: None,
        };
        let s = raw_envelope(1_785_638_114_914_200, "h", ST_BODY, "heron_bodies", &b).unwrap();
        // The event is a JSON string whose contents start with span_id.
        assert!(
            s.contains(r#""event":"{\"span_id\":\"abc-123\""#),
            "body must be posted as a pre-serialized string, span_id first: {s}"
        );
        // And it must survive the extra layer of escaping intact.
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let inner = v["event"].as_str().unwrap();
        let back: BodyEvent = serde_json::from_str(inner).unwrap();
        assert_eq!(back.span_id, "abc-123");
        assert_eq!(back.request_body.as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(back.response_body, None);
    }

    #[test]
    fn none_fields_are_omitted_not_nulled() {
        let b = BodyEvent {
            span_id: "x".into(),
            request_body: None,
            response_body: None,
            request_headers: None,
            response_headers: None,
        };
        let s = serde_json::to_string(&b).unwrap();
        assert_eq!(s, r#"{"span_id":"x"}"#);
        assert!(!s.contains("null"));
    }

    #[test]
    fn envelope_time_keeps_microsecond_precision() {
        let e = Envelope::new(1_785_638_114_914_200, "src-0", ST_SPAN, "heron_spans", ());
        let s = e.encode().unwrap();
        assert!(s.contains(r#""time":"1785638114.914200""#), "{s}");
        assert!(s.contains(r#""sourcetype":"heron_span""#), "{s}");
        assert!(s.contains(r#""host":"src-0""#), "{s}");
    }
}
