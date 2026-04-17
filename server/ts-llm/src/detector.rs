use ts_protocol::model::HttpRequestData;

use crate::model::ProviderFormat;

/// Detect the LLM provider from an HTTP request.
/// Returns None if the request doesn't match any known provider.
pub fn detect_provider(req: &HttpRequestData) -> Option<ProviderFormat> {
    // Anthropic first (has distinctive headers).
    if is_anthropic(req) {
        return Some(ProviderFormat::Anthropic);
    }
    // OpenAI Responses API before Chat Completions (more specific path).
    if is_openai_responses(req) {
        return Some(ProviderFormat::OpenAIResponses);
    }
    if is_openai(req) {
        return Some(ProviderFormat::OpenAI);
    }
    // Future: Azure, Gemini, Generic
    None
}

/// Detect Anthropic: POST to /v1/messages, with anthropic-version or x-api-key header.
fn is_anthropic(req: &HttpRequestData) -> bool {
    if req.method != "POST" {
        return false;
    }

    // Path check: strictly `/v1/messages` (ignoring query string). Sub-paths
    // like `/v1/messages/count_tokens` and `/v1/messages/batches` are auxiliary
    // endpoints that don't represent inference calls and must not be ingested
    // as LlmCalls.
    let path = req.uri.split('?').next().unwrap_or(&req.uri);
    if !path.ends_with("/v1/messages") {
        return false;
    }

    // Header check: anthropic-version or x-api-key starting with "sk-ant-"
    let has_anthropic_version = req.header("anthropic-version").is_some();
    let has_anthropic_key = req
        .header("x-api-key")
        .map(|v| v.starts_with("sk-ant-"))
        .unwrap_or(false);

    has_anthropic_version || has_anthropic_key
}

/// Check for Bearer auth header (common to OpenAI variants).
fn has_bearer_auth(req: &HttpRequestData) -> bool {
    req.header("authorization")
        .map(|v| v.starts_with("Bearer "))
        .unwrap_or(false)
}

/// Detect OpenAI Responses API: POST /v1/responses with Bearer auth.
fn is_openai_responses(req: &HttpRequestData) -> bool {
    if req.method != "POST" {
        return false;
    }
    let path = req.uri.split('?').next().unwrap_or(&req.uri);
    if !path.ends_with("/v1/responses") {
        return false;
    }
    has_bearer_auth(req)
}

/// Detect OpenAI Chat Completions: POST /v1/chat/completions with Bearer auth.
fn is_openai(req: &HttpRequestData) -> bool {
    if req.method != "POST" {
        return false;
    }
    let path = req.uri.split('?').next().unwrap_or(&req.uri);
    if !path.ends_with("/v1/chat/completions") {
        return false;
    }
    has_bearer_auth(req)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_detect_anthropic() {
        let req = make_request(
            "POST",
            "/v1/messages",
            vec![
                ("anthropic-version", "2023-06-01"),
                ("content-type", "application/json"),
            ],
        );
        assert_eq!(detect_provider(&req), Some(ProviderFormat::Anthropic));
    }

    #[test]
    fn test_detect_anthropic_with_query() {
        let req = make_request(
            "POST",
            "/v1/messages?beta=true",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert_eq!(detect_provider(&req), Some(ProviderFormat::Anthropic));
    }

    #[test]
    fn test_detect_anthropic_api_key() {
        let req = make_request(
            "POST",
            "/v1/messages",
            vec![("x-api-key", "sk-ant-api03-xxx")],
        );
        assert_eq!(detect_provider(&req), Some(ProviderFormat::Anthropic));
    }

    #[test]
    fn test_not_anthropic_count_tokens() {
        let req = make_request(
            "POST",
            "/v1/messages/count_tokens?beta=true",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert_eq!(detect_provider(&req), None);
    }

    #[test]
    fn test_not_anthropic_batches() {
        let req = make_request(
            "POST",
            "/v1/messages/batches",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert_eq!(detect_provider(&req), None);
    }

    #[test]
    fn test_not_anthropic_wrong_path() {
        let req = make_request(
            "POST",
            "/v1/chat/completions",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert_eq!(detect_provider(&req), None);
    }

    #[test]
    fn test_not_anthropic_get() {
        let req = make_request(
            "GET",
            "/v1/messages",
            vec![("anthropic-version", "2023-06-01")],
        );
        assert_eq!(detect_provider(&req), None);
    }

    #[test]
    fn test_detect_openai_responses() {
        let req = make_request(
            "POST",
            "/v1/responses",
            vec![
                ("authorization", "Bearer sk-xxx"),
                ("content-type", "application/json"),
            ],
        );
        assert_eq!(detect_provider(&req), Some(ProviderFormat::OpenAIResponses));
    }

    #[test]
    fn test_detect_openai_chat() {
        let req = make_request(
            "POST",
            "/v1/chat/completions",
            vec![("authorization", "Bearer sk-xxx")],
        );
        assert_eq!(detect_provider(&req), Some(ProviderFormat::OpenAI));
    }

    #[test]
    fn test_not_openai_no_auth() {
        let req = make_request(
            "POST",
            "/v1/responses",
            vec![("content-type", "application/json")],
        );
        assert_eq!(detect_provider(&req), None);
    }
}
