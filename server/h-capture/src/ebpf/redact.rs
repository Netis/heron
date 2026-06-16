//! Edge redaction — equal-length, best-effort scrubbing of secrets from the SSL
//! plaintext buffer before it is synthesized into frames and shipped to a
//! central collector.
//!
//! Why here, and why best-effort: the distributed topology splits at
//! `RawPacket`, so the probe ships plaintext (post-TLS) over the network. The
//! real security boundary is mTLS + network isolation (`tls.rs`); this is a
//! defence-in-depth scrubber that keeps the most obvious secrets — API keys in
//! `Authorization` / `x-api-key` headers, `sk-…` tokens in bodies — out of the
//! shipped bytes. It runs on the **contiguous uprobe buffer** (one `SSL_write`
//! is up to ~32 KiB, and a request's headers almost always land in the first
//! chunk) *before* `FlowSynthesizer` slices it into TCP segments, so a header
//! that would straddle a segment boundary is still scrubbed in one piece.
//!
//! Equal-length: every match is overwritten byte-for-byte with a mask byte, so
//! `Content-Length` and the synthesizer's absolute stream offsets are unchanged
//! — the scrubbed buffer is a drop-in for the original.
//!
//! Known limitation (documented, not a bug): a secret split across two uprobe
//! chunks can be missed (the header's value continues into a chunk this pass
//! never sees). Redaction is best-effort and does not replace mTLS / isolation.

/// Default header names whose values are masked (lowercased; matched
/// case-insensitively). These carry the API credentials for every supported
/// provider (`Authorization: Bearer …`, `x-api-key: …`, Azure's `api-key: …`)
/// plus session `Cookie`s.
pub const DEFAULT_HEADERS: &[&str] = &["authorization", "x-api-key", "api-key", "cookie"];

/// Default secret-token prefixes scrubbed anywhere in the buffer (e.g. a key
/// embedded in a JSON body, outside any header). The run of token characters
/// following the prefix is masked; the prefix itself is left visible so an
/// operator can see *that* a secret was redacted.
pub const DEFAULT_TOKEN_PREFIXES: &[&str] = &["sk-", "Bearer "];

/// Byte used to overwrite redacted spans.
const MASK: u8 = b'*';

#[inline]
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

#[inline]
fn eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// An equal-length secret scrubber configured with header names and token
/// prefixes. Cheap to construct; `redact` does no allocation.
#[derive(Debug, Clone)]
pub struct Redactor {
    /// Lowercased header names (no trailing colon).
    headers: Vec<Vec<u8>>,
    /// Token prefixes whose following token run is masked.
    token_prefixes: Vec<Vec<u8>>,
    mask: u8,
}

impl Redactor {
    /// Build a redactor. Header names are lowercased for case-insensitive
    /// matching; token prefixes are matched case-insensitively too.
    pub fn new<H, P>(headers: H, token_prefixes: P) -> Self
    where
        H: IntoIterator,
        H::Item: AsRef<str>,
        P: IntoIterator,
        P::Item: AsRef<str>,
    {
        Self {
            headers: headers
                .into_iter()
                .map(|h| h.as_ref().to_ascii_lowercase().into_bytes())
                .filter(|h| !h.is_empty())
                .collect(),
            token_prefixes: token_prefixes
                .into_iter()
                .map(|p| p.as_ref().as_bytes().to_vec())
                .filter(|p| !p.is_empty())
                .collect(),
            mask: MASK,
        }
    }

    /// A redactor with the built-in default header/token rules.
    pub fn with_defaults() -> Self {
        Self::new(
            DEFAULT_HEADERS.iter().copied(),
            DEFAULT_TOKEN_PREFIXES.iter().copied(),
        )
    }

    /// True if this redactor would do nothing (no rules) — lets callers skip the
    /// per-event buffer copy entirely.
    pub fn is_noop(&self) -> bool {
        self.headers.is_empty() && self.token_prefixes.is_empty()
    }

    /// Scrub `buf` in place. Length is never changed: matched spans are
    /// overwritten byte-for-byte with the mask byte.
    pub fn redact(&self, buf: &mut [u8]) {
        if !self.headers.is_empty() {
            self.redact_headers(buf);
        }
        if !self.token_prefixes.is_empty() {
            self.redact_tokens(buf);
        }
    }

    /// Mask the value of any configured header. Lines are `\n`-delimited; a
    /// header is `Name:` (case-insensitive) at a line start, and everything from
    /// the first non-space after the colon to the line end (excluding a trailing
    /// `\r`) is masked.
    fn redact_headers(&self, buf: &mut [u8]) {
        let mut ls = 0usize; // line start
        while ls < buf.len() {
            let le = buf[ls..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|off| ls + off)
                .unwrap_or(buf.len());

            for name in &self.headers {
                let after = ls + name.len();
                if after < le && buf[after] == b':' && eq_ignore_ascii_case(&buf[ls..after], name) {
                    // Value starts after the colon; skip leading spaces/tabs.
                    let mut vs = after + 1;
                    while vs < le && (buf[vs] == b' ' || buf[vs] == b'\t') {
                        vs += 1;
                    }
                    // Exclude a trailing CR so CRLF framing is preserved.
                    let mut ve = le;
                    if ve > vs && buf[ve - 1] == b'\r' {
                        ve -= 1;
                    }
                    for b in &mut buf[vs..ve] {
                        *b = self.mask;
                    }
                    break; // one header name per line
                }
            }

            ls = le + 1; // step past the '\n'
        }
    }

    /// Mask secret tokens by prefix anywhere in the buffer. A prefix matches only
    /// at a token boundary (start of buffer or preceded by a non-token byte), so
    /// `sk-` inside `task-force` is left alone; the run of token bytes following
    /// the prefix is masked.
    fn redact_tokens(&self, buf: &mut [u8]) {
        let mut i = 0usize;
        while i < buf.len() {
            let mut matched = false;
            for prefix in &self.token_prefixes {
                let end = i + prefix.len();
                if end <= buf.len()
                    && eq_ignore_ascii_case(&buf[i..end], prefix)
                    && (i == 0 || !is_token_byte(buf[i - 1]))
                {
                    // Mask the token run that follows the prefix.
                    let mut j = end;
                    while j < buf.len() && is_token_byte(buf[j]) {
                        buf[j] = self.mask;
                        j += 1;
                    }
                    i = j.max(end); // continue past the masked run
                    matched = true;
                    break;
                }
            }
            if !matched {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Redactor {
        Redactor::with_defaults()
    }

    /// Helper: redact a string's bytes and return the result as a String, plus
    /// assert the length never changed (the equal-length invariant).
    fn redacted(r: &Redactor, input: &str) -> String {
        let mut buf = input.as_bytes().to_vec();
        let before = buf.len();
        r.redact(&mut buf);
        assert_eq!(buf.len(), before, "redaction must be equal-length");
        String::from_utf8(buf).expect("mask byte keeps the buffer valid UTF-8")
    }

    #[test]
    fn masks_authorization_header_value_equal_length() {
        let input = "POST /v1/messages HTTP/1.1\r\nAuthorization: Bearer sk-ant-api03-SECRET\r\nContent-Length: 12\r\n\r\n";
        let out = redacted(&defaults(), input);
        // The Authorization value is gone…
        assert!(!out.contains("sk-ant-api03-SECRET"));
        assert!(!out.contains("Bearer sk"));
        // …but the header name, the request line, and Content-Length survive.
        assert!(out.contains("Authorization: "));
        assert!(out.contains("POST /v1/messages HTTP/1.1"));
        assert!(out.contains("Content-Length: 12"));
        // CRLF framing intact.
        assert!(out.contains("\r\n\r\n"));
    }

    #[test]
    fn masks_x_api_key_case_insensitively() {
        let input = "GET / HTTP/1.1\r\nX-Api-Key: sk-secretvalue\r\n\r\n";
        let out = redacted(&defaults(), input);
        assert!(!out.contains("secretvalue"));
        assert!(out.contains("X-Api-Key: "));
    }

    #[test]
    fn masks_azure_api_key_and_cookie() {
        let input = "GET / HTTP/1.1\r\napi-key: abcd1234\r\nCookie: session=deadbeef\r\n\r\n";
        let out = redacted(&defaults(), input);
        assert!(!out.contains("abcd1234"));
        assert!(!out.contains("deadbeef"));
        assert!(out.contains("api-key: "));
        assert!(out.contains("Cookie: "));
    }

    #[test]
    fn masks_sk_token_in_json_body_keeps_prefix() {
        let input = "POST /x HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"api_key\":\"sk-ant-api03-ABCDEF123\",\"model\":\"claude-3\"}";
        let out = redacted(&defaults(), input);
        assert!(!out.contains("sk-ant-api03-ABCDEF123"));
        assert!(out.contains("\"sk-"), "the prefix stays visible");
        // Non-secret JSON is untouched.
        assert!(out.contains("\"model\":\"claude-3\""));
        assert!(out.contains("Content-Type: application/json"));
    }

    #[test]
    fn leaves_normal_text_with_sk_substring_untouched() {
        // `sk-` inside `task-force` is not at a token boundary → not a secret.
        let input = "POST /x HTTP/1.1\r\n\r\n{\"note\":\"the task-force shipped\"}";
        let out = redacted(&defaults(), input);
        assert_eq!(out, input, "no token boundary → no redaction");
    }

    #[test]
    fn leaves_plain_body_untouched() {
        let input = "POST /v1/messages HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}";
        let out = redacted(&defaults(), input);
        assert_eq!(out, input);
    }

    #[test]
    fn header_value_without_leading_space_is_masked() {
        let input = "GET / HTTP/1.1\r\nAuthorization:sk-novalue-space\r\n\r\n";
        let out = redacted(&defaults(), input);
        assert!(!out.contains("novalue-space"));
        assert!(out.contains("Authorization:"));
    }

    #[test]
    fn header_name_substring_is_not_matched() {
        // `X-Authorization-Mode` must not be treated as `Authorization`.
        let input = "GET / HTTP/1.1\r\nX-Authorization-Mode: strict\r\n\r\n";
        let out = redacted(&defaults(), input);
        assert!(out.contains("strict"), "non-matching header is left alone");
    }

    #[test]
    fn empty_redactor_is_noop() {
        let r = Redactor::new(Vec::<String>::new(), Vec::<String>::new());
        assert!(r.is_noop());
        let input = "Authorization: Bearer sk-secret\r\n";
        assert_eq!(redacted(&r, input), input);
    }

    #[test]
    fn token_run_stops_at_quote_and_whitespace() {
        // Masking must stop at the closing quote / space, not eat the rest.
        let input = "x sk-AAA111 y \"sk-BBB222\" z";
        let out = redacted(&defaults(), input);
        assert!(out.contains("sk-")); // prefixes remain
        assert!(!out.contains("AAA111"));
        assert!(!out.contains("BBB222"));
        // The trailing markers survive — the run didn't overrun.
        assert!(out.contains(" y "));
        assert!(out.ends_with(" z"));
    }
}
