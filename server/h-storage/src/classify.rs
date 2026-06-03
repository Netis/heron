//! Best-effort serving-software classification for the Services view.
//!
//! Wire-traffic doesn't carry an "I am vLLM" header. Each project
//! leaves slightly different fingerprints though, and combining a
//! handful of cheap signals — `Server` response header, distinctive
//! native paths, LiteLLM's custom response headers, well-known
//! upstream hostnames, distinct `finish_reason` values, plus small
//! request- and response-body samples — gives a definitive label for
//! almost every endpoint at near-zero query cost.
//!
//! Backend-neutral: pure functions over the cheap signals each backend
//! pulls from its own `query_services` aggregate, so DuckDB / ClickHouse
//! share one copy.
//!
//! Signal table (highest confidence first):
//!
//! | App         | Primary signal                                                  |
//! |-------------|-----------------------------------------------------------------|
//! | `ollama`    | path `/api/chat` / `/api/generate` / `/api/tags` (native API)    |
//! | `llamacpp`  | path `/completion` / `/tokenize` / `/props` (non-`/v1/...`)      |
//! | `sglang`    | path `/generate` / `/health_generate` / `/get_server_info` /     |
//! |             | `/flush_cache` / `/encode` (SGLang's own surface alongside       |
//! |             | OpenAI-compat)                                                   |
//! | `vllm`      | path `/version` / `/v1/score` (vLLM-specific endpoints)          |
//! | `litellm`   | any `x-litellm-*` response header (LiteLLM stamps these)         |
//! | `openai`    | request `Host: api.openai.com`                                   |
//! | `anthropic` | request `Host: api.anthropic.com`                                |
//! | `sglang`    | `finish_reason` ∈ {`matched_stop`, `matched_eos`, `stop_str`}    |
//! |             | (SGLang's stop-condition machinery; vLLM doesn't have these)     |
//! | `vllm`      | response body has `"id":"chatcmpl-tool-` (vLLM tool_call_id) OR  |
//! |             | `"system_fingerprint":"fp_…"` (vLLM only)                        |
//! | `vllm`      | request body contains `chatcmpl-tool-` — agentic flows echo the  |
//! |             | server's prior tool_call_id back in assistant history; works     |
//! |             | even when responses are SSE-streamed (response_body is NULL)     |
//! | `sglang`    | uvicorn endpoint, model name starts with `glm` or `deepseek`     |
//! |             | (SGLang is the reference deployment for those families)         |
//! | `vllm`      | uvicorn fallback — vLLM is by far the more common openai-compat  |
//! |             | server, better default than `unknown`                            |
//! | `None`      | nothing matches (no Server header, no distinctive paths)         |

/// Classify one Services-page row from the cheap signals we can pull
/// out of the SQL aggregate. Returns the app label (lowercase, stable
/// — surfaces straight in the API JSON / UI badges).
#[allow(clippy::too_many_arguments)]
pub fn classify_app(
    server_header: Option<&str>,
    raw_response_headers_json: Option<&str>,
    raw_request_headers_json: Option<&str>,
    request_paths: &[String],
    finish_reasons: &[String],
    models: &[String],
    sample_request_body: Option<&str>,
    sample_response_body: Option<&str>,
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

    // -- SGLang-specific paths. Even when serving OpenAI-compat,
    // -- SGLang exposes /generate / /health_generate / /flush_cache /
    // -- /get_server_info / /encode at the same port.
    if request_paths.iter().any(|p| {
        matches!(
            p.as_str(),
            "/generate"
                | "/health_generate"
                | "/get_server_info"
                | "/flush_cache"
                | "/encode"
                | "/start_profile"
                | "/stop_profile"
        )
    }) {
        return Some("sglang".to_string());
    }

    // -- vLLM-specific paths. /version returns vLLM's version JSON.
    // -- /v1/score is reranking, vLLM-only.
    if request_paths
        .iter()
        .any(|p| p == "/version" || p.starts_with("/v1/score"))
    {
        return Some("vllm".to_string());
    }

    // -- LiteLLM stamps its own response headers.
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

    // -- SGLang-exclusive finish_reasons. Captured for both streaming
    // -- and non-streaming calls (extracted from SSE for the former).
    // -- `matched_stop` / `matched_eos` / `stop_str` come out of SGLang's
    // -- stop-condition machinery and vLLM doesn't have them.
    if finish_reasons.iter().any(|fr| {
        let l = fr.to_ascii_lowercase();
        l == "matched_stop" || l == "matched_eos" || l == "stop_str"
    }) {
        return Some("sglang".to_string());
    }

    // -- vLLM signatures in the response body sample. Two patterns:
    // --   1. tool_call_id `"id":"chatcmpl-tool-<hex>"` — vLLM's
    // --      tool parser uses this format; SGLang follows OpenAI's
    // --      `call_<hex>`.
    // --   2. `system_fingerprint":"fp_<hex>"` — vLLM emits this
    // --      field (mirroring OpenAI's); SGLang leaves it null.
    let vllm_in_response_body = sample_response_body
        .map(|body| {
            body.contains("\"id\":\"chatcmpl-tool-")
                || body.contains("\"id\": \"chatcmpl-tool-")
                || body.contains("\"system_fingerprint\":\"fp_")
                || body.contains("\"system_fingerprint\": \"fp_")
        })
        .unwrap_or(false);
    if vllm_in_response_body {
        return Some("vllm".to_string());
    }

    // -- vLLM signature in the request body. Agentic flows include
    // -- prior `assistant.tool_calls[].id` values in their message
    // -- history, and vLLM-generated tool_call_ids carry the
    // -- `chatcmpl-tool-` prefix. Works even when the response is
    // -- SSE-streamed (response_body is NULL).
    if let Some(body) = sample_request_body {
        if body.contains("chatcmpl-tool-") {
            return Some("vllm".to_string());
        }
    }

    // -- Server header check. uvicorn matches BOTH vLLM and SGLang;
    // -- the multi-model tiebreaker handles LiteLLM, the model-name
    // -- heuristic handles SGLang's GLM/DeepSeek family deployments,
    // -- and everything else falls through to vLLM.
    let is_uvicorn = server_header
        .map(|sh| {
            let l = sh.to_ascii_lowercase();
            l.contains("uvicorn") || l.contains("hypercorn")
        })
        .unwrap_or(false);
    let has_litellm_server = server_header
        .map(|sh| sh.to_ascii_lowercase().starts_with("litellm"))
        .unwrap_or(false);
    let has_ollama_server = server_header
        .map(|sh| sh.to_ascii_lowercase().contains("ollama"))
        .unwrap_or(false);

    if has_ollama_server {
        return Some("ollama".to_string());
    }
    if has_litellm_server {
        return Some("litellm".to_string());
    }

    if is_uvicorn {
        // The "uvicorn + ≥3 distinct models → litellm" tiebreaker
        // used to live here, but it was window-width-sensitive: at
        // 7 d a vLLM serving Qwen3.5-35B accumulates 3-4 stray model
        // names (`text-embedding-ada-002`, `test`, …) from rogue
        // clients and gets misclassified. The signal isn't strong
        // enough to overrule the body / path / header evidence we
        // already weighed above. Removed.

        // Model-name heuristic. SGLang is the reference deployment for
        // the Zhipu GLM family and the DeepSeek family in production;
        // both are non-trivial to run on vLLM (custom kernels, MLA,
        // etc.). Matches the user's prod corpus exactly.
        let lower_models: Vec<String> = models.iter().map(|m| m.to_ascii_lowercase()).collect();
        let smells_like_sglang = lower_models.iter().any(|m| {
            m.starts_with("glm")
                || m.contains("/glm")
                || m.starts_with("deepseek")
                || m.contains("/deepseek")
        });
        if smells_like_sglang {
            return Some("sglang".to_string());
        }

        return Some("vllm".to_string());
    }

    None
}

/// Pull the `Server` header value out of a serialized `Vec<(String,
/// String)>` JSON blob (the same format used by the rest of the
/// codebase — see `h_protocol::model::HttpResponseData::headers`).
///
/// Returns `None` if the blob is missing, doesn't parse, or has no
/// matching header.
pub fn extract_server_header(raw_response_headers_json: Option<&str>) -> Option<String> {
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

#[cfg(test)]
fn _headermap(items: &[(&str, &str)]) -> String {
    let v: Vec<(String, String)> = items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    serde_json::to_string(&v).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(items: &[(&str, &str)]) -> String {
        _headermap(items)
    }

    /// Empty everything — shared test default. Use overrides to inject
    /// the specific signal each case is exercising.
    struct C {
        server_header: Option<String>,
        raw_response_headers_json: Option<String>,
        raw_request_headers_json: Option<String>,
        request_paths: Vec<String>,
        finish_reasons: Vec<String>,
        models: Vec<String>,
        sample_request_body: Option<String>,
        sample_response_body: Option<String>,
    }
    impl Default for C {
        fn default() -> Self {
            Self {
                server_header: None,
                raw_response_headers_json: None,
                raw_request_headers_json: None,
                request_paths: vec![],
                finish_reasons: vec![],
                models: vec![],
                sample_request_body: None,
                sample_response_body: None,
            }
        }
    }
    fn run(c: &C) -> Option<String> {
        classify_app(
            c.server_header.as_deref(),
            c.raw_response_headers_json.as_deref(),
            c.raw_request_headers_json.as_deref(),
            &c.request_paths,
            &c.finish_reasons,
            &c.models,
            c.sample_request_body.as_deref(),
            c.sample_response_body.as_deref(),
        )
    }

    #[test]
    fn ollama_via_native_path() {
        let c = C {
            request_paths: vec!["/api/chat".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("ollama"));
    }

    #[test]
    fn ollama_via_server_header_with_oai_path() {
        let c = C {
            server_header: Some("ollama/0.1.45".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "ollama/0.1.45")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["llama3".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("ollama"));
    }

    #[test]
    fn llamacpp_via_completion_path() {
        let c = C {
            request_paths: vec!["/completion".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("llamacpp"));
    }

    #[test]
    fn sglang_via_native_path() {
        // SGLang exposes /generate alongside its OpenAI-compat surface.
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into(), "/generate".into()],
            models: vec!["llama-3-8b".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("sglang"));
    }

    #[test]
    fn sglang_via_health_generate_path() {
        let c = C {
            request_paths: vec!["/health_generate".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("sglang"));
    }

    #[test]
    fn sglang_via_matched_stop_finish_reason() {
        // Streaming endpoint — no response body, but SSE-extracted
        // finish_reason `matched_stop` exposes SGLang.
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            finish_reasons: vec!["stop".into(), "matched_stop".into()],
            models: vec!["mystery-model".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("sglang"));
    }

    #[test]
    fn vllm_via_version_path() {
        let c = C {
            request_paths: vec!["/v1/chat/completions".into(), "/version".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("vllm"));
    }

    #[test]
    fn vllm_via_response_body_tool_call_id() {
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["qwen35-27b".into()],
            sample_response_body: Some(
                r#"{"id":"chatcmpl-abc","choices":[{"message":{"tool_calls":[{"id":"chatcmpl-tool-deadbeef","type":"function"}]}}]}"#.into(),
            ),
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("vllm"));
    }

    #[test]
    fn vllm_via_response_body_system_fingerprint() {
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["qwen35-27b".into()],
            sample_response_body: Some(
                r#"{"id":"chatcmpl-x","system_fingerprint":"fp_abc123","choices":[]}"#.into(),
            ),
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("vllm"));
    }

    #[test]
    fn vllm_via_request_body_tool_history() {
        // Streaming-only endpoint — response_body is null. Agentic
        // round N+1 sends prior assistant.tool_calls history back to
        // the server, and vLLM's tool_call_ids carry `chatcmpl-tool-`.
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["mystery-model".into()],
            sample_request_body: Some(
                r#"{"messages":[{"role":"assistant","tool_calls":[{"id":"chatcmpl-tool-deadbeef","type":"function","function":{"name":"x"}}]}]}"#.into(),
            ),
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("vllm"));
    }

    #[test]
    fn litellm_via_response_header() {
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[
                ("server", "uvicorn"),
                ("x-litellm-call-id", "abc123"),
            ])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["gpt-4o".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("litellm"));
    }

    #[test]
    fn sglang_via_glm_model_heuristic() {
        // Single GLM model on uvicorn, no LiteLLM signal, no
        // discriminating finish_reason. Falls back to the model-name
        // heuristic — Zhipu's reference deployment is SGLang.
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["GLM-5.1".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("sglang"));
    }

    #[test]
    fn sglang_via_deepseek_model_heuristic() {
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["deepseek-v3".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("sglang"));
    }

    #[test]
    fn vllm_default_for_single_model_uvicorn() {
        // 1 model, uvicorn, no LiteLLM signals, no SGLang signals →
        // pick the more common openai-compat server.
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["qwen35-27b".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("vllm"));
    }

    #[test]
    fn openai_via_request_host() {
        let c = C {
            raw_request_headers_json: Some(hdrs(&[("host", "api.openai.com")])),
            request_paths: vec!["/v1/chat/completions".into()],
            models: vec!["gpt-4".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("openai"));
    }

    #[test]
    fn anthropic_via_request_host() {
        let c = C {
            raw_request_headers_json: Some(hdrs(&[("host", "api.anthropic.com")])),
            request_paths: vec!["/v1/messages".into()],
            models: vec!["claude-3-5-sonnet".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("anthropic"));
    }

    #[test]
    fn unknown_when_no_signals() {
        let c = C::default();
        assert!(run(&c).is_none());
    }

    #[test]
    fn ollama_path_wins_over_uvicorn_server() {
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/api/chat".into()],
            models: vec!["llama3".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("ollama"));
    }

    #[test]
    fn sglang_finish_reason_beats_glm_heuristic_consistency() {
        // SGLang finish_reason AND GLM model — should still be SGLang.
        let c = C {
            server_header: Some("uvicorn".into()),
            raw_response_headers_json: Some(hdrs(&[("server", "uvicorn")])),
            request_paths: vec!["/v1/chat/completions".into()],
            finish_reasons: vec!["matched_stop".into()],
            models: vec!["GLM-5.1".into()],
            ..Default::default()
        };
        assert_eq!(run(&c).as_deref(), Some("sglang"));
    }

    #[test]
    fn extract_server_header_finds_match() {
        let h = hdrs(&[("content-type", "application/json"), ("Server", "uvicorn")]);
        assert_eq!(extract_server_header(Some(&h)).as_deref(), Some("uvicorn"));
    }

    #[test]
    fn extract_server_header_handles_missing() {
        let h = hdrs(&[("content-type", "application/json")]);
        assert!(extract_server_header(Some(&h)).is_none());
    }
}
