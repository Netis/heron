//! HTTP request/response pairing. Consumes `ProtocolEvent`s (per-direction
//! HTTP/SSE parser output) and produces `HttpJoinerEvent`s — a two-phase
//! stream that decouples request observation (for downstream concurrency
//! tracking) from exchange completion (for storage / semantic extraction).
//!
//! The joiner is wire-API-agnostic: it pairs HTTP exchanges regardless of
//! whether they look like LLM traffic. Non-LLM traffic still lands here,
//! which is the whole point — see proposal 0001.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use bytes::Bytes;
use tracing::warn;
use uuid::Uuid;

use ts_common::internal_metrics::{Metric, MetricsWorker};

use crate::model::{HttpRequestData, HttpResponseData, ProtocolEvent, SseEventData};
use crate::net::FlowKey;

/// Upper bound on how long an unpaired HTTP request may sit in `pending`
/// before heartbeats evict it. Covers pathological cases where a response
/// is never observed (connection abort, capture gap, TLS renegotiation).
///
/// This is a transport-layer concern — decoupled from `TurnConfig`'s
/// agent-level idle timeout, which governs a different question ("when is
/// an agent conversation considered finished?"). The two happen to share a
/// 10-minute default by coincidence, not by contract.
pub const PENDING_STALE_TIMEOUT_US: i64 = 600_000_000;

/// HTTP exchange — request + (optional) response pair. Authoritative
/// transport-layer record; persisted to `http_exchanges`.
///
/// `sse_events` (the reconstructed content stream) is NOT a field: SSE events
/// do not land in storage. They travel alongside on the in-flight
/// `HttpJoinerEvent::Exchange` payload for downstream semantic extraction,
/// then are dropped.
#[derive(Debug, Clone)]
pub struct HttpExchange {
    pub id: String,
    pub stream_id: String,
    pub client_ip: IpAddr,
    pub client_port: u16,
    pub server_ip: IpAddr,
    pub server_port: u16,

    pub method: String,
    pub uri: String,
    pub request_headers: Vec<(String, String)>,
    pub request_body: Bytes,

    /// `None` = no response received (pending expired before response).
    pub status: Option<u16>,
    pub response_headers: Vec<(String, String)>,
    /// `None` = SSE (body wasn't retained) or response never arrived.
    /// `Some(Bytes::new())` = real empty body (204/304/HEAD).
    pub response_body: Option<Bytes>,
    pub is_sse: bool,

    pub request_time: i64,
    pub response_first_byte_time: Option<i64>,
    pub response_complete_time: Option<i64>,
}

/// Output event of the `HttpJoiner`. Two downstream-visible phases:
/// `RequestObserved` fires on request arrival (drives `LlmEvent::Start`
/// and metrics concurrency +1); `Exchange` fires on response completion
/// with the paired record plus any SSE events seen on that flow.
#[derive(Debug, Clone)]
pub enum HttpJoinerEvent {
    /// HTTP request observed; exchange still in-flight. Downstream consumers
    /// (`LlmProcessor`) use this to run wire-API detection and emit
    /// concurrency-tracking `Start` events.
    RequestObserved(Arc<HttpRequestData>),

    /// Exchange paired (request + response + any SSE events). `sse_events` is
    /// non-empty iff `exchange.is_sse`.
    Exchange {
        exchange: HttpExchange,
        sse_events: Vec<SseEventData>,
    },

    /// Time-advancing heartbeat. Forwarded from upstream `ProtocolEvent`.
    /// Downstream consumers (metrics, turn tracker) use these to close stale
    /// windows during traffic idle.
    Heartbeat { ts: i64, stream_id: String },
}

/// Per-flow in-flight state.
struct Pending {
    request: HttpRequestData,
    sse_events: Vec<SseEventData>,
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
    pub fn process(&mut self, event: ProtocolEvent) -> Vec<HttpJoinerEvent> {
        match event {
            ProtocolEvent::HttpRequest(req) => self.on_request(req),
            ProtocolEvent::SseEvent(sse) => {
                self.on_sse(sse);
                Vec::new()
            }
            ProtocolEvent::HttpResponse(resp) => self.on_response(resp),
            ProtocolEvent::Heartbeat { ts, stream_id } => {
                self.cleanup_stale(&stream_id, ts, PENDING_STALE_TIMEOUT_US);
                vec![HttpJoinerEvent::Heartbeat { ts, stream_id }]
            }
        }
    }

    fn on_request(&mut self, req: HttpRequestData) -> Vec<HttpJoinerEvent> {
        let flow_key = req.flow_key.clone();

        // Overwriting a pending on the same flow is only interesting if the
        // previous request is still "fresh" — that suggests a genuine protocol
        // anomaly (response lost, pipelined requests we don't understand).
        // If the previous entry is already past the stale timeout, it's a
        // long-dead flow being recycled: replace silently.
        if let Some(prev) = self.pending.get(&flow_key) {
            let age = req.timestamp_us - prev.request.timestamp_us;
            if age > PENDING_STALE_TIMEOUT_US {
                self.pending.remove(&flow_key);
            } else {
                warn!(
                    flow = %flow_key,
                    age_secs = age as f64 / 1_000_000.0,
                    "overwriting pending HTTP exchange — previous request on this flow had no response"
                );
            }
        }

        let arc_req = Arc::new(req.clone());
        self.pending.insert(
            flow_key,
            Pending {
                request: req,
                sse_events: Vec::new(),
            },
        );
        vec![HttpJoinerEvent::RequestObserved(arc_req)]
    }

    fn on_sse(&mut self, sse: SseEventData) {
        if let Some(pending) = self.pending.get_mut(&sse.flow_key) {
            pending.sse_events.push(sse);
        }
    }

    fn on_response(&mut self, resp: HttpResponseData) -> Vec<HttpJoinerEvent> {
        let pending = match self.pending.remove(&resp.flow_key) {
            Some(p) => p,
            None => {
                self.metrics.counter(Metric::HttpExchangesIncomplete).inc();
                return Vec::new();
            }
        };

        let is_sse = !pending.sse_events.is_empty()
            || resp
                .content_type()
                .map(|ct| ct.starts_with("text/event-stream"))
                .unwrap_or(false);

        let exchange = HttpExchange {
            id: Uuid::now_v7().to_string(),
            stream_id: pending.request.flow_key.stream_id.clone(),
            client_ip: pending.request.client_addr.0,
            client_port: pending.request.client_addr.1,
            server_ip: pending.request.server_addr.0,
            server_port: pending.request.server_addr.1,

            method: pending.request.method,
            uri: pending.request.uri,
            request_headers: pending.request.headers,
            request_body: pending.request.body,

            status: Some(resp.status),
            response_headers: resp.headers,
            // Parser emits Bytes::new() for SSE (never retained); translate to
            // None here so the schema reads honestly. Real empty bodies
            // (204/304/HEAD) still pass through as Some(Bytes::new()).
            response_body: if is_sse { None } else { Some(resp.body) },
            is_sse,

            request_time: pending.request.timestamp_us,
            response_first_byte_time: Some(resp.first_byte_timestamp_us),
            response_complete_time: Some(resp.complete_timestamp_us),
        };

        self.metrics.counter(Metric::HttpExchangesCompleted).inc();
        vec![HttpJoinerEvent::Exchange {
            exchange,
            sse_events: pending.sse_events,
        }]
    }

    /// Remove pending requests on `stream_id` older than `timeout_us`. Only
    /// the named stream's entries are inspected; per-stream clocks advance
    /// independently.
    pub fn cleanup_stale(&mut self, stream_id: &str, now_us: i64, timeout_us: i64) -> usize {
        let before = self.pending.len();
        self.pending.retain(|flow_key, pending| {
            if flow_key.stream_id != stream_id {
                return true;
            }
            let age = now_us - pending.request.timestamp_us;
            if age > timeout_us {
                warn!(
                    flow = %flow_key,
                    age_secs = age as f64 / 1_000_000.0,
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
                .counter(Metric::HttpExchangesExpired)
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
    use ts_common::internal_metrics::MetricsSystem;

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[
                Metric::HttpExchangesCompleted,
                Metric::HttpExchangesIncomplete,
                Metric::HttpExchangesExpired,
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
        }
    }

    #[test]
    fn request_emits_request_observed() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let events = joiner.process(ProtocolEvent::HttpRequest(make_request(flow(5000), 1_000_000)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            HttpJoinerEvent::RequestObserved(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.uri, "/v1/chat");
            }
            _ => panic!("expected RequestObserved"),
        }
        assert_eq!(joiner.pending_count(), 1);
    }

    #[test]
    fn non_sse_pair_produces_exchange_with_body() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let fk = flow(5000);
        joiner.process(ProtocolEvent::HttpRequest(make_request(fk.clone(), 1_000_000)));
        let events = joiner.process(ProtocolEvent::HttpResponse(make_response(fk, 1_000_000, false)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            HttpJoinerEvent::Exchange { exchange, sse_events } => {
                assert!(!exchange.is_sse);
                assert_eq!(exchange.status, Some(200));
                assert!(sse_events.is_empty());
                let body = exchange.response_body.as_ref().expect("non-sse has body");
                assert_eq!(body.as_ref(), b"{\"ok\":true}");
                assert!(!exchange.id.is_empty());
            }
            _ => panic!("expected Exchange"),
        }
        assert_eq!(joiner.pending_count(), 0);
    }

    #[test]
    fn sse_pair_has_none_body_and_carries_events() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let fk = flow(5000);
        joiner.process(ProtocolEvent::HttpRequest(make_request(fk.clone(), 1_000_000)));
        joiner.process(ProtocolEvent::SseEvent(make_sse(
            fk.clone(),
            1_100_000,
            "message_start",
            "{}",
        )));
        let events = joiner.process(ProtocolEvent::HttpResponse(make_response(fk, 1_000_000, true)));
        match &events[0] {
            HttpJoinerEvent::Exchange { exchange, sse_events } => {
                assert!(exchange.is_sse);
                assert!(exchange.response_body.is_none());
                assert_eq!(sse_events.len(), 1);
                assert_eq!(sse_events[0].event_type, "message_start");
            }
            _ => panic!("expected Exchange"),
        }
    }

    #[test]
    fn response_without_request_bumps_incomplete() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let events = joiner.process(ProtocolEvent::HttpResponse(make_response(flow(5000), 1_000_000, false)));
        assert!(events.is_empty());
    }

    #[test]
    fn heartbeat_past_timeout_evicts_pending() {
        let mut joiner = HttpJoiner::new(test_metrics());
        joiner.process(ProtocolEvent::HttpRequest(make_request(flow(5000), 1_000_000)));
        assert_eq!(joiner.pending_count(), 1);

        let events = joiner.process(ProtocolEvent::Heartbeat {
            ts: 2_000_000,
            stream_id: String::new(),
        });
        assert!(matches!(events.as_slice(), [HttpJoinerEvent::Heartbeat { .. }]));
        assert_eq!(joiner.pending_count(), 1, "still fresh");

        let events = joiner.process(ProtocolEvent::Heartbeat {
            ts: 1_000_000 + PENDING_STALE_TIMEOUT_US + 1,
            stream_id: String::new(),
        });
        assert!(matches!(events.as_slice(), [HttpJoinerEvent::Heartbeat { .. }]));
        assert_eq!(joiner.pending_count(), 0);
    }

    #[test]
    fn cleanup_is_per_stream() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        let mut req_a = make_request(flow(5000), 1_000_000);
        req_a.flow_key = FlowKey::new("stream-a".into(), ip, 5000, ip, 8080);
        joiner.process(ProtocolEvent::HttpRequest(req_a));

        let mut req_b = make_request(flow(5001), 1_000_000);
        req_b.flow_key = FlowKey::new("stream-b".into(), ip, 5001, ip, 8080);
        joiner.process(ProtocolEvent::HttpRequest(req_b));
        assert_eq!(joiner.pending_count(), 2);

        joiner.process(ProtocolEvent::Heartbeat {
            ts: 1_000_000 + PENDING_STALE_TIMEOUT_US + 1,
            stream_id: "stream-a".into(),
        });
        assert_eq!(joiner.pending_count(), 1, "only stream-a evicted");
    }

    #[test]
    fn stale_pending_replaced_silently_on_reuse() {
        let mut joiner = HttpJoiner::new(test_metrics());
        let fk = flow(5000);
        joiner.process(ProtocolEvent::HttpRequest(make_request(fk.clone(), 1_000_000)));
        let mut req2 = make_request(fk, 1_000_000);
        req2.timestamp_us = 1_000_000 + PENDING_STALE_TIMEOUT_US + 1;
        let events = joiner.process(ProtocolEvent::HttpRequest(req2));
        assert!(matches!(events.as_slice(), [HttpJoinerEvent::RequestObserved(_)]));
        assert_eq!(joiner.pending_count(), 1);
    }
}
