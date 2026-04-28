use std::sync::Arc;

use ts_common::internal_metrics::{Metric, MetricsWorker};
use ts_protocol::joiner::HttpJoinerEvent;
use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};
use uuid::Uuid;

use crate::model::{AgentCallInfo, ApiType, LlmCall, LlmCallStart, LlmEvent};
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
            HttpJoinerEvent::Request(req) => self.on_request(&req),
            HttpJoinerEvent::Exchange {
                id: _,
                request,
                response,
                sse_events,
            } => self.on_exchange(request, response, sse_events),
            HttpJoinerEvent::Heartbeat { ts, source_id } => {
                vec![LlmEvent::Heartbeat { ts, source_id }]
            }
        }
    }

    fn on_request(&mut self, req: &HttpRequestData) -> Vec<LlmEvent> {
        let Some(outcome) = self.wire_apis.detect(req) else {
            self.metrics.counter(Metric::WireIgnored).inc();
            return Vec::new();
        };
        self.metrics.counter(Metric::WireDetected).inc();
        vec![LlmEvent::Start(LlmCallStart {
            source_id: req.flow_key.source_id.clone(),
            wire_api: outcome.wire_api.name(),
            model: outcome.request_info.model,
            is_stream: outcome.request_info.is_stream,
            server_ip: req.server_addr.0,
            timestamp_us: req.timestamp_us,
        })]
    }

    fn on_exchange(
        &mut self,
        request: Arc<HttpRequestData>,
        response: Arc<HttpResponseData>,
        sse_events: Vec<SseEventData>,
    ) -> Vec<LlmEvent> {
        let Some(outcome) = self.wire_apis.detect(&request) else {
            // Already counted WireIgnored on Request; silent here.
            return Vec::new();
        };

        let extractor = outcome.wire_api;
        let req_info = outcome.request_info;

        // resp_info carries tokens / finish_reason / response_id / reconstructed body.
        let resp_info = if !sse_events.is_empty() {
            extractor.extract_sse(&sse_events)
        } else {
            extractor.extract_response(&response)
        };

        let model = resp_info.model.unwrap_or(req_info.model);

        let request_time = request.timestamp_us;
        let response_time = response.first_byte_timestamp_us;
        let complete_time = response.complete_timestamp_us;

        let ttft_ms = if response_time > request_time {
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

        let request_body = std::str::from_utf8(&request.body)
            .ok()
            .map(|s| s.to_string());

        let call = LlmCall {
            source_id: request.flow_key.source_id.clone(),
            id: Uuid::now_v7().to_string(),
            wire_api: extractor.name(),
            model,
            api_type: ApiType::Chat,
            request_time,
            response_time: Some(response_time),
            complete_time: Some(complete_time),
            request_path: request.uri.clone(),
            is_stream: req_info.is_stream,
            request_body,
            status_code: Some(response.status),
            finish_reason: resp_info.finish_reason,
            response_body: resp_info.response_body,
            input_tokens: resp_info.input_tokens,
            output_tokens: resp_info.output_tokens,
            total_tokens,
            cache_read_input_tokens: resp_info.cache_read_input_tokens,
            cache_creation_input_tokens: resp_info.cache_creation_input_tokens,
            ttft_ms,
            e2e_latency_ms,
            client_ip: request.client_addr.0,
            client_port: request.client_addr.1,
            server_ip: request.server_addr.0,
            server_port: request.server_addr.1,
            response_id: resp_info.response_id,
            request_headers: request.headers.clone(),
            response_headers: response.headers.clone(),
        };

        let agent = self.build_call_info(&call);
        vec![LlmEvent::Complete {
            call: Arc::new(call),
            agent,
        }]
    }

    fn build_call_info(&self, call: &LlmCall) -> Option<AgentCallInfo> {
        build_agent_call_info(call, &self.registry, &self.wire_apis, &self.metrics)
    }
}

/// Compute the full per-call classification and return it as an `AgentCallInfo`.
///
/// Returns `None` when no profile matches the call or the profile cannot
/// extract a `(session_id, turn_id?)` pair — those calls are non-agent traffic
/// and never enter turn assembly.
///
/// Public so tests in downstream crates (ts-turn) can construct call-info
/// records the same way the production pipeline does.
pub fn build_agent_call_info(
    call: &LlmCall,
    registry: &AgentProfileRegistry,
    wire_apis: &WireApiRegistry,
    metrics: &ts_common::internal_metrics::MetricsWorker,
) -> Option<AgentCallInfo> {
    let profile = registry.find(call)?;
    let is_generic = profile.name().starts_with("generic-");
    let Some(ids) = profile.extract_ids(call) else {
        if is_generic {
            metrics.counter(ts_common::internal_metrics::Metric::LlmGenericSessionIdUnsynth).inc();
        }
        return None;
    };
    if ids.tool_id_canonicalized {
        metrics.counter(ts_common::internal_metrics::Metric::LlmGenericToolIdCanonicalized).inc();
    }
    Some(AgentCallInfo {
        agent_kind: profile.name(),
        session_id: ids.session_id,
        subagent_name: profile.subagent(call),
        is_user_turn_start: profile.is_user_turn_start(call),
        is_turn_terminal: profile.is_turn_terminal(call, wire_apis),
        is_auxiliary: profile.is_auxiliary(call),
        user_input: profile.extract_user_input(call),
        assistant_text: profile.extract_assistant_text(call),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_apis as wa;
    use bytes::Bytes;
    use std::net::IpAddr;
    use std::sync::Arc;
    use ts_common::internal_metrics::MetricsSystem;
    use ts_protocol::model::{HttpRequestData, HttpResponseData, SseEventData};
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
                Metric::WireDetected,
                Metric::WireIgnored,
                Metric::LlmGenericToolIdCanonicalized,
                Metric::LlmGenericSessionIdUnsynth,
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
                (
                    "authorization".to_string(),
                    "Bearer sk-test-key".to_string(),
                ),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body_json.to_string()),
            timestamp_us: 1_000_000,
        }
    }

    /// Build a paired `(request, response)` suitable for constructing
    /// `HttpJoinerEvent::Exchange` in tests. The response can be mutated
    /// before being wrapped in `Arc` if a test wants to tweak headers.
    fn exchange_parts(
        req: HttpRequestData,
        resp_body: Bytes,
        is_sse: bool,
    ) -> (Arc<HttpRequestData>, HttpResponseData) {
        let flow_key = req.flow_key.clone();
        let client_addr = req.client_addr;
        let server_addr = req.server_addr;
        let timestamp_us = req.timestamp_us;
        let resp = HttpResponseData {
            flow_key,
            client_addr,
            server_addr,
            status: 200,
            version: 1,
            headers: if is_sse {
                vec![("content-type".to_string(), "text/event-stream".to_string())]
            } else {
                vec![("content-type".to_string(), "application/json".to_string())]
            },
            body: if is_sse { Bytes::new() } else { resp_body },
            first_byte_timestamp_us: timestamp_us + 100_000,
            complete_timestamp_us: timestamp_us + 200_000,
        };
        (Arc::new(req), resp)
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
    fn request_detects_and_emits_start() {
        use serde_json::json;
        let mut proc = LlmProcessor::new(wire_apis(), empty_registry(), test_metrics());
        let body = json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        let events = proc.process(HttpJoinerEvent::Request(Arc::new(openai_request(&body))));
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
    fn non_llm_request_bumps_ignored_no_event() {
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
        let events = proc.process(HttpJoinerEvent::Request(Arc::new(req)));
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
        let (request, response) = exchange_parts(req, Bytes::from(resp_body.to_string()), false);
        let events = proc.process(HttpJoinerEvent::Exchange {
            id: "xchg-1".to_string(),
            request,
            response: Arc::new(response),
            sse_events: vec![],
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert_eq!(call.wire_api, wa::OPENAI_CHAT);
                assert_eq!(call.request_path, "/v1/chat/completions");
                assert_eq!(call.finish_reason.as_deref(), Some("stop"));
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
        let (request, response) = exchange_parts(req, Bytes::new(), true);
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
        let events = proc.process(HttpJoinerEvent::Exchange {
            id: "xchg-2".to_string(),
            request,
            response: Arc::new(response),
            sse_events: sse,
        });
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert!(call.is_stream);
                assert_eq!(call.finish_reason.as_deref(), Some("stop"));
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
            source_id: "s1".into(),
        });
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::Heartbeat { ts: 1_234_567, .. }]
        ));
    }

    #[test]
    fn claude_cli_exchange_attaches_identity() {
        use crate::agents::build_default_registry;
        let mut proc = LlmProcessor::new(
            wire_apis(),
            Arc::new(build_default_registry()),
            test_metrics(),
        );

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
        let (request, response) = exchange_parts(req, Bytes::from(resp_body.to_string()), false);
        let events = proc.process(HttpJoinerEvent::Exchange {
            id: "xchg-3".to_string(),
            request,
            response: Arc::new(response),
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

    /// Build a minimal `LlmCall` with the given Claude-CLI request body. Other
    /// fields are filled with defaults — only the body and headers matter for
    /// the classification predicates.
    fn claude_call(body: &str, finish_reason: Option<&str>) -> LlmCall {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        LlmCall {
            source_id: String::new(),
            id: "c1".into(),
            wire_api: crate::wire_apis::ANTHROPIC,
            model: "claude-sonnet".into(),
            api_type: ApiType::Chat,
            request_time: 0,
            response_time: None,
            complete_time: None,
            request_path: "/v1/messages".into(),
            is_stream: false,
            request_body: Some(body.to_string()),
            status_code: Some(200),
            finish_reason: finish_reason.map(str::to_string),
            response_body: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            ttft_ms: None,
            e2e_latency_ms: None,
            client_ip: ip,
            client_port: 0,
            server_ip: ip,
            server_port: 0,
            response_id: None,
            request_headers: vec![
                ("user-agent".into(), "claude-cli/2.1.98".into()),
                ("x-claude-code-session-id".into(), "sess-1".into()),
                ("anthropic-version".into(), "2023-06-01".into()),
            ],
            response_headers: vec![],
        }
    }

    #[test]
    fn classification_main_agent_user_start_tool_use() {
        let registry = crate::agents::build_default_registry();
        let wa = crate::wire_apis::build_default_wire_api_registry();
        let body = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"tools":[{"name":"Agent"},{"name":"Bash"}]}"#;
        let call = claude_call(body, Some("tool_use"));
        let id = build_agent_call_info(&call, &registry, &wa, &test_metrics()).expect("call info");
        assert!(id.subagent_name.is_none());
        assert_eq!(id.is_user_turn_start, Some(true));
        assert!(!id.is_turn_terminal, "tool_use is not terminal");
        assert!(!id.is_auxiliary);
        assert!(id.user_input.is_some(), "user_input populated for fresh prompt");
    }

    #[test]
    fn classification_main_agent_continuation_terminal() {
        let registry = crate::agents::build_default_registry();
        let wa = crate::wire_apis::build_default_wire_api_registry();
        let body = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}],"tools":[{"name":"Agent"},{"name":"Bash"}]}"#;
        let call = claude_call(body, Some("end_turn"));
        let id = build_agent_call_info(&call, &registry, &wa, &test_metrics()).expect("call info");
        assert!(id.subagent_name.is_none());
        assert_eq!(id.is_user_turn_start, Some(false));
        assert!(id.is_turn_terminal, "end_turn is wire-terminal");
        assert!(!id.is_auxiliary);
    }

    #[test]
    fn classification_subagent_carries_raw_protocol_terminal() {
        // Sub-agent layering is orthogonal: `is_turn_terminal` reports the raw
        // protocol semantics (here, `end_turn` IS terminal at the wire level).
        // The sub-agent dispatch is identified separately via `subagent_name`,
        // and consumers (turn tracker) compose the two:
        //   `subagent_name.is_none() && is_turn_terminal` → main-agent terminal.
        let registry = crate::agents::build_default_registry();
        let wa = crate::wire_apis::build_default_wire_api_registry();
        let body = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"do research"}]}],"tools":[{"name":"Read"},{"name":"Grep"}]}"#;
        let call = claude_call(body, Some("end_turn"));
        let id = build_agent_call_info(&call, &registry, &wa, &test_metrics()).expect("call info");
        assert_eq!(id.subagent_name.as_deref(), Some("task"));
        assert_eq!(id.is_user_turn_start, Some(true));
        assert!(
            id.is_turn_terminal,
            "wire-level end_turn is terminal regardless of sub-agent layering"
        );
        // Composed view at the consumer:
        assert!(
            !(id.subagent_name.is_none() && id.is_turn_terminal),
            "sub-agent terminal must not close the parent's turn"
        );
    }

    #[test]
    fn classification_auxiliary_call() {
        // claude-cli session-title: tools field present and empty → auxiliary.
        let registry = crate::agents::build_default_registry();
        let wa = crate::wire_apis::build_default_wire_api_registry();
        let body = r#"{"messages":[{"role":"user","content":"generate title"}],"tools":[]}"#;
        let call = claude_call(body, Some("end_turn"));
        let id = build_agent_call_info(&call, &registry, &wa, &test_metrics()).expect("call info");
        assert!(id.is_auxiliary, "tools=[] flags auxiliary one-shot");
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
        let (request, mut response) =
            exchange_parts(req, Bytes::from(resp_body.to_string()), false);
        response
            .headers
            .push(("x-request-id".to_string(), "rid-42".to_string()));
        let events = proc.process(HttpJoinerEvent::Exchange {
            id: "xchg-4".to_string(),
            request,
            response: Arc::new(response),
            sse_events: vec![],
        });
        match &events[0] {
            LlmEvent::Complete { call, .. } => {
                assert!(call
                    .request_headers
                    .iter()
                    .any(|(k, _)| k == "authorization"));
                assert!(call
                    .response_headers
                    .iter()
                    .any(|(k, v)| k == "x-request-id" && v == "rid-42"));
            }
            _ => panic!("expected Complete"),
        }
    }
}
