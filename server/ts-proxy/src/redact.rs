//! API-key redaction applied to captured request headers BEFORE they
//! hand off to the storage pipeline. The forwarded request to the
//! upstream is unaffected — only what TokenScope persists is touched.
//!
//! Five header names are recognized (case-insensitive, as HTTP requires):
//! `authorization`, `x-api-key`, `anthropic-api-key`, `api-key`,
//! `x-goog-api-key`. For `authorization` we preserve the auth scheme
//! prefix (`Bearer `, `Basic `) so the wire-api detector can still tell
//! which auth style was in play after redaction.

use ts_common::config::RedactPolicy;

const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "anthropic-api-key",
    "api-key",
    "x-goog-api-key",
];

const HIDDEN_PLACEHOLDER: &str = "***redacted***";

/// Apply `policy` in place over `headers`. Header-name matching is
/// case-insensitive. Non-sensitive headers are left untouched.
pub fn redact_headers(headers: &mut Vec<(String, String)>, policy: RedactPolicy) {
    if matches!(policy, RedactPolicy::Show) {
        return;
    }
    for (name, value) in headers.iter_mut() {
        if !is_sensitive(name) {
            continue;
        }
        *value = redact_value(name, value, policy);
    }
}

fn is_sensitive(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|s| name.eq_ignore_ascii_case(s))
}

fn redact_value(name: &str, value: &str, policy: RedactPolicy) -> String {
    match policy {
        RedactPolicy::Show => value.to_string(),
        RedactPolicy::Hide => {
            // For Authorization, keep the auth scheme so wire-api
            // detection (which looks at `Bearer ` vs `sk-ant-`) still has
            // a usable signal post-redaction.
            if name.eq_ignore_ascii_case("authorization") {
                if let Some((scheme, _)) = value.split_once(' ') {
                    return format!("{scheme} {HIDDEN_PLACEHOLDER}");
                }
            }
            HIDDEN_PLACEHOLDER.to_string()
        }
        RedactPolicy::MaskMiddle => mask_middle(name, value),
    }
}

/// Keep the first 4 and last 4 characters of the secret portion,
/// replace the middle with `***`. For Authorization we split off the
/// scheme prefix first so `Bearer sk-abc…wxyz` is still legible.
fn mask_middle(name: &str, value: &str) -> String {
    if name.eq_ignore_ascii_case("authorization") {
        if let Some((scheme, rest)) = value.split_once(' ') {
            return format!("{scheme} {}", mask_secret(rest));
        }
    }
    mask_secret(value)
}

fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    let n = chars.len();
    // Strings short enough that 4+4 would overlap: collapse to ***.
    if n < 9 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(n - 4).collect();
    format!("{head}***{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn show_is_noop() {
        let mut headers = vec![
            h("Authorization", "Bearer sk-abcd1234efgh5678"),
            h("x-api-key", "secret123456789"),
            h("Content-Type", "application/json"),
        ];
        redact_headers(&mut headers, RedactPolicy::Show);
        assert_eq!(headers[0].1, "Bearer sk-abcd1234efgh5678");
        assert_eq!(headers[1].1, "secret123456789");
        assert_eq!(headers[2].1, "application/json");
    }

    #[test]
    fn mask_middle_keeps_scheme_and_edges() {
        let mut headers = vec![h("Authorization", "Bearer sk-abcd1234efgh5678wxyz")];
        redact_headers(&mut headers, RedactPolicy::MaskMiddle);
        assert_eq!(headers[0].1, "Bearer sk-a***wxyz");
    }

    #[test]
    fn mask_middle_x_api_key() {
        let mut headers = vec![h("x-api-key", "abcdefghijklmnop")];
        redact_headers(&mut headers, RedactPolicy::MaskMiddle);
        assert_eq!(headers[0].1, "abcd***mnop");
    }

    #[test]
    fn mask_middle_anthropic_key() {
        // Real-shape Anthropic key with sk-ant- prefix.
        let mut headers = vec![h("anthropic-api-key", "sk-ant-api03-secretpart-ending")];
        redact_headers(&mut headers, RedactPolicy::MaskMiddle);
        assert_eq!(headers[0].1, "sk-a***ding");
    }

    #[test]
    fn mask_middle_x_goog_api_key() {
        let mut headers = vec![h("x-goog-api-key", "AIzaSyDfooooooooooooooooooobar")];
        redact_headers(&mut headers, RedactPolicy::MaskMiddle);
        assert_eq!(headers[0].1, "AIza***obar");
    }

    #[test]
    fn mask_middle_short_secret_collapses() {
        let mut headers = vec![h("x-api-key", "short")];
        redact_headers(&mut headers, RedactPolicy::MaskMiddle);
        assert_eq!(headers[0].1, "***");
    }

    #[test]
    fn hide_keeps_authorization_scheme() {
        let mut headers = vec![
            h("Authorization", "Bearer sk-secret"),
            h("x-api-key", "another-secret"),
        ];
        redact_headers(&mut headers, RedactPolicy::Hide);
        assert_eq!(headers[0].1, "Bearer ***redacted***");
        assert_eq!(headers[1].1, "***redacted***");
    }

    #[test]
    fn hide_handles_authorization_without_space() {
        // Malformed but possible — no space after scheme.
        let mut headers = vec![h("Authorization", "BearersktokenABC123")];
        redact_headers(&mut headers, RedactPolicy::Hide);
        assert_eq!(headers[0].1, "***redacted***");
    }

    #[test]
    fn case_insensitive_header_name_match() {
        let mut headers = vec![
            h("AUTHORIZATION", "Bearer sk-abcd1234efgh"),
            h("X-Api-Key", "abcdefghijklmn"),
        ];
        redact_headers(&mut headers, RedactPolicy::MaskMiddle);
        assert_eq!(headers[0].1, "Bearer sk-a***efgh");
        assert_eq!(headers[1].1, "abcd***klmn");
    }

    #[test]
    fn untouched_headers_pass_through() {
        let mut headers = vec![
            h("Content-Type", "application/json"),
            h("User-Agent", "openai-python/1.0"),
            h("Accept", "*/*"),
        ];
        let snapshot = headers.clone();
        redact_headers(&mut headers, RedactPolicy::Hide);
        assert_eq!(headers, snapshot);
    }

    #[test]
    fn empty_value_collapses_under_mask() {
        let mut headers = vec![h("x-api-key", "")];
        redact_headers(&mut headers, RedactPolicy::MaskMiddle);
        assert_eq!(headers[0].1, "***");
    }
}
