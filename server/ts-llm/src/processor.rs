use std::collections::HashMap;
use std::sync::Arc;

use tracing::warn;
use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_protocol::model::{HttpRequestData, HttpResponseData, ProtocolEvent, SseEventData};
use ts_protocol::net::FlowKey;
use uuid::Uuid;

use crate::anthropic::AnthropicProvider;
use crate::detector::detect_provider;
use crate::model::{
    ApiType, CallIdentity, LlmCall, LlmCallStart, LlmEvent, Provider, ProviderFormat,
};
use crate::openai::{OpenAiChatProvider, OpenAiResponsesProvider};
use crate::profile::ProfileRegistry;

/// Default stale-pending-call timeout. Pending LLM calls older than this are
/// evicted on every heartbeat and silently replaced (rather than warned)
/// when the same flow is reused for a new request. 10 minutes matches the
/// turn-tracker idle timeout.
const PENDING_STALE_TIMEOUT_US: i64 = 600_000_000;

/// Get the Provider implementation for a given format.
fn get_provider(format: ProviderFormat) -> &'static dyn Provider {
    match format {
        ProviderFormat::Anthropic => &AnthropicProvider,
        ProviderFormat::OpenAI => &OpenAiChatProvider,
        ProviderFormat::OpenAIResponses => &OpenAiResponsesProvider,
    }
}

/// Pending LLM call waiting for response completion.
struct PendingCall {
    request: HttpRequestData,
    provider: ProviderFormat,
    model: String,
    is_stream: bool,
    tenant_id: Option<String>,
    sse_events: Vec<SseEventData>,
}

/// Processes ProtocolEvents and extracts LlmCall records.
pub struct LlmProcessor {
    pending: HashMap<FlowKey, PendingCall>,
    registry: Arc<ProfileRegistry>,
    metrics: MetricsWorker,
}

impl LlmProcessor {
    pub fn new(registry: Arc<ProfileRegistry>, metrics: MetricsWorker) -> Self {
        Self {
            pending: HashMap::new(),
            registry,
            metrics,
        }
    }

    /// Process a single protocol event. Returns LlmEvents (Start and/or Complete).
    pub fn process(&mut self, event: ProtocolEvent) -> Vec<LlmEvent> {
        match event {
            ProtocolEvent::HttpRequest(req) => self.on_request(req),
            ProtocolEvent::SseEvent(sse) => {
                self.on_sse(sse);
                Vec::new()
            }
            ProtocolEvent::HttpResponse(resp) => match self.on_response(resp) {
                Some(call) => {
                    let identity = self.build_identity(&call);
                    vec![LlmEvent::Complete {
                        call: Arc::new(call),
                        identity,
                    }]
                }
                None => Vec::new(),
            },
            ProtocolEvent::Heartbeat { ts, stream_id } => {
                self.cleanup_stale(ts, PENDING_STALE_TIMEOUT_US);
                vec![LlmEvent::Heartbeat { ts, stream_id }]
            }
        }
    }

    fn on_request(&mut self, req: HttpRequestData) -> Vec<LlmEvent> {
        let provider = match detect_provider(&req) {
            Some(p) => p,
            None => {
                self.metrics.counter(Metric::LlmRequestsIgnored).inc();
                return Vec::new();
            }
        };

        let extractor = get_provider(provider);
        let info = extractor.extract_request(&req);

        let flow_key = req.flow_key.clone();
        let start = LlmCallStart {
            stream_id: req.flow_key.stream_id.clone(),
            provider,
            model: info.model.clone(),
            is_stream: info.is_stream,
            server_ip: req.server_addr.0,
            timestamp_us: req.timestamp_us,
        };

        // Overwriting a pending call on the same flow is only interesting
        // if the previous request is still "fresh" — that suggests a
        // genuine protocol anomaly (response lost, pipelined requests we
        // don't understand). If the previous entry is already past the
        // stale timeout, it's just a long-dead flow being recycled for a
        // new TCP connection with the same 5-tuple: replace silently.
        if let Some(prev) = self.pending.get(&flow_key) {
            let age = req.timestamp_us - prev.request.timestamp_us;
            if age > PENDING_STALE_TIMEOUT_US {
                self.pending.remove(&flow_key);
            } else {
                warn!(
                    flow = %flow_key,
                    age_secs = age as f64 / 1_000_000.0,
                    "overwriting pending LLM call — previous request on this flow had no response"
                );
            }
        }

        self.pending.insert(
            flow_key,
            PendingCall {
                request: req,
                provider,
                model: info.model,
                is_stream: info.is_stream,
                tenant_id: info.tenant_id,
                sse_events: Vec::new(),
            },
        );

        self.metrics.counter(Metric::LlmRequestsDetected).inc();
        vec![LlmEvent::Start(start)]
    }

    fn on_sse(&mut self, sse: SseEventData) {
        if let Some(pending) = self.pending.get_mut(&sse.flow_key) {
            pending.sse_events.push(sse);
        }
    }

    fn on_response(&mut self, resp: HttpResponseData) -> Option<LlmCall> {
        let pending = match self.pending.remove(&resp.flow_key) {
            Some(p) => p,
            None => {
                self.metrics.counter(Metric::LlmResponsesOrphaned).inc();
                return None;
            }
        };

        let extractor = get_provider(pending.provider);

        // SSE path: the event stream is the ground truth. The response body is
        // raw SSE text (not JSON) and ts-protocol no longer retains it, so
        // calling extract_response there would always yield Value::Null.
        // Non-SSE path: parse the response body as JSON.
        let resp_info = if pending.is_stream && !pending.sse_events.is_empty() {
            extractor.extract_sse(&pending.sse_events)
        } else {
            extractor.extract_response(&resp)
        };

        let model = resp_info.model.unwrap_or(pending.model);

        let request_time = pending.request.timestamp_us;
        let response_time = resp.first_byte_timestamp_us;
        let complete_time = resp.complete_timestamp_us;

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
        let id = Uuid::now_v7().to_string();

        let request_body = std::str::from_utf8(&pending.request.body)
            .ok()
            .map(|s| s.to_string());

        Some(LlmCall {
            stream_id: pending.request.flow_key.stream_id.clone(),
            id,
            provider: pending.provider,
            model,
            api_type: ApiType::Chat,
            tenant_id: pending.tenant_id,
            request_time,
            response_time: Some(response_time),
            complete_time: Some(complete_time),
            request_path: pending.request.uri,
            is_stream: pending.is_stream,
            request_body,
            status_code: Some(resp.status),
            finish_reason: resp_info.finish_reason,
            response_body: resp_info.response_body,
            input_tokens: resp_info.input_tokens,
            output_tokens: resp_info.output_tokens,
            total_tokens,
            cache_read_input_tokens: resp_info.cache_read_input_tokens,
            cache_creation_input_tokens: resp_info.cache_creation_input_tokens,
            ttfb_ms,
            e2e_latency_ms,
            client_ip: pending.request.client_addr.0,
            client_port: pending.request.client_addr.1,
            server_ip: pending.request.server_addr.0,
            server_port: pending.request.server_addr.1,
            response_id: resp_info.response_id,
            request_headers: pending.request.headers,
            response_headers: resp.headers,
        })
    }

    /// Get the count of pending (unmatched) requests.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn build_identity(&self, call: &LlmCall) -> Option<CallIdentity> {
        let profile = self.registry.find(call)?;
        let ids = profile.extract_ids(call)?;
        let name = profile.name();
        Some(CallIdentity {
            profile_name: name,
            client_kind: name.to_string(),
            session_id: ids.session_id,
            turn_id_hint: ids.turn_id,
        })
    }

    /// Remove pending calls older than `timeout_us` microseconds.
    /// Returns the number of expired entries removed.
    pub fn cleanup_stale(&mut self, now_us: i64, timeout_us: i64) -> usize {
        let before = self.pending.len();
        self.pending.retain(|flow_key, pending| {
            let age = now_us - pending.request.timestamp_us;
            if age > timeout_us {
                warn!(
                    flow = %flow_key,
                    age_secs = age as f64 / 1_000_000.0,
                    model = %pending.model,
                    "expiring stale pending LLM call"
                );
                false
            } else {
                true
            }
        });
        let expired = before - self.pending.len();
        if expired > 0 {
            self.metrics
                .counter(Metric::LlmPendingExpired)
                .add(expired as u64);
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FinishReason;
    use bytes::Bytes;
    use std::net::IpAddr;
    use ts_protocol::net::FlowKey;

    fn empty_registry() -> std::sync::Arc<crate::profile::ProfileRegistry> {
        std::sync::Arc::new(crate::profile::ProfileRegistry::new())
    }

    fn test_metrics() -> MetricsWorker {
        use ts_common::internal_metrics::MetricsSystem;
        let mut sys = MetricsSystem::new();
        let w = sys.register_worker(
            "test",
            &[
                Metric::LlmRequestsDetected,
                Metric::LlmRequestsIgnored,
                Metric::LlmCallsCompleted,
                Metric::LlmResponsesOrphaned,
                Metric::LlmPendingExpired,
            ],
        );
        let _svc = sys.start();
        w
    }

    fn flow() -> FlowKey {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        FlowKey::new(String::new(), ip, 5000, ip, 8080)
    }

    fn addr() -> ((IpAddr, u16), (IpAddr, u16)) {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        ((ip, 5000), (ip, 8080))
    }

    fn openai_chat_request(body_json: &serde_json::Value) -> HttpRequestData {
        let (client, server) = addr();
        HttpRequestData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            version: 1,
            headers: vec![
                (
                    "authorization".to_string(),
                    "Bearer sk-test-key-1234".to_string(),
                ),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body_json.to_string()),
            timestamp_us: 1_000_000,
        }
    }

    fn anthropic_request(body_json: &serde_json::Value) -> HttpRequestData {
        let (client, server) = addr();
        HttpRequestData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
            method: "POST".to_string(),
            uri: "/v1/messages".to_string(),
            version: 1,
            headers: vec![
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("x-api-key".to_string(), "sk-ant-api03-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body_json.to_string()),
            timestamp_us: 1_000_000,
        }
    }

    fn http_response(body_json: &serde_json::Value) -> HttpResponseData {
        let (client, server) = addr();
        HttpResponseData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
            status: 200,
            version: 1,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(body_json.to_string()),
            first_byte_timestamp_us: 1_100_000,
            complete_timestamp_us: 1_200_000,
        }
    }

    fn sse_event(event_type: &str, data: &str, ts: i64) -> SseEventData {
        let (client, server) = addr();
        SseEventData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
            event_type: event_type.to_string(),
            data: data.to_string(),
            timestamp_us: ts,
        }
    }

    #[test]
    fn test_openai_chat_non_streaming() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        let req_body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        let events = proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Start(s) => {
                assert_eq!(s.provider, ProviderFormat::OpenAI);
                assert_eq!(s.model, "gpt-4");
                assert!(!s.is_stream);
            }
            _ => panic!("expected Start event"),
        }

        let resp_body = json!({
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        });
        let events = proc.process(ProtocolEvent::HttpResponse(http_response(&resp_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert_eq!(call.provider, ProviderFormat::OpenAI);
                assert_eq!(call.model, "gpt-4");
                assert!(!call.is_stream);
                assert_eq!(call.finish_reason, Some(FinishReason::Complete));
                assert_eq!(call.input_tokens, Some(5));
                assert_eq!(call.output_tokens, Some(3));
                assert_eq!(call.total_tokens, Some(8));
                assert_eq!(call.status_code, Some(200));
                assert!(call.ttfb_ms.unwrap() > 0.0);
                assert!(call.e2e_latency_ms.unwrap() > 0.0);
            }
            _ => panic!("expected Complete event"),
        }
        assert_eq!(proc.pending_count(), 0);
    }

    #[test]
    fn test_openai_chat_streaming() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        let req_body = json!({"model": "gpt-4", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
        let events = proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Start(s) => assert!(s.is_stream),
            _ => panic!("expected Start"),
        }

        // SSE chunks
        let chunks = vec![
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
            sse_event("", "[DONE]", 1_210_000),
        ];
        for chunk in chunks {
            let events = proc.process(ProtocolEvent::SseEvent(chunk));
            assert!(events.is_empty(), "SSE events should not produce LlmEvents");
        }

        // Final HTTP response closes the call
        let resp = {
            let (client, server) = addr();
            HttpResponseData {
                flow_key: flow(),
                client_addr: client,
                server_addr: server,
                status: 200,
                version: 1,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Bytes::new(),
                first_byte_timestamp_us: 1_100_000,
                complete_timestamp_us: 1_250_000,
            }
        };
        let events = proc.process(ProtocolEvent::HttpResponse(resp));
        assert_eq!(events.len(), 1);
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
    fn test_anthropic_streaming() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        let req_body = json!({"model": "claude-3-opus", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
        let events = proc.process(ProtocolEvent::HttpRequest(anthropic_request(&req_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Start(s) => {
                assert_eq!(s.provider, ProviderFormat::Anthropic);
                assert!(s.is_stream);
            }
            _ => panic!("expected Start"),
        }

        let sse_chunks = vec![
            sse_event(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_01","model":"claude-3-opus","role":"assistant","usage":{"input_tokens":10}}}"#,
                1_100_000,
            ),
            sse_event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                1_110_000,
            ),
            sse_event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
                1_120_000,
            ),
            sse_event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
                1_130_000,
            ),
            sse_event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
                1_140_000,
            ),
            sse_event("message_stop", r#"{"type":"message_stop"}"#, 1_150_000),
        ];
        for chunk in sse_chunks {
            assert!(proc.process(ProtocolEvent::SseEvent(chunk)).is_empty());
        }

        let resp = {
            let (client, server) = addr();
            HttpResponseData {
                flow_key: flow(),
                client_addr: client,
                server_addr: server,
                status: 200,
                version: 1,
                headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
                body: Bytes::new(),
                first_byte_timestamp_us: 1_100_000,
                complete_timestamp_us: 1_200_000,
            }
        };
        let events = proc.process(ProtocolEvent::HttpResponse(resp));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert_eq!(call.provider, ProviderFormat::Anthropic);
                assert!(call.is_stream);
                assert_eq!(call.finish_reason, Some(FinishReason::Complete));
                assert_eq!(call.input_tokens, Some(10));
                assert_eq!(call.output_tokens, Some(3));
                assert_eq!(call.total_tokens, Some(13));
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn test_response_without_request_ignored() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        let resp_body = json!({"model": "gpt-4", "choices": [{"finish_reason": "stop"}]});
        let events = proc.process(ProtocolEvent::HttpResponse(http_response(&resp_body)));
        assert!(
            events.is_empty(),
            "response with no pending request should be ignored"
        );
    }

    #[test]
    fn test_cleanup_stale_pending() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        let req_body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));
        assert_eq!(proc.pending_count(), 1);

        // Not stale yet (request was at 1_000_000 us = 1s)
        let expired = proc.cleanup_stale(2_000_000, 5_000_000); // now=2s, timeout=5s
        assert_eq!(expired, 0);
        assert_eq!(proc.pending_count(), 1);

        // Now stale (request was at 1s, now=7s, timeout=5s)
        let expired = proc.cleanup_stale(7_000_000, 5_000_000);
        assert_eq!(expired, 1);
        assert_eq!(proc.pending_count(), 0);
    }

    #[test]
    fn heartbeat_triggers_stale_cleanup() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        let req_body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));
        assert_eq!(proc.pending_count(), 1);

        // Heartbeat well inside the timeout window — nothing should evict.
        let events = proc.process(ProtocolEvent::Heartbeat {
            ts: 2_000_000,
            stream_id: String::new(),
        });
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::Heartbeat { ts: 2_000_000, .. }]
        ));
        assert_eq!(proc.pending_count(), 1);

        // Heartbeat past the 600s timeout — pending must be evicted.
        let events = proc.process(ProtocolEvent::Heartbeat {
            ts: 1_000_000 + PENDING_STALE_TIMEOUT_US + 1,
            stream_id: String::new(),
        });
        assert!(matches!(events.as_slice(), [LlmEvent::Heartbeat { .. }]));
        assert_eq!(proc.pending_count(), 0);
    }

    #[test]
    fn stale_pending_is_replaced_silently_on_reuse() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        // First request establishes a pending entry at t=1s.
        let req_body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));
        assert_eq!(proc.pending_count(), 1);

        // Second request on the same flow, well past the stale timeout.
        // Should silently replace — still exactly one pending entry.
        let mut req2 = openai_chat_request(&req_body);
        req2.timestamp_us = 1_000_000 + PENDING_STALE_TIMEOUT_US + 1;
        let events = proc.process(ProtocolEvent::HttpRequest(req2));
        assert!(matches!(events.as_slice(), [LlmEvent::Start(_)]));
        assert_eq!(proc.pending_count(), 1);
    }

    #[test]
    fn test_non_llm_request_ignored() {
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());
        let (client, server) = addr();
        let req = HttpRequestData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
            method: "GET".to_string(),
            uri: "/health".to_string(),
            version: 1,
            headers: vec![],
            body: Bytes::new(),
            timestamp_us: 0,
        };
        let events = proc.process(ProtocolEvent::HttpRequest(req));
        assert!(events.is_empty());
        assert_eq!(proc.pending_count(), 0);
    }

    #[test]
    fn complete_for_claude_cli_attaches_identity() {
        use crate::profiles::build_default_registry;
        use std::sync::Arc;

        let registry = Arc::new(build_default_registry());
        let mut proc = LlmProcessor::new(registry, test_metrics());

        let (client, server) = addr();
        let body = serde_json::json!({
            "model": "claude-sonnet",
            "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        let req = HttpRequestData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
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
        proc.process(ProtocolEvent::HttpRequest(req));

        let resp_body = serde_json::json!({
            "id": "msg_01",
            "model": "claude-sonnet",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let events = proc.process(ProtocolEvent::HttpResponse(http_response(&resp_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call, identity } => {
                let id = identity.as_ref().expect("claude-cli should match");
                assert_eq!(id.profile_name, "claude-cli");
                assert_eq!(id.client_kind, "claude-cli");
                assert_eq!(id.session_id, "sess-xyz");
                assert_eq!(
                    id.turn_id_hint, None,
                    "anthropic path has no explicit turn_id"
                );
                assert_eq!(call.id.len() > 0, true);
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn complete_without_profile_match_has_no_identity() {
        use crate::profiles::build_default_registry;
        use std::sync::Arc;

        let registry = Arc::new(build_default_registry());
        let mut proc = LlmProcessor::new(registry, test_metrics());

        let req_body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        });
        proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));

        let resp_body = serde_json::json!({
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        });
        let events = proc.process(ProtocolEvent::HttpResponse(http_response(&resp_body)));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call: _, identity } => {
                assert!(identity.is_none(), "no profile should match");
            }
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn test_headers_and_response_id_passed_through() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(empty_registry(), test_metrics());

        let req_body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        proc.process(ProtocolEvent::HttpRequest(openai_chat_request(&req_body)));

        let resp_body = json!({
            "id": "chatcmpl-xyz789",
            "model": "gpt-4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        });
        let (client, server) = addr();
        let resp = HttpResponseData {
            flow_key: flow(),
            client_addr: client,
            server_addr: server,
            status: 200,
            version: 1,
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-request-id".to_string(), "req_test_456".to_string()),
            ],
            body: Bytes::from(resp_body.to_string()),
            first_byte_timestamp_us: 1_100_000,
            complete_timestamp_us: 1_200_000,
        };
        let events = proc.process(ProtocolEvent::HttpResponse(resp));
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                // response_id extracted from body
                assert_eq!(call.response_id.as_deref(), Some("chatcmpl-xyz789"));
                // request headers preserved from original request
                assert!(call
                    .request_headers
                    .iter()
                    .any(|(k, _)| k == "authorization"));
                assert!(call
                    .request_headers
                    .iter()
                    .any(|(k, _)| k == "content-type"));
                // response headers preserved
                assert!(call
                    .response_headers
                    .iter()
                    .any(|(k, v)| k == "x-request-id" && v == "req_test_456"));
            }
            _ => panic!("expected Complete event"),
        }
    }
}
