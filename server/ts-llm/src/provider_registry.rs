//! Registry of `Provider` trait objects.
//!
//! First-match semantics mirror `ProfileRegistry` (see `profile.rs`): order
//! determines priority. Callers build a registry once at pipeline
//! construction, wrap it in `Arc`, and clone into every `LlmProcessor` shard.
//!
//! Adding a new LLM provider is a two-step change:
//!   1. Implement `Provider` in a new module (the impl owns its own detection
//!      logic via `matches()`).
//!   2. Register it in `providers::build_default_provider_registry()`.

use ts_protocol::model::HttpRequestData;

use crate::model::Provider;

/// First-match registry of `Provider` implementations.
pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn with(mut self, provider: Box<dyn Provider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// Return the first provider whose `matches()` returns true for `req`.
    pub fn detect(&self, req: &HttpRequestData) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.matches(req))
    }

    /// Look up a previously-detected provider by its stable `name()`.
    /// Used by `LlmProcessor::on_response` to route the response parsing
    /// to the same provider that matched the request.
    pub fn find_by_name(&self, name: &str) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.name() == name)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_names as pn;
    use crate::providers::build_default_provider_registry;
    use bytes::Bytes;
    use ts_protocol::net::FlowKey;

    fn make_request(method: &str, uri: &str, headers: Vec<(&str, &str)>) -> HttpRequestData {
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
            body: Bytes::new(),
            timestamp_us: 0,
        }
    }

    #[test]
    fn detect_anthropic() {
        let reg = build_default_provider_registry();
        let req = make_request(
            "POST",
            "/v1/messages",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert_eq!(reg.detect(&req).map(|p| p.name()), Some(pn::ANTHROPIC));
    }

    #[test]
    fn detect_openai_responses_before_chat() {
        // /v1/responses must win even though Chat Completions is also POST+Bearer.
        let reg = build_default_provider_registry();
        let req = make_request(
            "POST",
            "/v1/responses",
            vec![("authorization", "Bearer sk-xxx")],
        );
        assert_eq!(
            reg.detect(&req).map(|p| p.name()),
            Some(pn::OPENAI_RESPONSES)
        );
    }

    #[test]
    fn detect_openai_chat() {
        let reg = build_default_provider_registry();
        let req = make_request(
            "POST",
            "/v1/chat/completions",
            vec![("authorization", "Bearer sk-xxx")],
        );
        assert_eq!(reg.detect(&req).map(|p| p.name()), Some(pn::OPENAI));
    }

    #[test]
    fn detect_none_for_unknown() {
        let reg = build_default_provider_registry();
        let req = make_request("GET", "/healthz", vec![]);
        assert!(reg.detect(&req).is_none());
    }

    #[test]
    fn detect_ignores_count_tokens_subpath() {
        let reg = build_default_provider_registry();
        let req = make_request(
            "POST",
            "/v1/messages/count_tokens",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert!(reg.detect(&req).is_none());
    }

    #[test]
    fn find_by_name_round_trips() {
        let reg = build_default_provider_registry();
        assert_eq!(
            reg.find_by_name(pn::ANTHROPIC).map(|p| p.name()),
            Some(pn::ANTHROPIC)
        );
        assert_eq!(
            reg.find_by_name(pn::OPENAI_RESPONSES).map(|p| p.name()),
            Some(pn::OPENAI_RESPONSES)
        );
        assert_eq!(
            reg.find_by_name(pn::OPENAI).map(|p| p.name()),
            Some(pn::OPENAI)
        );
        assert!(reg.find_by_name("gemini").is_none());
    }
}
