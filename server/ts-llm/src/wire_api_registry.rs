//! Registry of `WireApi` trait objects.
//!
//! Detection is two-pass:
//!   1. `classify_route` on every wire API. An `Accept` short-circuits and
//!      wins. Wire APIs that `Reject` are dropped from the candidate pool.
//!   2. If no wire API accepted and at least one returned `Unknown`, parse
//!      the request body once and call `matches_shape` on each remaining
//!      candidate in registry order; the first match wins.
//!
//! Registry order still matters when multiple wire APIs would `Accept` the
//! same request (e.g. `/v1/responses` must precede `/v1/chat/completions`)
//! or when shape-pass candidates overlap. Adding a wire API is still a
//! two-step change:
//!   1. Implement `WireApi` in a new module.
//!   2. Register it in `wire_apis::build_default_wire_api_registry()`.

use serde_json::Value;
use ts_protocol::model::HttpRequestData;

use crate::model::{RequestInfo, RouteVerdict, WireApi};

/// Result of a successful `WireApiRegistry::detect` call: the winning wire API
/// plus the `RequestInfo` extracted from the same single body parse that
/// detection did. Fusing the two avoids the historical double-parse (once in
/// shape matching, once in `extract_request`).
pub struct DetectionOutcome<'a> {
    pub wire_api: &'a dyn WireApi,
    pub request_info: RequestInfo,
}

/// Two-pass detection registry of `WireApi` implementations.
pub struct WireApiRegistry {
    wire_apis: Vec<Box<dyn WireApi>>,
}

impl WireApiRegistry {
    pub fn new() -> Self {
        Self {
            wire_apis: Vec::new(),
        }
    }

    pub fn with(mut self, wire_api: Box<dyn WireApi>) -> Self {
        self.wire_apis.push(wire_api);
        self
    }

    /// Run two-pass detection and extract request info in one pass.
    ///
    /// Returns the first wire API that accepts the request by route (or — if
    /// nobody accepts — the first Unknown candidate whose `matches_shape`
    /// returns true against the parsed JSON body), paired with the
    /// `RequestInfo` extracted from that same body. The body is parsed **at
    /// most once** per call, and only when a wire API actually matches —
    /// route-rejected non-LLM traffic never triggers a parse.
    pub fn detect(&self, req: &HttpRequestData) -> Option<DetectionOutcome<'_>> {
        // Pass 1: iterate, short-circuit on Accept, collect Unknowns.
        let mut deferred: Vec<&dyn WireApi> = Vec::new();
        for p in &self.wire_apis {
            match p.classify_route(req) {
                RouteVerdict::Accept => {
                    // Parse the body now for extract_request. `Value::Null`
                    // on non-JSON bodies preserves prior tolerance.
                    let body: Value =
                        serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                    let request_info = p.extract_request(req, &body);
                    return Some(DetectionOutcome {
                        wire_api: p.as_ref(),
                        request_info,
                    });
                }
                RouteVerdict::Reject => {}
                RouteVerdict::Unknown => deferred.push(p.as_ref()),
            }
        }
        if deferred.is_empty() {
            return None;
        }

        // Pass 2: only for POST + JSON bodies that parse.
        if req.method != "POST" || !is_json_content_type(req) {
            return None;
        }
        let body: Value = serde_json::from_slice(&req.body).ok()?;
        let winner = deferred.into_iter().find(|p| p.matches_shape(req, &body))?;
        let request_info = winner.extract_request(req, &body);
        Some(DetectionOutcome {
            wire_api: winner,
            request_info,
        })
    }

    /// Look up a previously-detected wire API by its stable `name()`.
    /// Used by `LlmProcessor::on_response` to route the response parsing
    /// to the same wire API that matched the request.
    pub fn find_by_name(&self, name: &str) -> Option<&dyn WireApi> {
        self.wire_apis
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.name() == name)
    }
}

/// True when Content-Type's media type is exactly `application/json`
/// (ignoring parameters like `; charset=utf-8`).
fn is_json_content_type(req: &HttpRequestData) -> bool {
    req.content_type()
        .map(|ct| {
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false)
}

impl Default for WireApiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_apis as wa;
    use crate::wire_apis::build_default_wire_api_registry;
    use bytes::Bytes;
    use ts_protocol::net::FlowKey;

    fn make_request(method: &str, uri: &str, headers: Vec<(&str, &str)>) -> HttpRequestData {
        make_request_body(method, uri, headers, Bytes::new())
    }

    fn make_request_body(
        method: &str,
        uri: &str,
        headers: Vec<(&str, &str)>,
        body: Bytes,
    ) -> HttpRequestData {
        HttpRequestData {
            flow_key: FlowKey::new(
                String::new(),
                "127.0.0.1".parse().unwrap(),
                1000,
                "127.0.0.1".parse().unwrap(),
                8080,
            ),
            client_addr: ("127.0.0.1".parse().unwrap(), 1000),
            server_addr: ("127.0.0.1".parse().unwrap(), 8080),
            method: method.to_string(),
            uri: uri.to_string(),
            version: 1,
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body,
            timestamp_us: 0,
        }
    }

    fn detect_name(reg: &WireApiRegistry, req: &HttpRequestData) -> Option<&'static str> {
        reg.detect(req).map(|o| o.wire_api.name())
    }

    fn json_post(uri: &str, extra_headers: Vec<(&str, &str)>, body: &str) -> HttpRequestData {
        let mut headers = vec![
            ("authorization", "Bearer sk-xxx"),
            ("content-type", "application/json"),
        ];
        headers.extend(extra_headers);
        make_request_body(
            "POST",
            uri,
            headers,
            Bytes::copy_from_slice(body.as_bytes()),
        )
    }

    #[test]
    fn detect_anthropic() {
        let reg = build_default_wire_api_registry();
        let req = make_request(
            "POST",
            "/v1/messages",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert_eq!(detect_name(&reg, &req), Some(wa::ANTHROPIC));
    }

    #[test]
    fn detect_openai_responses_before_chat() {
        // /v1/responses must win even though Chat Completions is also POST+Bearer.
        let reg = build_default_wire_api_registry();
        let req = make_request(
            "POST",
            "/v1/responses",
            vec![("authorization", "Bearer sk-xxx")],
        );
        assert_eq!(
            detect_name(&reg, &req),
            Some(wa::OPENAI_RESPONSES)
        );
    }

    #[test]
    fn detect_openai_chat() {
        let reg = build_default_wire_api_registry();
        let req = make_request(
            "POST",
            "/v1/chat/completions",
            vec![("authorization", "Bearer sk-xxx")],
        );
        assert_eq!(detect_name(&reg, &req), Some(wa::OPENAI_CHAT));
    }

    #[test]
    fn detect_none_for_unknown() {
        let reg = build_default_wire_api_registry();
        let req = make_request("GET", "/healthz", vec![]);
        assert!(reg.detect(&req).is_none());
    }

    #[test]
    fn detect_ignores_count_tokens_subpath() {
        let reg = build_default_wire_api_registry();
        let req = make_request(
            "POST",
            "/v1/messages/count_tokens",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert!(reg.detect(&req).is_none());
    }

    #[test]
    fn detect_openai_chat_via_gateway_prefix() {
        // pcap A case: /v1/llm/chat/completions. Route pass says Unknown
        // (suffix doesn't match /v1/chat/completions), shape pass picks it
        // up from model+messages.
        let reg = build_default_wire_api_registry();
        let req = json_post(
            "/v1/llm/chat/completions",
            vec![],
            r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(detect_name(&reg, &req), Some(wa::OPENAI_CHAT));
    }

    #[test]
    fn detect_openai_responses_via_gateway_prefix() {
        // Same story for the Responses API behind a prefix — `input` + no
        // `messages` is the distinguishing shape signal.
        let reg = build_default_wire_api_registry();
        let req = json_post(
            "/proxy/openai/v1/responses",
            vec![],
            r#"{"model":"gpt-4o","input":"Tell me a joke."}"#,
        );
        assert_eq!(
            detect_name(&reg, &req),
            Some(wa::OPENAI_RESPONSES)
        );
    }

    #[test]
    fn detect_anthropic_via_shape_when_system_present() {
        // No anthropic-version header, gateway prefix path; top-level
        // `system` is the exclusive Anthropic signal.
        let reg = build_default_wire_api_registry();
        let req = json_post(
            "/proxy/anthropic/v1/messages",
            vec![],
            r#"{"model":"claude-3","messages":[{"role":"user","content":"hi"}],"max_tokens":100,"system":"be concise"}"#,
        );
        assert_eq!(detect_name(&reg, &req), Some(wa::ANTHROPIC));
    }

    #[test]
    fn detect_anthropic_header_overrides_path() {
        // Header alone is enough — even an unusual path.
        let reg = build_default_wire_api_registry();
        let req = make_request(
            "POST",
            "/api/agents/run",
            vec![
                ("anthropic-version", "2023-06-01"),
                ("x-api-key", "sk-ant-abc"),
            ],
        );
        assert_eq!(detect_name(&reg, &req), Some(wa::ANTHROPIC));
    }

    #[test]
    fn detect_none_for_anthropic_without_version_on_custom_path() {
        // A request that looks like OpenAI Chat on a weird path and has no
        // Anthropic headers should fall through shape to OpenAI.
        let reg = build_default_wire_api_registry();
        let req = json_post(
            "/weird/endpoint",
            vec![],
            r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(detect_name(&reg, &req), Some(wa::OPENAI_CHAT));
    }

    #[test]
    fn detect_anthropic_header_beats_openai_path() {
        // Anomalous traffic: OpenAI Chat path + anthropic-version header.
        // Anthropic's header-only Accept rule wins over OpenAI's path
        // Accept because Anthropic comes first in the registry and the
        // `anthropic-version` header is an unambiguous positive signal.
        // OpenAI wire APIs also Reject this request (header-level negative)
        // so they couldn't win even without Anthropic's Accept.
        let reg = build_default_wire_api_registry();
        let req = json_post(
            "/v1/chat/completions",
            vec![("anthropic-version", "2023-06-01")],
            r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(detect_name(&reg, &req), Some(wa::ANTHROPIC));
    }

    #[test]
    fn detect_none_for_non_llm_json_post() {
        // Generic JSON business API on the same host.
        let reg = build_default_wire_api_registry();
        let req = json_post("/api/users", vec![], r#"{"name":"alice"}"#);
        assert!(reg.detect(&req).is_none());
    }

    #[test]
    fn detect_shape_pass_requires_json_content_type() {
        // POST with a plausible OpenAI-shaped body but non-JSON Content-Type
        // must not trip shape-pass.
        let reg = build_default_wire_api_registry();
        let req = make_request_body(
            "POST",
            "/weird/endpoint",
            vec![
                ("authorization", "Bearer sk-xxx"),
                ("content-type", "text/plain"),
            ],
            Bytes::from_static(br#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#),
        );
        assert!(reg.detect(&req).is_none());
    }

    #[test]
    fn detect_returns_request_info_on_accept() {
        // Route-accept path: detect must surface model + stream from the body
        // with exactly one parse internally.
        let reg = build_default_wire_api_registry();
        let req = json_post(
            "/v1/chat/completions",
            vec![],
            r#"{"model":"gpt-4","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let outcome = reg.detect(&req).expect("should accept");
        assert_eq!(outcome.wire_api.name(), wa::OPENAI_CHAT);
        assert_eq!(outcome.request_info.model, "gpt-4");
        assert!(outcome.request_info.is_stream);
    }

    #[test]
    fn detect_returns_request_info_on_shape_pass() {
        // Shape-pass path: gateway prefix, body carries the identifying shape.
        // request_info must also be populated here from the same single parse.
        let reg = build_default_wire_api_registry();
        let req = json_post(
            "/v1/llm/chat/completions",
            vec![],
            r#"{"model":"gpt-4o","stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let outcome = reg.detect(&req).expect("should accept via shape pass");
        assert_eq!(outcome.wire_api.name(), wa::OPENAI_CHAT);
        assert_eq!(outcome.request_info.model, "gpt-4o");
        assert!(!outcome.request_info.is_stream);
    }

    #[test]
    fn find_by_name_round_trips() {
        let reg = build_default_wire_api_registry();
        assert_eq!(
            reg.find_by_name(wa::ANTHROPIC).map(|p| p.name()),
            Some(wa::ANTHROPIC)
        );
        assert_eq!(
            reg.find_by_name(wa::OPENAI_RESPONSES).map(|p| p.name()),
            Some(wa::OPENAI_RESPONSES)
        );
        assert_eq!(
            reg.find_by_name(wa::OPENAI_CHAT).map(|p| p.name()),
            Some(wa::OPENAI_CHAT)
        );
        assert!(reg.find_by_name("gemini").is_none());
    }
}
