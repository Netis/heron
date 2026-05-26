//! Test-only helpers for constructing minimal valid values needed in
//! integration tests. Gated behind `#[cfg(any(test, feature = "test-support"))]`
//! in `lib.rs`.

use crate::model::{ApiType, LlmCall};
use std::net::IpAddr;

/// Construct the smallest valid `LlmCall` suitable for use in fixture tests
/// where only the parsed request body matters. All optional fields are `None`;
/// IPs are loopback; wire_api defaults to `"anthropic"`.
pub fn empty_llm_call() -> LlmCall {
    LlmCall {
        source_id: String::new(),
        id: "test".into(),
        wire_api: crate::wire_apis::ANTHROPIC,
        model: "test-model".into(),
        api_type: ApiType::Chat,
        request_time: 0,
        response_time: None,
        complete_time: None,
        request_path: "/v1/messages".into(),
        is_stream: false,
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
        client_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
        client_port: 0,
        server_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
        server_port: 0,
        response_id: None,
        request_headers: vec![],
        response_headers: vec![],
        is_agent_request: false,
        tool_surface: None,
        agent_topology: None,
        tool_call_count: 0,
        tool_names: vec![],
    }
}
