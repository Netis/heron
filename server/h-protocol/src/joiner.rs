//! HTTP request/response pairing. Consumes `HttpParseEvent`s (per-direction
//! HTTP/SSE parser output) and produces `HttpJoinerEvent`s — a two-phase
//! stream that decouples request observation (for downstream concurrency
//! tracking) from exchange completion (for storage / semantic extraction).
//!
//! The joiner is wire-API-agnostic: it pairs HTTP exchanges regardless of
//! whether they look like LLM traffic. Non-LLM traffic still lands here,
//! which is the whole point — see proposal 0001.
//!
//! `HttpExchange` is a thin carrier of `(id, Arc<HttpRequestData>,
//! Arc<HttpResponseData>)` — the joiner never reshapes fields into a flat
//! storage struct. Downstream consumers read transport fields off the two
//! Arcs directly.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use bytes::Bytes;
use tracing::warn;
use uuid::Uuid;

use h_common::internal_metrics::{Metric, MetricsWorker};

use crate::model::{HttpParseEvent, HttpRequestData, HttpResponseData, SseEventData};
use crate::net::FlowKey;

/// Upper bound on how long a pending HTTP exchange may sit **silent** before
/// heartbeats evict it. The clock is reset by any SSE activity on the flow
/// (see `Pending.last_activity_us`), so a live streaming response keeps
/// refreshing itself regardless of total duration. Eviction therefore means
/// "nothing at all for 10 minutes" — almost always a dead connection
/// (abort, capture gap, TLS renegotiation).
///
/// 10 minutes is chosen to comfortably exceed realistic non-streaming TTFT
/// on slow provider paths (Anthropic/OpenAI SDK client defaults sit at the
/// same order). It is decoupled from `TurnConfig.idle_timeout_secs`, which
/// answers a different question (when is an agent *conversation* done).
pub const PENDING_STALE_TIMEOUT_US: i64 = 600_000_000;

/// Paired HTTP exchange carried as two `Arc`s over the original request /
/// response records. Authoritative transport-layer record; persisted to
/// `http_exchanges`.
///
/// `id` is a UUIDv7 minted by the joiner at pairing time — the primary key
/// for `http_exchanges` and the stable correlation id downstream (e.g.
/// `LlmCall.exchange_id` FK).
///
/// SSE events are NOT a field here: the reconstructed content stream does
/// not land in storage. SSE events travel alongside on the in-flight
/// `HttpJoinerEvent::Exchange` payload for downstream semantic extraction
/// and are dropped at the end of that hop.
#[derive(Debug, Clone)]
pub struct HttpExchange {
    pub id: String,
    pub request: Arc<HttpRequestData>,
    pub response: Arc<HttpResponseData>,
    /// Number of SSE events observed on this exchange. `0` for non-SSE.
    /// Sourced from `HttpJoinerEvent::Exchange.sse_events.len()` at the
    /// storage fan-out site (`stage.rs`).
    pub sse_event_count: u32,
    /// Sum of `data:` payload bytes across all SSE events on this exchange.
    /// Frame overhead (`event:`/`data:`/blank-line separators) is excluded —
    /// raw SSE wire bytes are discarded at parse time. `0` for non-SSE.
    pub sse_data_bytes: u64,
}

/// Compute `(count, data_bytes)` from a slice of SSE events.
/// `data_bytes` is the sum of each event's `data:` payload length.
pub fn sse_summary(events: &[SseEventData]) -> (u32, u64) {
    let count = events.len() as u32;
    let bytes = events.iter().map(|e| e.data.len() as u64).sum();
    (count, bytes)
}

impl HttpExchange {
    pub fn source_id(&self) -> &str {
        &self.request.flow_key.source_id
    }

    pub fn client_addr(&self) -> (IpAddr, u16) {
        self.request.client_addr
    }

    pub fn server_addr(&self) -> (IpAddr, u16) {
        self.request.server_addr
    }

    pub fn request_time_us(&self) -> i64 {
        self.request.timestamp_us
    }

    pub fn response_first_byte_time_us(&self) -> i64 {
        self.response.first_byte_timestamp_us
    }

    pub fn response_complete_time_us(&self) -> i64 {
        self.response.complete_timestamp_us
    }

    /// `true` iff the response's `Content-Type` starts with
    /// `text/event-stream`. Transport-layer decision; the authoritative
    /// signal for whether to persist a response body (see
    /// `stored_response_body`).
    pub fn is_sse(&self) -> bool {
        self.response
            .content_type()
            .map(|ct| ct.starts_with("text/event-stream"))
            .unwrap_or(false)
    }

    /// `None` for SSE — the parser emits `Bytes::new()` for SSE response
    /// bodies (raw bytes are never retained), and persisting that empty
    /// buffer would store a zero-length blob instead of the semantic NULL
    /// expected by storage. Callers writing the response body should use
    /// this accessor rather than `self.response.body` directly.
    pub fn stored_response_body(&self) -> Option<&Bytes> {
        if self.is_sse() {
            None
        } else {
            Some(&self.response.body)
        }
    }
}

/// Output event of the `HttpJoiner`. Two downstream-visible phases:
/// `Request` fires on request arrival (drives `LlmEvent::Start` and metrics
/// concurrency +1); `Exchange` fires on response completion with both Arcs
/// plus any SSE events seen on that flow.
#[derive(Debug, Clone)]
pub enum HttpJoinerEvent {
    /// HTTP request observed; exchange still in-flight. Downstream consumers
    /// (`LlmProcessor`) use this to run wire-API detection and emit
    /// concurrency-tracking `Start` events.
    Request(Arc<HttpRequestData>),

    /// Exchange paired (request + response + any SSE events). `sse_events`
    /// is non-empty iff the response was SSE-framed.
    Exchange {
        /// UUIDv7 minted at pairing time. Primary key for `http_exchanges`
        /// and correlation id for downstream records (e.g. LlmCall).
        id: String,
        request: Arc<HttpRequestData>,
        response: Arc<HttpResponseData>,
        sse_events: Vec<SseEventData>,
    },

    /// Time-advancing heartbeat. Forwarded from upstream `HttpParseEvent`.
    /// Downstream consumers (metrics, turn tracker) use these to close stale
    /// windows during traffic idle.
    Heartbeat { ts: i64, source_id: String },
}

/// Per-flow in-flight state.
struct Pending {
    request: Arc<HttpRequestData>,
    sse_events: Vec<SseEventData>,
    /// Last observed activity on this flow — request arrival or latest SSE
    /// event. Drives staleness eviction so that long-running streams aren't
    /// killed by the `PENDING_STALE_TIMEOUT_US` cap.
    last_activity_us: i64,
}

/// Pairs HTTP requests + responses across a single flow-worker shard.
pub struct HttpJoiner {
    pending: HashMap<FlowKey, Pending>,
    metrics: MetricsWorker,
}

impl HttpJoiner {
    pub fn new(metrics: MetricsWorker) -> Self {
        Self {
            pending: HashMap::new(),
            metrics,
        }
    }

    /// Process a single protocol event. Returns zero or more joiner events.
    pub fn process(&mut self, event: HttpParseEvent) -> Vec<HttpJoinerEvent> {
        match event {
            HttpParseEvent::HttpRequest(req) => self.on_request(req),
            HttpParseEvent::SseEvent(sse) => {
                self.on_sse(sse);
                Vec::new()
            }
            HttpParseEvent::HttpResponse(resp) => self.on_response(resp),
            HttpParseEvent::Heartbeat { ts, source_id } => {
                self.metrics.counter(Metric::JoinerHeartbeatsReceived).inc();
                self.cleanup_stale(&source_id, ts, PENDING_STALE_TIMEOUT_US);
                vec![HttpJoinerEvent::Heartbeat { ts, source_id }]
            }
        }
    }

    fn on_request(&mut self, req: HttpRequestData) -> Vec<HttpJoinerEvent> {
        let flow_key = req.flow_key.clone();
        let last_activity_us = req.timestamp_us;

        // Overwriting a pending on the same flow is only interesting if the
        // previous exchange is still "fresh" (had activity within the stale
        // window) — that suggests a genuine protocol anomaly (response lost,
        // pipelined requests we don't understand). If the previous entry has
        // been silent past the stale timeout, it's a long-dead flow being
        // recycled: replace silently.
        if let Some(prev) = self.pending.get(&flow_key) {
            let silence = last_activity_us - prev.last_activity_us;
            if silence > PENDING_STALE_TIMEOUT_US {
                self.pending.remove(&flow_key);
            } else {
                warn!(
                    flow = %flow_key,
                    silence_secs = silence as f64 / 1_000_000.0,
                    "overwriting pending HTTP exchange — previous request on this flow had no response"
                );
            }
        }

        let arc_req = Arc::new(req);
        self.pending.insert(
            flow_key,
            Pending {
                request: Arc::clone(&arc_req),
                sse_events: Vec::new(),
                last_activity_us,
            },
        );
        vec![HttpJoinerEvent::Request(arc_req)]
    }

    fn on_sse(&mut self, sse: SseEventData) {
        if let Some(pending) = self.pending.get_mut(&sse.flow_key) {
            // SSE events should arrive in order, but clamp to max defensively:
            // an out-of-order event must never roll the clock backwards.
            if sse.timestamp_us > pending.last_activity_us {
                pending.last_activity_us = sse.timestamp_us;
            }
            pending.sse_events.push(sse);
        }
    }

    fn on_response(&mut self, resp: HttpResponseData) -> Vec<HttpJoinerEvent> {
        let pending = match self.pending.remove(&resp.flow_key) {
            Some(p) => p,
            None => {
                self.metrics.counter(Metric::HttpJoinerUnpaired).inc();
                return Vec::new();
            }
        };

        let response = Arc::new(resp);
        let id = Uuid::now_v7().to_string();

        self.metrics.counter(Metric::HttpJoinerDone).inc();
        vec![HttpJoinerEvent::Exchange {
            id,
            request: pending.request,
            response,
            sse_events: pending.sse_events,
        }]
    }

    /// Remove pending entries on `source_id` whose last activity is older
    /// than `timeout_us`. Only the named source's entries are inspected;
    /// per-source clocks advance independently. Staleness is measured from
    /// `last_activity_us`, not request-arrival time, so a live SSE stream
    /// with recent events is not evicted regardless of its total duration.
    pub fn cleanup_stale(&mut self, source_id: &str, now_us: i64, timeout_us: i64) -> usize {
        let before = self.pending.len();
        self.pending.retain(|flow_key, pending| {
            if flow_key.source_id != source_id {
                return true;
            }
            let silence = now_us - pending.last_activity_us;
            if silence > timeout_us {
                warn!(
                    flow = %flow_key,
                    silence_secs = silence as f64 / 1_000_000.0,
                    "expiring stale pending HTTP exchange"
                );
                false
            } else {
                true
            }
        });
        let expired = before - self.pending.len();
        if expired > 0 {
            self.metrics
                .counter(Metric::HttpJoinerExpired)
                .add(expired as u64);
        }
        expired
    }

    /// Count of in-flight (unpaired) requests. Primarily for tests + probes.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use h_common::internal_metrics::MetricsSystem;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[
                Metric::HttpJoinerDone,
                Metric::HttpJoinerUnpaired,
                Metric::HttpJoinerExpired,
                Metric::JoinerHeartbeatsReceived,
            ],
        );
        let _svc = sys.start();
        w
    }

    fn flow(port: u16) -> FlowKey {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        FlowKey::new(String::new(), ip, port, ip, 8080)
    }

    fn make_request(fk: FlowKey, ts_us: i64) -> HttpRequestData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        HttpRequestData {
            flow_key: fk,
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/chat".to_string(),
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(b"{}"),
            timestamp_us: ts_us,
            process: None,
        }
    }

    fn make_response(fk: FlowKey, ts_us: i64, sse: bool) -> HttpResponseData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let headers = if sse {
            vec![("content-type".to_string(), "text/event-stream".to_string())]
        } else {
            vec![("content-type".to_string(), "application/json".to_string())]
        };
        HttpResponseData {
            flow_key: fk,
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            status: 200,
            version: 1,
            headers,
            body: if sse {
                Bytes::new()
            } else {
                Bytes::from_static(b"{\"ok\":true}")
            },
            first_byte_timestamp_us: ts_us + 100,
            complete_timestamp_us: ts_us + 200,
            process: None,
        }
    }

    fn make_sse(fk: FlowKey, ts_us: i64, event_type: &str, data: &str) -> SseEventData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        SseEventData {
            flow_key: fk,
            client_addr: (ip, 1234),
            server_addr: (ip, 8080),
            event_type: event_type.to_string(),
            data: data.to_string(),
            timestamp_us: ts_us,
            process: None,
        }
    }

    #[test]
    fn request_emits_request_event() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let events = joiner.process(HttpParseEvent::HttpRequest(make_request(
            flow(5000),
            1_000_000,
        )));
        assert_eq!(events.len(), 1);
        match &events[0] {
            HttpJoinerEvent::Request(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.uri, "/v1/chat");
            }
            _ => panic!("expected Request"),
        }
        assert_eq!(joiner.pending_count(), 1);
    }

    #[test]
    fn non_sse_pair_produces_exchange_with_body() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let fk = flow(5000);
        joiner.process(HttpParseEvent::HttpRequest(make_request(
            fk.clone(),
            1_000_000,
        )));
        let events = joiner.process(HttpParseEvent::HttpResponse(make_response(
            fk, 1_000_000, false,
        )));
        assert_eq!(events.len(), 1);
        match &events[0] {
            HttpJoinerEvent::Exchange {
                id,
                request,
                response,
                sse_events,
            } => {
                let (sse_event_count, sse_data_bytes) = sse_summary(sse_events);
                let xchg = HttpExchange {
                    id: id.clone(),
                    request: request.clone(),
                    response: response.clone(),
                    sse_event_count,
                    sse_data_bytes,
                };
                assert!(!xchg.is_sse());
                assert_eq!(response.status, 200);
                assert!(sse_events.is_empty());
                let body = xchg.stored_response_body().expect("non-sse has body");
                assert_eq!(body.as_ref(), b"{\"ok\":true}");
                assert!(!id.is_empty());
            }
            _ => panic!("expected Exchange"),
        }
        assert_eq!(joiner.pending_count(), 0);
    }

    #[test]
    fn sse_pair_has_none_body_and_carries_events() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let fk = flow(5000);
        joiner.process(HttpParseEvent::HttpRequest(make_request(
            fk.clone(),
            1_000_000,
        )));
        joiner.process(HttpParseEvent::SseEvent(make_sse(
            fk.clone(),
            1_100_000,
            "message_start",
            "{}",
        )));
        let events = joiner.process(HttpParseEvent::HttpResponse(make_response(
            fk, 1_000_000, true,
        )));
        match &events[0] {
            HttpJoinerEvent::Exchange {
                id,
                request,
                response,
                sse_events,
            } => {
                let (sse_event_count, sse_data_bytes) = sse_summary(sse_events);
                let xchg = HttpExchange {
                    id: id.clone(),
                    request: request.clone(),
                    response: response.clone(),
                    sse_event_count,
                    sse_data_bytes,
                };
                assert!(xchg.is_sse());
                assert!(xchg.stored_response_body().is_none());
                assert_eq!(sse_events.len(), 1);
                assert_eq!(sse_events[0].event_type, "message_start");
                assert_eq!(xchg.sse_event_count, 1);
                assert_eq!(xchg.sse_data_bytes, 2); // "{}"
            }
            _ => panic!("expected Exchange"),
        }
    }

    #[test]
    fn response_without_request_bumps_incomplete() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let events = joiner.process(HttpParseEvent::HttpResponse(make_response(
            flow(5000),
            1_000_000,
            false,
        )));
        assert!(events.is_empty());
    }

    #[test]
    fn heartbeat_past_timeout_evicts_pending() {
        let mut joiner = HttpJoiner::new(test_metrics());
        joiner.process(HttpParseEvent::HttpRequest(make_request(
            flow(5000),
            1_000_000,
        )));
        assert_eq!(joiner.pending_count(), 1);

        let events = joiner.process(HttpParseEvent::Heartbeat {
            ts: 2_000_000,
            source_id: String::new(),
        });
        assert!(matches!(
            events.as_slice(),
            [HttpJoinerEvent::Heartbeat { .. }]
        ));
        assert_eq!(joiner.pending_count(), 1, "still fresh");

        let events = joiner.process(HttpParseEvent::Heartbeat {
            ts: 1_000_000 + PENDING_STALE_TIMEOUT_US + 1,
            source_id: String::new(),
        });
        assert!(matches!(
            events.as_slice(),
            [HttpJoinerEvent::Heartbeat { .. }]
        ));
        assert_eq!(joiner.pending_count(), 0);
    }

    #[test]
    fn cleanup_is_per_source() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        let mut req_a = make_request(flow(5000), 1_000_000);
        req_a.flow_key = FlowKey::new("source-a".into(), ip, 5000, ip, 8080);
        joiner.process(HttpParseEvent::HttpRequest(req_a));

        let mut req_b = make_request(flow(5001), 1_000_000);
        req_b.flow_key = FlowKey::new("source-b".into(), ip, 5001, ip, 8080);
        joiner.process(HttpParseEvent::HttpRequest(req_b));
        assert_eq!(joiner.pending_count(), 2);

        joiner.process(HttpParseEvent::Heartbeat {
            ts: 1_000_000 + PENDING_STALE_TIMEOUT_US + 1,
            source_id: "source-a".into(),
        });
        assert_eq!(joiner.pending_count(), 1, "only source-a evicted");
    }

    #[test]
    fn stale_pending_replaced_silently_on_reuse() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let fk = flow(5000);
        joiner.process(HttpParseEvent::HttpRequest(make_request(
            fk.clone(),
            1_000_000,
        )));
        let mut req2 = make_request(fk, 1_000_000);
        req2.timestamp_us = 1_000_000 + PENDING_STALE_TIMEOUT_US + 1;
        let events = joiner.process(HttpParseEvent::HttpRequest(req2));
        assert!(matches!(events.as_slice(), [HttpJoinerEvent::Request(_)]));
        assert_eq!(joiner.pending_count(), 1);
    }

    #[test]
    fn sse_activity_refreshes_staleness_clock() {
        // Long-running SSE stream (> PENDING_STALE_TIMEOUT_US total) must not
        // be evicted as long as SSE events keep arriving. Only silence past
        // the timeout should trigger cleanup.
        let mut joiner = HttpJoiner::new(test_metrics());
        let fk = flow(5000);
        joiner.process(HttpParseEvent::HttpRequest(make_request(
            fk.clone(),
            1_000_000,
        )));

        // Feed an SSE event well after the raw-request-age timeout — with
        // activity-based staleness, this keeps the pending alive.
        let sse_ts = 1_000_000 + PENDING_STALE_TIMEOUT_US + 30_000_000; // +30s past
        joiner.process(HttpParseEvent::SseEvent(make_sse(
            fk.clone(),
            sse_ts,
            "message_delta",
            "{}",
        )));

        // Heartbeat shortly after the SSE event — within the timeout from
        // last_activity. Must not evict.
        joiner.process(HttpParseEvent::Heartbeat {
            ts: sse_ts + 60_000_000, // +60s silence, well under timeout
            source_id: String::new(),
        });
        assert_eq!(joiner.pending_count(), 1, "live stream must not be evicted");

        // Now silence past the timeout from the last SSE event — evict.
        joiner.process(HttpParseEvent::Heartbeat {
            ts: sse_ts + PENDING_STALE_TIMEOUT_US + 1,
            source_id: String::new(),
        });
        assert_eq!(
            joiner.pending_count(),
            0,
            "silent past timeout must be evicted"
        );
    }
}
