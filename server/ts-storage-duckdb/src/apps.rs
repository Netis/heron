//! Best-effort serving-software classification for the Services view.
//!
//! Wire-traffic doesn't carry an "I am vLLM" header. Each project
//! leaves slightly different fingerprints though, and combining a
//! handful of cheap signals — the `Server` response header, distinctive
//! native paths, LiteLLM's custom response headers, well-known
//! upstream hostnames — gives a usable label for the table at almost
//! zero query cost.
//!
//! What ships today:
//!
//! | App         | Primary signal                                                  |
//! |-------------|-----------------------------------------------------------------|
//! | `ollama`    | path `/api/chat` / `/api/generate` / `/api/tags` (native API)    |
//! | `llamacpp`  | path `/completion` / `/tokenize` / `/props` (non-`/v1/...`)      |
//! | `litellm`   | any `x-litellm-*` response header (LiteLLM stamps these)         |
//! | `openai`    | request `Host: api.openai.com`                                   |
//! | `anthropic` | request `Host: api.anthropic.com`                                |
//! | `openai-compat` | `Server: uvicorn` (vLLM and SGLang both — body sample needed |
//! |             | to disambiguate; tracked as a follow-up)                         |
//! | `None`      | nothing matches — show no badge                                  |
//!
//! A secondary signal — endpoint serves ≥ 3 distinct models — bumps an
//! otherwise-`openai-compat` row to `litellm`. The user's production
//! data has `127.0.0.1:4000` serving five different models from one
//! port; that's a LiteLLM proxy. Real vLLM occasionally hosts multiple
//! LoRAs at one endpoint, so this is a *hint*, not a definitive
//! signal — we only apply it as a tiebreaker.

use std::collections::HashMap;

/// Classify one Services-page row from the cheap signals we can pull
/// out of the SQL aggregate. Returns the app label (lowercase, stable
/// — surfaces straight in the API JSON / UI badges).
pub(crate) fn classify_app(
    server_header: Option<&str>,
    raw_response_headers_json: Option<&str>,
    raw_request_headers_json: Option<&str>,
    request_paths: &[String],
    models: &[String],
) -> Option<String> {
    // -- Native non-OpenAI surface, very high confidence. --
    if request_paths.iter().any(|p| {
        let p = p.to_ascii_lowercase();
        p.starts_with("/api/chat")
            || p.starts_with("/api/generate")
            || p.starts_with("/api/tags")
            || p.starts_with("/api/show")
            || p == "/api/version"
    }) {
        return Some("ollama".to_string());
    }
    if request_paths.iter().any(|p| {
        // llama.cpp's native completion paths sit at the root, not
        // under /v1/. `/v1/...` paths exist too (OpenAI-compat) but
        // by themselves don't distinguish llama.cpp from vLLM.
        matches!(
            p.as_str(),
            "/completion" | "/tokenize" | "/detokenize" | "/props"
        )
    }) {
        return Some("llamacpp".to_string());
    }

    // -- LiteLLM stamps its own response headers. This is a one-shot
    // -- substring check on the JSON blob — cheap enough to run on
    // -- every row, even if it's mildly indirect.
    if let Some(json) = raw_response_headers_json {
        let lower = json.to_ascii_lowercase();
        if lower.contains("x-litellm-") || lower.contains("\"server\":\"litellm") {
            return Some("litellm".to_string());
        }
    }

    // -- Upstream hostnames from the request side. Captured request_headers
    // -- carries `Host:` even when we're routed through a sniffer that
    // -- doesn't terminate TLS — the parser stashes the inner host.
    if let Some(host) = extract_host_header(raw_request_headers_json) {
        let host = host.to_ascii_lowercase();
        if host == "api.openai.com" || host.ends_with(".api.openai.com") {
            return Some("openai".to_string());
        }
        if host == "api.anthropic.com" || host.ends_with(".api.anthropic.com") {
            return Some("anthropic".to_string());
        }
        if host == "generativelanguage.googleapis.com" {
            return Some("gemini".to_string());
        }
    }

    // -- Server header. uvicorn matches BOTH vLLM and SGLang; we ship
    // -- them as `openai-compat` today and the (in-flight) body-sample
    // -- follow-up will split them.
    let mut base_label: Option<&'static str> = None;
    if let Some(sh) = server_header {
        let l = sh.to_ascii_lowercase();
        if l.contains("ollama") {
            return Some("ollama".to_string());
        }
        if l.contains("uvicorn") || l.contains("hypercorn") {
            base_label = Some("openai-compat");
        }
        if l.starts_with("litellm") {
            return Some("litellm".to_string());
        }
    }

    // -- Multi-model tiebreaker for LiteLLM. Real LiteLLM proxies
    // -- routinely host 5-20 models at one port; vLLM occasionally
    // -- hosts multiple LoRAs but rarely > 2. Threshold of 3 keeps
    // -- the false-positive risk small.
    if base_label == Some("openai-compat") && models.len() >= 3 {
        return Some("litellm".to_string());
    }

    base_label.map(String::from)
}

/// Pull the `Server` header value out of a serialized `Vec<(String,
/// String)>` JSON blob (the same format used by the rest of the
/// codebase — see `ts_protocol::model::HttpResponseData::headers`).
///
/// Returns `None` if the blob is missing, doesn't parse, or has no
/// matching header.
pub(crate) fn extract_server_header(raw_response_headers_json: Option<&str>) -> Option<String> {
    let raw = raw_response_headers_json?;
    let parsed: Vec<(String, String)> = serde_json::from_str(raw).ok()?;
    parsed
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("server"))
        .map(|(_, v)| v)
}

/// Like `extract_server_header` but for request `Host`. We use it to
/// distinguish proxy traffic terminating at e.g. `api.openai.com`
/// (where the `Host` header is the give-away) from same-IP
/// self-hosted services.
fn extract_host_header(raw_request_headers_json: Option<&str>) -> Option<String> {
    let raw = raw_request_headers_json?;
    let parsed: Vec<(String, String)> = serde_json::from_str(raw).ok()?;
    parsed
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v)
}

#[allow(dead_code)] // exposed for tests in this module
fn _headermap(items: &[(&str, &str)]) -> String {
    let v: Vec<(String, String)> = items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    serde_json::to_string(&v).unwrap()
}

#[allow(dead_code)] // exposed for tests in this module
fn _empty_map() -> HashMap<String, String> {
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(items: &[(&str, &str)]) -> String {
        _headermap(items)
    }

    #[test]
    fn ollama_via_native_path() {
        let app = classify_app(None, None, None, &["/api/chat".to_string()], &[]);
        assert_eq!(app.as_deref(), Some("ollama"));
    }

    #[test]
    fn ollama_via_server_header_with_oai_path() {
        // Ollama exposes `/v1/chat/completions` too in compat mode —
        // path alone won't help, but a `Server: ollama/x.y` header
        // settles it.
        let h = hdrs(&[("server", "ollama/0.1.45")]);
        let app = classify_app(
            Some("ollama/0.1.45"),
            Some(&h),
            None,
            &["/v1/chat/completions".to_string()],
            &["llama3".to_string()],
        );
        assert_eq!(app.as_deref(), Some("ollama"));
    }

    #[test]
    fn llamacpp_via_completion_path() {
        let app = classify_app(None, None, None, &["/completion".to_string()], &[]);
        assert_eq!(app.as_deref(), Some("llamacpp"));
    }

    #[test]
    fn litellm_via_response_header() {
        let h = hdrs(&[
            ("server", "uvicorn"),
            ("x-litellm-call-id", "abc123"),
        ]);
        let app = classify_app(
            Some("uvicorn"),
            Some(&h),
            None,
            &["/v1/chat/completions".to_string()],
            &["gpt-4o".to_string()],
        );
        assert_eq!(app.as_deref(), Some("litellm"));
    }

    #[test]
    fn litellm_via_multi_model_tiebreaker() {
        // Three+ distinct models on one uvicorn endpoint → LiteLLM
        // (real-world signal from wuneng's 127.0.0.1:4000).
        let app = classify_app(
            Some("uvicorn"),
            Some(&hdrs(&[("server", "uvicorn")])),
            None,
            &["/v1/chat/completions".to_string()],
            &[
                "qwen3-embed-8b".to_string(),
                "glm5".to_string(),
                "qwen35-27b".to_string(),
            ],
        );
        assert_eq!(app.as_deref(), Some("litellm"));
    }

    #[test]
    fn openai_compat_for_single_model_uvicorn() {
        // 1 model, uvicorn, no LiteLLM signals → can't tell vLLM from
        // SGLang yet; bucket as openai-compat.
        let app = classify_app(
            Some("uvicorn"),
            Some(&hdrs(&[("server", "uvicorn")])),
            None,
            &["/v1/chat/completions".to_string()],
            &["qwen35-27b".to_string()],
        );
        assert_eq!(app.as_deref(), Some("openai-compat"));
    }

    #[test]
    fn openai_via_request_host() {
        let req_h = hdrs(&[("host", "api.openai.com")]);
        let app = classify_app(
            None,
            None,
            Some(&req_h),
            &["/v1/chat/completions".to_string()],
            &["gpt-4".to_string()],
        );
        assert_eq!(app.as_deref(), Some("openai"));
    }

    #[test]
    fn anthropic_via_request_host() {
        let req_h = hdrs(&[("host", "api.anthropic.com")]);
        let app = classify_app(
            None,
            None,
            Some(&req_h),
            &["/v1/messages".to_string()],
            &["claude-3-5-sonnet".to_string()],
        );
        assert_eq!(app.as_deref(), Some("anthropic"));
    }

    #[test]
    fn unknown_when_no_signals() {
        let app = classify_app(None, None, None, &[], &[]);
        assert!(app.is_none());
    }

    #[test]
    fn ollama_path_wins_over_uvicorn_server() {
        // Edge case: an Ollama install fronted by an unrelated uvicorn
        // tool that proxies to /api/chat. Path matches first.
        let app = classify_app(
            Some("uvicorn"),
            Some(&hdrs(&[("server", "uvicorn")])),
            None,
            &["/api/chat".to_string()],
            &["llama3".to_string()],
        );
        assert_eq!(app.as_deref(), Some("ollama"));
    }

    #[test]
    fn extract_server_header_finds_match() {
        let h = hdrs(&[("content-type", "application/json"), ("Server", "uvicorn")]);
        assert_eq!(
            extract_server_header(Some(&h)).as_deref(),
            Some("uvicorn")
        );
    }

    #[test]
    fn extract_server_header_handles_missing() {
        let h = hdrs(&[("content-type", "application/json")]);
        assert!(extract_server_header(Some(&h)).is_none());
    }
}
