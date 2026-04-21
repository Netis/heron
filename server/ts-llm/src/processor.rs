use std::sync::Arc;

use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_protocol::joiner::{HttpExchange, HttpJoinerEvent};
use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};
use uuid::Uuid;

use crate::model::{AgentIdentity, ApiType, LlmCall, LlmCallStart, LlmEvent};
use crate::profile::AgentProfileRegistry;
use crate::wire_api_registry::WireApiRegistry;

/// Processes `HttpJoinerEvent`s and extracts `LlmCall` records. Stateless —
/// request/response pairing now lives in `ts_protocol::joiner::HttpJoiner`.
pub struct LlmProcessor {
    wire_apis: Arc<WireApiRegistry>,
    registry: Arc<AgentProfileRegistry>,
    metrics: MetricsWorker,
}

impl LlmProcessor {
    pub fn new(
        wire_apis: Arc<WireApiRegistry>,
        registry: Arc<AgentProfileRegistry>,
        metrics: MetricsWorker,
    ) -> Self {
        Self {
            wire_apis,
            registry,
            metrics,
        }
    }

    /// Process a single joiner event. Returns LlmEvents (Start and/or
    /// Complete and/or Heartbeat).
    pub fn process(&mut self, event: HttpJoinerEvent) -> Vec<LlmEvent> {
        match event {
            HttpJoinerEvent::RequestObserved(req) => self.on_request_observed(&req),
            HttpJoinerEvent::Exchange { exchange, sse_events } => {
                self.on_exchange(exchange, sse_events)
            }
            HttpJoinerEvent::Heartbeat { ts, stream_id } => {
                vec![LlmEvent::Heartbeat { ts, stream_id }]
            }
        }
    }

    fn on_request_observed(&mut self, req: &HttpRequestData) -> Vec<LlmEvent> {
        let Some(extractor) = self.wire_apis.detect(req) else {
            self.metrics.counter(Metric::LlmRequestsIgnored).inc();
            return Vec::new();
        };
        let info = extractor.extract_request(req);
        self.metrics.counter(Metric::LlmRequestsDetected).inc();
        vec![LlmEvent::Start(LlmCallStart {
            stream_id: req.flow_key.stream_id.clone(),
            wire_api: extractor.name(),
            model: info.model,
            is_stream: info.is_stream,
            server_ip: req.server_addr.0,
            timestamp_us: req.timestamp_us,
        })]
    }

    fn on_exchange(&mut self, exchange: HttpExchange, sse_events: Vec<SseEventData>) -> Vec<LlmEvent> {
        // Reconstruct an HttpRequestData view over the exchange for wire-API
        // detection. This keeps the processor stateless — no pending map.
        let req_view = http_request_view(&exchange);
        let Some(extractor) = self.wire_apis.detect(&req_view) else {
            // Already counted LlmRequestsIgnored on RequestObserved; silent here.
            return Vec::new();
        };

        let req_info = extractor.extract_request(&req_view);
        let resp_view = http_response_view(&exchange);

        // resp_info carries tokens / finish_reason / response_id / reconstructed body.
        let resp_info = if !sse_events.is_empty() {
            extractor.extract_sse(&sse_events)
        } else {
            extractor.extract_response(&resp_view)
        };

        let model = resp_info.model.unwrap_or(req_info.model);

        let request_time = exchange.request_time;
        let response_time = exchange.response_first_byte_time.unwrap_or(request_time);
        let complete_time = exchange.response_complete_time.unwrap_or(request_time);

        let ttfb_ms = if response_time > request_time {
            Some((response_time - request_time) as f64 / 1000.0)
        } else {
            None
        };
        let e2e_latency_ms = if complete_time > request_time {
            Some((complete_time - request_time) as f64 / 1000.0)
        } else {
            None
        };

        let mut total_tokens = resp_info.total_tokens;
        if total_tokens.is_none() {
            total_tokens = match (resp_info.input_tokens, resp_info.output_tokens) {
                (Some(i), Some(o)) => Some(i + o),
                _ => None,
            };
        }

        self.metrics.counter(Metric::LlmCallsCompleted).inc();

        let request_body = std::str::from_utf8(&exchange.request_body)
            .ok()
            .map(|s| s.to_string());

        let call = LlmCall {
            stream_id: exchange.stream_id.clone(),
            id: Uuid::now_v7().to_string(),
            wire_api: extractor.name(),
            model,
            api_type: ApiType::Chat,
            tenant_id: req_info.tenant_id,
            request_time,
            response_time: Some(response_time),
            complete_time: Some(complete_time),
            request_path: exchange.uri.clone(),
            is_stream: req_info.is_stream,
            request_body,
            status_code: exchange.status,
            finish_reason: resp_info.finish_reason,
            response_body: resp_info.response_body,
            input_tokens: resp_info.input_tokens,
            output_tokens: resp_info.output_tokens,
            total_tokens,
            cache_read_input_tokens: resp_info.cache_read_input_tokens,
            cache_creation_input_tokens: resp_info.cache_creation_input_tokens,
            ttfb_ms,
            e2e_latency_ms,
            client_ip: exchange.client_ip,
            client_port: exchange.client_port,
            server_ip: exchange.server_ip,
            server_port: exchange.server_port,
            response_id: resp_info.response_id,
            request_headers: exchange.request_headers,
            response_headers: exchange.response_headers,
        };

        let agent = self.build_identity(&call);
        vec![LlmEvent::Complete {
            call: Arc::new(call),
            agent,
        }]
    }

    fn build_identity(&self, call: &LlmCall) -> Option<AgentIdentity> {
        let profile = self.registry.find(call)?;
        let ids = profile.extract_ids(call)?;
        Some(AgentIdentity {
            agent_kind: profile.name(),
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        })
    }
}

/// Build an `HttpRequestData` view backed by an `HttpExchange`'s request
/// side — for wire-API detection + extraction, which accept `HttpRequestData`
/// by reference.
fn http_request_view(exchange: &HttpExchange) -> HttpRequestData {
    use ts_protocol::net::FlowKey;
    HttpRequestData {
        flow_key: FlowKey::new(
            exchange.stream_id.clone(),
            exchange.client_ip,
            exchange.client_port,
            exchange.server_ip,
            exchange.server_port,
        ),
        client_addr: (exchange.client_ip, exchange.client_port),
        server_addr: (exchange.server_ip, exchange.server_port),
        method: exchange.method.clone(),
        uri: exchange.uri.clone(),
        version: 1,
        headers: exchange.request_headers.clone(),
        body: exchange.request_body.clone(),
        timestamp_us: exchange.request_time,
    }
}

/// Build an `HttpResponseData` view for non-SSE extraction. For SSE, the
/// caller should use `extract_sse` on the accumulated events instead — this
/// view's body is empty for SSE exchanges (see `HttpExchange.response_body`).
fn http_response_view(exchange: &HttpExchange) -> HttpResponseData {
    use bytes::Bytes;
    use ts_protocol::net::FlowKey;
    HttpResponseData {
        flow_key: FlowKey::new(
            exchange.stream_id.clone(),
            exchange.client_ip,
            exchange.client_port,
            exchange.server_ip,
            exchange.server_port,
        ),
        client_addr: (exchange.client_ip, exchange.client_port),
        server_addr: (exchange.server_ip, exchange.server_port),
        status: exchange.status.unwrap_or(0),
        version: 1,
        headers: exchange.response_headers.clone(),
        body: exchange.response_body.clone().unwrap_or_else(Bytes::new),
        first_byte_timestamp_us: exchange.response_first_byte_time.unwrap_or(exchange.request_time),
        complete_timestamp_us: exchange.response_complete_time.unwrap_or(exchange.request_time),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FinishReason;
    use crate::wire_apis as wa;
    use bytes::Bytes;
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_common::internal_metrics::MetricsSystem;
    use ts_protocol::model::{HttpRequestData, SseEventData};
    use ts_protocol::net::FlowKey;

    fn empty_registry() -> Arc<AgentProfileRegistry> {
        Arc::new(AgentProfileRegistry::new())
    }

    fn wire_apis() -> Arc<WireApiRegistry> {
        Arc::new(crate::wire_apis::build_default_wire_api_registry())
    }

    fn test_metrics() -> MetricsWorker {
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[
                Metric::LlmRequestsDetected,
                Metric::LlmRequestsIgnored,
                Metric::LlmCallsCompleted,
            ],
        );
        let _svc = sys.start();
        w
    }

    fn flow() -> FlowKey {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        FlowKey::new(String::new(), ip, 5000, ip, 8080)
    }

    fn openai_request(body_json: &serde_json::Value) -> HttpRequestData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        HttpRequestData {
            flow_key: flow(),
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            version: 1,
            headers: vec![
                ("authorization".to_string(), "Bearer sk-test-key".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body_json.to_string()),
            timestamp_us: 1_000_000,
        }
    }

    fn exchange_from(req: HttpRequestData, resp_body: Bytes, is_sse: bool) -> HttpExchange {
        HttpExchange {
            id: "xchg-1".to_string(),
            stream_id: req.flow_key.stream_id.clone(),
            client_ip: req.client_addr.0,
            client_port: req.client_addr.1,
            server_ip: req.server_addr.0,
            server_port: req.server_addr.1,
            method: req.method.clone(),
            uri: req.uri.clone(),
            request_headers: req.headers.clone(),
            request_body: req.body.clone(),
            status: Some(200),
            response_headers: if is_sse {
                vec![("content-type".to_string(), "text/event-stream".to_string())]
            } else {
                vec![("content-type".to_string(), "application/json".to_string())]
            },
            response_body: if is_sse { None } else { Some(resp_body) },
            is_sse,
            request_time: req.timestamp_us,
            response_first_byte_time: Some(req.timestamp_us + 100_000),
            response_complete_time: Some(req.timestamp_us + 200_000),
        }
    }

    fn sse_event(event_type: &str, data: &str, ts: i64) -> SseEventData {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        SseEventData {
            flow_key: flow(),
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            event_type: event_type.to_string(),
            data: data.to_string(),
            timestamp_us: ts,
        }
    }

    #[test]
    fn request_observed_detects_and_emits_start() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(wire_apis(), empty_registry(), test_metrics());
        let body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        let events = proc.process(HttpJoinerEvent::RequestObserved(Arc::new(openai_request(&body))));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Start(s) => {
                assert_eq!(s.wire_api, wa::OPENAI_CHAT);
                assert_eq!(s.model, "gpt-4");
                assert!(!s.is_stream);
            }
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn non_llm_request_observed_bumps_ignored_no_event() {
        let mut proc = LlmProcessor::new(wire_apis(), empty_registry(), test_metrics());
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let req = HttpRequestData {
            flow_key: flow(),
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "GET".to_string(),
            uri: "/health".to_string(),
            version: 1,
            headers: vec![],
            body: Bytes::new(),
            timestamp_us: 0,
        };
        let events = proc.process(HttpJoinerEvent::RequestObserved(Arc::new(req)));
        assert!(events.is_empty());
    }

    #[test]
    fn exchange_non_sse_emits_complete_with_correlation_id() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(wire_apis(), empty_registry(), test_metrics());
        let req_body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        let req = openai_request(&req_body);
        let resp_body = json!({
            "id": "chatcmpl-xyz",
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        });
        let exchange = exchange_from(req, Bytes::from(resp_body.to_string()), false);
        let events = proc.process(HttpJoinerEvent::Exchange {
            exchange,
            sse_events: vec![],
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert_eq!(call.wire_api, wa::OPENAI_CHAT);
                assert_eq!(call.request_path, "/v1/chat/completions");
                assert_eq!(call.finish_reason, Some(FinishReason::Complete));
                assert_eq!(call.input_tokens, Some(5));
                assert_eq!(call.output_tokens, Some(3));
                assert_eq!(call.total_tokens, Some(8));
                assert_eq!(call.response_id.as_deref(), Some("chatcmpl-xyz"));
                assert!(call.response_body.is_some());
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn exchange_sse_reconstructs_output_and_tokens() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(wire_apis(), empty_registry(), test_metrics());
        let req_body = json!({"model": "gpt-4", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
        let req = openai_request(&req_body);
        let exchange = exchange_from(req, Bytes::new(), true);
        let sse = vec![
            sse_event(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
                1_100_000,
            ),
            sse_event(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{"content":"hello"}}]}"#,
                1_150_000,
            ),
            sse_event(
                "",
                r#"{"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
                1_200_000,
            ),
        ];
        let events = proc.process(HttpJoinerEvent::Exchange { exchange, sse_events: sse });
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert!(call.is_stream);
                assert_eq!(call.finish_reason, Some(FinishReason::Complete));
                assert_eq!(call.input_tokens, Some(5));
                assert_eq!(call.output_tokens, Some(2));
                assert!(call.response_body.is_some());
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn heartbeat_passthrough() {
        let mut proc = LlmProcessor::new(wire_apis(), empty_registry(), test_metrics());
        let events = proc.process(HttpJoinerEvent::Heartbeat {
            ts: 1_234_567,
            stream_id: "s1".into(),
        });
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::Heartbeat { ts: 1_234_567, .. }]
        ));
    }

    #[test]
    fn claude_cli_exchange_attaches_identity() {
        use crate::agents::build_default_registry;
        let mut proc = LlmProcessor::new(wire_apis(), Arc::new(build_default_registry()), test_metrics());

        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let body = serde_json::json!({
            "model": "claude-sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        let req = HttpRequestData {
            flow_key: flow(),
            client_addr: (ip, 5000),
            server_addr: (ip, 8080),
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            version: 1,
            headers: vec![
                ("user-agent".to_string(), "claude-cli/2.1.98".to_string()),
                (
                    "x-claude-code-session-id".to_string(),
                    "sess-xyz".to_string(),
                ),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body.to_string()),
            timestamp_us: 1_000_000,
        };
        let resp_body = serde_json::json!({
            "id": "msg_01",
            "model": "claude-sonnet",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let exchange = exchange_from(req, Bytes::from(resp_body.to_string()), false);
        let events = proc.process(HttpJoinerEvent::Exchange {
            exchange,
            sse_events: vec![],
        });
        match &events[0] {
            LlmEvent::Complete { call, agent } => {
                let id = agent.as_ref().expect("claude-cli should match");
                assert_eq!(id.agent_kind, "claude-cli");
                assert_eq!(id.session_id, "sess-xyz");
                assert!(!call.id.is_empty());
                assert_eq!(call.request_path, "/v1/messages");
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn headers_pass_through() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(wire_apis(), empty_registry(), test_metrics());
        let req_body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        let req = openai_request(&req_body);
        let resp_body = json!({
            "id": "chatcmpl-1",
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let mut exchange = exchange_from(req, Bytes::from(resp_body.to_string()), false);
        exchange
            .response_headers
            .push(("x-request-id".to_string(), "rid-42".to_string()));
        let events = proc.process(HttpJoinerEvent::Exchange {
            exchange,
            sse_events: vec![],
        });
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert!(call.request_headers.iter().any(|(k, _)| k == "authorization"));
                assert!(call
                    .response_headers
                    .iter()
                    .any(|(k, v)| k == "x-request-id" && v == "rid-42"));
            }
            _ => panic!("expected Complete"),
        }
    }
}
