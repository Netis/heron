//! Header-classifier helpers shared by the OpenAI Chat Completions and
//! Responses wire APIs. Kept deliberately tiny: only predicates that decide
//! "is this request even a candidate for OpenAI?" live here. Parse-time
//! field mapping is *not* shared — each wire API owns its own reader so a
//! new/changed field in one API cannot silently affect the other.

use ts_protocol::model::HttpRequestData;

/// True when the request carries an OpenAI-style `Authorization: Bearer ...`
/// header. All OpenAI-family wire APIs require this.
pub fn has_bearer_auth(req: &HttpRequestData) -> bool {
    req.header("authorization")
        .map(|v| v.starts_with("Bearer "))
        .unwrap_or(false)
}

/// Header-level signals that rule OpenAI wire APIs out — the request is
/// unambiguously Anthropic. `anthropic-version` is the strongest (Anthropic
/// SDKs always set it); `Bearer sk-ant-*` is weaker because gateways that
/// re-sign keys can erase it, but when present it's a reliable negative.
pub fn is_anthropic_request(req: &HttpRequestData) -> bool {
    if req.header("anthropic-version").is_some() {
        return true;
    }
    req.header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.starts_with("sk-ant-"))
        .unwrap_or(false)
}
