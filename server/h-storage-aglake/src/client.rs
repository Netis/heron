//! HTTP clients: [`HecClient`] for writes, [`SearchClient`] for reads,
//! [`ManagementClient`] for per-index retention.
//!
//! Each wraps one `reqwest::Client` (internally an `Arc`'d connection pool).
//! We deliberately do not reuse aglake's own `aglake-agent` HEC client: it opens
//! a fresh TCP connection per request, cannot do TLS or gzip, and discards the
//! response body — but the response body is exactly what the retry state
//! machine needs, since a partial-success 400 carries the index of the first
//! bad event.

use std::collections::HashMap;
use std::time::Duration;

use h_common::config::AglakeConfig;
use h_common::error::{AppError, Result};
use serde::Deserialize;

fn err<E: std::fmt::Display>(ctx: &str, e: E) -> AppError {
    AppError::Storage(format!("aglake {ctx}: {e}"))
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Splunk HEC writer.
///
/// # Retry semantics — at-least-once
///
/// [`h_storage::WriteBuffer`] discards a batch whose flush returns `Err`, so
/// all retrying has to happen here. The rules follow what aglaked actually
/// does:
///
/// * **200** — events are already fsynced. Never resend.
/// * **400 with `invalid-event-number: k`** — HEC ingests the valid prefix and
///   stops at the first bad event, so `[start, start+k)` is committed, event
///   `k` is malformed, and the rest was never seen. Skip past `k` and carry
///   on; this is deterministic progress, not a retry, so it does not consume
///   the retry budget.
/// * **401 / 415 / other 400** — configuration or protocol faults. Resending
///   cannot help.
/// * **413** — halve the batch and re-split once.
/// * **5xx / timeout / connection error** — the request may or may not have
///   landed. With acks enabled, ask before resending; otherwise resend and
///   accept a possible duplicate.
///
/// The gap this leaves is a aglaked restart mid-flight: ack ids are
/// process-local and reset, so a resend can duplicate. Duplicates are visible
/// (two rows with one id) and harmless for everything except metric sums,
/// which is what `metrics_dedup` is for.
///
/// # How the ack is actually used
///
/// aglake issues an ack id **in the same response that reports success** — so
/// there is no id to ask about when the response is the thing that got lost.
/// The way through is to send every request on a **freshly minted channel**:
/// aglake's per-channel counter starts at zero, so the only id that request
/// could ever be given is `0`, and `POST /services/collector/ack` with
/// `{"acks":[0]}` becomes a direct question — *did this request commit?*
/// Measured against aglaked: `false` before the write, `true` after.
///
/// The answer degrades safely in every direction. aglake's channel table is
/// in-memory and LRU-capped, so a restart or heavy churn answers `false` and
/// we resend — exactly what would have happened with acks off. A 400 never
/// issues an ack id, but that path is already deterministic through
/// `invalid-event-number`, so it never consults one. The case acks cannot
/// cover is a 500 raised after some indexes in the batch already committed:
/// no id is issued, and the resend duplicates that prefix.
pub(crate) struct HecClient {
    http: reqwest::Client,
    endpoint: String,
    ack_endpoint: String,
    token: String,
    max_body_bytes: usize,
    max_event_bytes: usize,
    gzip: bool,
    use_ack: bool,
    retries: u32,
    backoff: Duration,
}

/// What aglaked said about one HEC request.
enum HecOutcome {
    Ok,
    /// Valid prefix committed; event at this 0-based index is malformed.
    PartialUpTo(usize),
    TooLarge,
    /// Worth another attempt (5xx, timeout, connection reset).
    Transient(String),
    /// Retrying cannot help.
    Permanent(String),
}

#[derive(Deserialize)]
struct HecResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    text: String,
    #[serde(rename = "invalid-event-number", default)]
    invalid_event_number: Option<usize>,
}

/// `{"acks": {"0": true}}` — the answer to an ack query.
#[derive(Deserialize, Default)]
struct AckResponse {
    #[serde(default)]
    acks: HashMap<String, bool>,
}

impl HecClient {
    pub(crate) fn new(config: &AglakeConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|e| err("client build", e))?;
        let base = config.url.trim_end_matches('/');
        Ok(Self {
            http,
            endpoint: format!("{base}/services/collector/event"),
            ack_endpoint: format!("{base}/services/collector/ack"),
            token: config.hec_token.clone(),
            max_body_bytes: config.max_body_bytes,
            max_event_bytes: config.max_event_bytes,
            gzip: config.gzip,
            use_ack: config.use_ack,
            retries: config.write_retries,
            backoff: Duration::from_millis(config.retry_backoff_ms),
        })
    }

    /// Send pre-serialized HEC envelopes (one JSON object per element, no
    /// trailing newline needed — aglaked parses a concatenated stream).
    pub(crate) async fn send(&self, events: Vec<String>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let events = self.enforce_event_size(events);
        for chunk in self.split_by_bytes(&events) {
            self.send_chunk(chunk).await?;
        }
        Ok(())
    }

    /// Drop any single event that exceeds the configured ceiling.
    ///
    /// An event past aglake's 16 MiB WAL frame limit is treated as corruption
    /// during crash replay and silently discarded — the worst possible failure
    /// mode. Refusing to send it trades one lost event for a loud log line and
    /// a store that stays replayable. `[body_cap]` normally keeps events three
    /// orders of magnitude below this; the guard matters when it is disabled.
    fn enforce_event_size(&self, events: Vec<String>) -> Vec<String> {
        let mut oversized = 0usize;
        let kept: Vec<String> = events
            .into_iter()
            .filter(|e| {
                if e.len() > self.max_event_bytes {
                    oversized += 1;
                    false
                } else {
                    true
                }
            })
            .collect();
        if oversized > 0 {
            tracing::error!(
                target: "aglake::write",
                dropped = oversized,
                max_event_bytes = self.max_event_bytes,
                "aglake: dropped oversized event(s); they would be discarded as \
                 corruption on crash replay. Enable [body_cap] or lower it."
            );
        }
        kept
    }

    fn split_by_bytes<'a>(&self, events: &'a [String]) -> Vec<&'a [String]> {
        let mut out = Vec::new();
        let (mut start, mut acc) = (0usize, 0usize);
        for (i, e) in events.iter().enumerate() {
            let n = e.len() + 1;
            if acc + n > self.max_body_bytes && i > start {
                out.push(&events[start..i]);
                start = i;
                acc = 0;
            }
            acc += n;
        }
        if start < events.len() {
            out.push(&events[start..]);
        }
        out
    }

    async fn send_chunk(&self, chunk: &[String]) -> Result<()> {
        let mut start = 0usize;
        let mut attempt = 0u32;
        while start < chunk.len() {
            match self.post(&chunk[start..]).await {
                HecOutcome::Ok => return Ok(()),
                HecOutcome::PartialUpTo(k) => {
                    tracing::warn!(
                        target: "aglake::write",
                        index = start + k,
                        "aglake rejected an event; the batch prefix before it is \
                         committed. Skipping it and continuing."
                    );
                    start += k + 1;
                }
                HecOutcome::TooLarge => {
                    // Re-split this range with a smaller ceiling. One level is
                    // enough: max_body_bytes is already well under aglaked's
                    // default and events are individually capped.
                    let half = (chunk.len() - start).div_ceil(2).max(1);
                    if half == chunk.len() - start {
                        return Err(err("write", "413 on an unsplittable batch"));
                    }
                    let mid = start + half;
                    Box::pin(self.send_chunk(&chunk[start..mid])).await?;
                    Box::pin(self.send_chunk(&chunk[mid..])).await?;
                    return Ok(());
                }
                HecOutcome::Transient(msg) => {
                    attempt += 1;
                    if attempt > self.retries {
                        return Err(err("write", format!("giving up after {attempt}: {msg}")));
                    }
                    tokio::time::sleep(self.backoff * attempt).await;
                }
                HecOutcome::Permanent(msg) => return Err(err("write", msg)),
            }
        }
        Ok(())
    }

    async fn post(&self, events: &[String]) -> HecOutcome {
        let mut body = Vec::with_capacity(events.iter().map(|e| e.len() + 1).sum());
        for e in events {
            body.extend_from_slice(e.as_bytes());
            body.push(b'\n');
        }

        // A channel used exactly once, so the only ack id it can be given is
        // 0 and asking about that id asks about this request. See the type
        // docs for why a shared channel could not answer the same question.
        let channel = self
            .use_ack
            .then(|| uuid::Uuid::now_v7().to_string())
            .filter(|_| !events.is_empty());

        let mut req = self.http.post(&self.endpoint);
        if !self.token.is_empty() {
            req = req.header("Authorization", format!("Splunk {}", self.token));
        }
        if let Some(ch) = &channel {
            req = req.header("X-Splunk-Request-Channel", ch);
        }
        if self.gzip {
            use flate2::{write::GzEncoder, Compression};
            use std::io::Write;
            let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
            if enc.write_all(&body).is_ok() {
                if let Ok(z) = enc.finish() {
                    body = z;
                    req = req.header("Content-Encoding", "gzip");
                }
            }
        }

        let resp = match req.body(body).send().await {
            Ok(r) => r,
            Err(e) => {
                return self
                    .resolve_transient(channel.as_deref(), e.to_string())
                    .await
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status.is_success() {
            return HecOutcome::Ok;
        }
        if status.as_u16() == 413 {
            return HecOutcome::TooLarge;
        }
        if status.is_server_error() {
            return self
                .resolve_transient(channel.as_deref(), format!("{status}: {}", truncate(&text)))
                .await;
        }
        if status.as_u16() == 400 {
            if let Ok(r) = serde_json::from_str::<HecResponse>(&text) {
                if let Some(k) = r.invalid_event_number {
                    return HecOutcome::PartialUpTo(k);
                }
                return HecOutcome::Permanent(format!("400 code={} {}", r.code, truncate(&r.text)));
            }
        }
        HecOutcome::Permanent(format!("{status}: {}", truncate(&text)))
    }

    /// The request failed in a way that leaves it genuinely unknown whether
    /// the batch landed. With acks on, stop guessing and ask.
    async fn resolve_transient(&self, channel: Option<&str>, msg: String) -> HecOutcome {
        let Some(ch) = channel else {
            return HecOutcome::Transient(msg);
        };
        if self.ack_committed(ch).await == Some(true) {
            tracing::info!(
                target: "aglake::write",
                reason = %msg,
                "aglake: request failed after the batch was committed; \
                 acknowledged, so not resending"
            );
            return HecOutcome::Ok;
        }
        HecOutcome::Transient(msg)
    }

    /// `Some(true)` when aglake confirms the batch on `channel` reached disk,
    /// `Some(false)` when it says otherwise, `None` when the question itself
    /// could not be answered. Only `Some(true)` suppresses a resend — the
    /// other two both mean "we do not know it landed", which is a resend.
    pub(crate) async fn ack_committed(&self, channel: &str) -> Option<bool> {
        let mut req = self
            .http
            .post(&self.ack_endpoint)
            .query(&[("channel", channel)])
            .json(&serde_json::json!({ "acks": [0] }));
        if !self.token.is_empty() {
            req = req.header("Authorization", format!("Splunk {}", self.token));
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let parsed: AckResponse = resp.json().await.ok()?;
        parsed.acks.get("0").copied()
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(300).collect()
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// One row of a search result. Values are whatever JSON aglake emitted.
pub(crate) type Row = serde_json::Map<String, serde_json::Value>;

#[derive(Deserialize, Default)]
pub(crate) struct SearchResult {
    /// `results` or `events`, depending on whether the pipeline ended in a
    /// transforming command. Kept for diagnostics.
    #[allow(dead_code)]
    #[serde(default)]
    pub mode: String,
    /// Rows **emitted**, not rows matched — a pipeline redefines it. Page
    /// totals therefore come from a separate `| stats count` query, added in
    /// Phase 2.
    #[allow(dead_code)]
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub rows: Vec<Row>,
    #[serde(default)]
    pub events: Vec<Row>,
}

impl SearchResult {
    /// Rows regardless of which mode the server answered in. Pipelines ending
    /// in `| table` come back as `results`; a bare search comes back as
    /// `events`.
    pub(crate) fn rows(self) -> Vec<Row> {
        if self.rows.is_empty() && !self.events.is_empty() {
            self.events
        } else {
            self.rows
        }
    }
}

/// SPL reader over `/api/v1/search`.
///
/// ⚠️ These endpoints are unauthenticated in aglaked — there is no token to
/// present, unlike HEC. Access control has to come from the network.
pub(crate) struct SearchClient {
    http: reqwest::Client,
    endpoint: String,
    ping_url: String,
}

impl SearchClient {
    pub(crate) fn new(config: &AglakeConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.search_timeout_secs))
            .build()
            .map_err(|e| err("client build", e))?;
        let base = config.url.trim_end_matches('/');
        Ok(Self {
            http,
            endpoint: format!("{base}/api/v1/search"),
            ping_url: format!("{base}/api/v1/indexes"),
        })
    }

    /// Run a query. `earliest` / `latest` are epoch-second strings; `"0"` means
    /// unbounded, which disables bucket pruning and should be avoided on any
    /// path that knows its time range.
    pub(crate) async fn search(
        &self,
        spl: &str,
        earliest: &str,
        latest: &str,
    ) -> Result<SearchResult> {
        let body = serde_json::json!({ "q": spl, "earliest": earliest, "latest": latest });
        let resp = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| err("search", e))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| err("search", e))?;
        if !status.is_success() {
            return Err(err("search", format!("{status}: {}", truncate(&text))));
        }
        serde_json::from_str(&text).map_err(|e| err("search decode", e))
    }

    /// Unbounded variant, for the few reads whose trait signature carries no
    /// time range (the filter-dropdown distincts).
    pub(crate) async fn search_all_time(&self, spl: &str) -> Result<SearchResult> {
        self.search(spl, "0", "0").await
    }

    pub(crate) async fn ping(&self) -> Result<()> {
        let resp = self
            .http
            .get(&self.ping_url)
            .send()
            .await
            .map_err(|e| err("connect", e))?;
        if !resp.status().is_success() {
            return Err(err(
                "connect",
                format!("{} from {}", resp.status(), self.ping_url),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Index management (retention)
// ---------------------------------------------------------------------------

/// Per-index settings, over aglake's Splunk-compatible management REST face.
///
/// # This API is not always there
///
/// The whole `/en-US/splunkd/__raw` namespace — this endpoint included — is
/// mounted **only when aglaked finds vendored Splunk frontend assets** at
/// `--splunk-web-dir`. Started without them, every route here answers 404 with
/// an empty body. A deployment that runs aglaked purely as an ingest/search
/// engine therefore cannot be told about retention at all, and Heron has to
/// notice that rather than log a stream of failures. That is what
/// [`Self::list_indexes`] is for: one cheap call that answers both "is this
/// API here?" and "which of my indexes exist yet?".
///
/// # And it is session-authenticated
///
/// When aglaked runs with auth enabled, writes here need a login session cookie
/// plus a CSRF form key — a browser flow, not something a server-side client
/// holds. The HEC token does **not** work. So retention management is
/// supported for the deployment Heron actually documents (aglaked bound to
/// loopback, auth off); anything else gets one clear warning and a no-op.
pub(crate) struct ManagementClient {
    http: reqwest::Client,
    /// `…/services/aglake/settings/indexes`
    settings_url: String,
    /// `…/services/data/indexes`
    list_url: String,
}

/// One index as the management API reports it.
pub(crate) struct IndexInfo {
    pub name: String,
    /// `frozenTimePeriodInSecs` — the TTL after which a bucket is frozen.
    pub frozen_after_secs: Option<i64>,
}

#[derive(Deserialize, Default)]
struct IndexFeed {
    #[serde(default)]
    entry: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    content: serde_json::Map<String, serde_json::Value>,
}

impl ManagementClient {
    pub(crate) fn new(config: &AglakeConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|e| err("client build", e))?;
        let base = format!("{}/en-US/splunkd/__raw", config.url.trim_end_matches('/'));
        Ok(Self {
            http,
            settings_url: format!("{base}/services/aglake/settings/indexes"),
            list_url: format!("{base}/services/data/indexes"),
        })
    }

    /// Every index aglake currently knows about, with its retention.
    ///
    /// Doubles as the availability probe — see the type docs. Also the only
    /// way to avoid guessing at 404s: pushing settings to an index that has
    /// never been written to is a 404 that means "not yet", which is
    /// indistinguishable by status code from the 404 that means "this API is
    /// not mounted".
    pub(crate) async fn list_indexes(&self) -> Result<Vec<IndexInfo>> {
        let resp = self
            .http
            .get(&self.list_url)
            .send()
            .await
            .map_err(|e| err("index list", e))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| err("index list", e))?;
        if !status.is_success() {
            return Err(err(
                "index list",
                format!("{status} from {} — {}", self.list_url, truncate(&text)),
            ));
        }
        let feed: IndexFeed = serde_json::from_str(&text).map_err(|e| err("index list", e))?;
        Ok(feed
            .entry
            .into_iter()
            .map(|e| IndexInfo {
                name: e.name,
                frozen_after_secs: e
                    .content
                    .get("frozenTimePeriodInSecs")
                    .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok())),
            })
            .collect())
    }

    /// Set one index's retention. `secs` is a TTL from event time, not a
    /// cutoff — aglake freezes a bucket once its newest event is that old.
    pub(crate) async fn set_retention(&self, index: &str, secs: u64) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/{index}", self.settings_url))
            .form(&[("frozen_after_secs", secs.to_string())])
            .send()
            .await
            .map_err(|e| err("set retention", e))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        Err(err(
            "set retention",
            format!("{status} on index {index}: {}", truncate(&text)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AglakeConfig {
        AglakeConfig {
            max_body_bytes: 100,
            max_event_bytes: 20,
            ..Default::default()
        }
    }

    #[test]
    fn split_by_bytes_respects_ceiling_and_keeps_order() {
        let c = HecClient::new(&cfg()).unwrap();
        let ev: Vec<String> = (0..10).map(|i| format!("{:0>29}", i)).collect(); // 30B each
        let chunks = c.split_by_bytes(&ev);
        assert!(chunks.len() > 1);
        for ch in &chunks {
            assert!(ch.iter().map(|e| e.len() + 1).sum::<usize>() <= 100 || ch.len() == 1);
        }
        let flat: Vec<&String> = chunks.iter().flat_map(|c| c.iter()).collect();
        assert_eq!(flat.len(), ev.len());
        assert_eq!(*flat[0], ev[0], "order must be preserved");
    }

    /// A single event larger than the ceiling still has to go somewhere:
    /// it becomes its own chunk rather than being silently merged or dropped.
    #[test]
    fn split_by_bytes_isolates_a_single_large_event() {
        let c = HecClient::new(&cfg()).unwrap();
        let ev = vec!["a".repeat(500), "b".into()];
        let chunks = c.split_by_bytes(&ev);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1);
    }

    #[test]
    fn oversized_events_are_dropped_not_sent() {
        let c = HecClient::new(&cfg()).unwrap();
        let kept = c.enforce_event_size(vec!["ok".into(), "x".repeat(21), "fine".into()]);
        assert_eq!(kept, vec!["ok".to_string(), "fine".to_string()]);
    }

    #[test]
    fn partial_success_response_parses_invalid_event_number() {
        let r: HecResponse = serde_json::from_str(
            r#"{"text":"Invalid data format","code":6,"invalid-event-number":7}"#,
        )
        .unwrap();
        assert_eq!(r.invalid_event_number, Some(7));
        assert_eq!(r.code, 6);

        let ok: HecResponse = serde_json::from_str(r#"{"text":"Success","code":0}"#).unwrap();
        assert_eq!(ok.invalid_event_number, None);
    }

    #[test]
    fn search_result_reads_either_mode() {
        let results: SearchResult =
            serde_json::from_str(r#"{"mode":"results","total":1,"rows":[{"n":5}]}"#).unwrap();
        assert_eq!(results.rows().len(), 1);

        let events: SearchResult =
            serde_json::from_str(r#"{"mode":"events","total":2,"events":[{"a":1},{"a":2}]}"#)
                .unwrap();
        assert_eq!(events.rows().len(), 2);

        let empty: SearchResult = serde_json::from_str(r#"{"mode":"results","total":0}"#).unwrap();
        assert!(empty.rows().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Retry state machine
// ---------------------------------------------------------------------------

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// What the mock should do with one request.
    #[derive(Clone)]
    enum Reply {
        Status(u16, &'static str),
        /// Accept the request and never answer, so the client times out.
        Hang,
    }

    /// A scripted HEC server on a real socket.
    ///
    /// The retry rules are the durability contract — `WriteBuffer` discards a
    /// batch whose flush returns `Err`, so a wrong branch here is a silently
    /// lost write or a duplicated one. They are also the part of this backend
    /// least reachable from a live server: getting aglaked to emit a 413, or to
    /// accept a request and then vanish, is not something a test can ask it
    /// for. Scripting the responses is the only way to walk every branch.
    struct MockHec {
        addr: SocketAddr,
        /// One entry per request received: `(path, body)`.
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockHec {
        fn start(script: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let seen_bg = Arc::clone(&seen);
            let next = Arc::new(AtomicUsize::new(0));

            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    // One thread per connection. A single-threaded accept loop
                    // would leave the retry's connection sitting in the backlog
                    // while `Hang` sleeps on the previous one, and the test
                    // would conclude no retry was attempted.
                    let seen_bg = Arc::clone(&seen_bg);
                    let next = Arc::clone(&next);
                    let script = script.clone();
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());

                        let mut request_line = String::new();
                        if reader.read_line(&mut request_line).is_err() {
                            return;
                        }
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or_default()
                            .to_string();

                        let mut len = 0usize;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                                break;
                            }
                            if let Some(v) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                len = v.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0u8; len];
                        let _ = reader.read_exact(&mut body);
                        let body = String::from_utf8_lossy(&body).to_string();

                        // Ack queries are bookkeeping, not part of the script.
                        if path.starts_with("/services/collector/ack") {
                            let body = r#"{"acks":{"0":false}}"#;
                            let _ = stream.write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\n\r\n{body}",
                                    body.len()
                                )
                                .as_bytes(),
                            );
                            return;
                        }

                        seen_bg.lock().unwrap().push((path, body));
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        let reply = script
                            .get(i)
                            .cloned()
                            .unwrap_or(Reply::Status(200, r#"{"text":"Success","code":0}"#));
                        match reply {
                            Reply::Hang => {
                                // Hold the socket open, answering nothing.
                                std::thread::sleep(std::time::Duration::from_secs(30));
                            }
                            Reply::Status(code, body) => {
                                let _ = stream.write_all(
                                    format!(
                                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n\
                                     Content-Length: {}\r\n\r\n{body}",
                                        body.len()
                                    )
                                    .as_bytes(),
                                );
                            }
                        }
                    });
                }
            });
            Self { addr, seen }
        }

        fn requests(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn client(mock: &MockHec, retries: u32) -> HecClient {
        HecClient::new(&AglakeConfig {
            url: format!("http://{}", mock.addr),
            hec_token: "t".into(),
            write_retries: retries,
            retry_backoff_ms: 1,
            request_timeout_secs: 1,
            // Off so a request body is comparable to what was sent.
            gzip: false,
            // The ack path has its own tests; keep these on the plain branch.
            use_ack: false,
            ..Default::default()
        })
        .unwrap()
    }

    fn events(n: usize) -> Vec<String> {
        (0..n).map(|i| format!(r#"{{"event":{i}}}"#)).collect()
    }

    /// 200 means fsynced. Resending would duplicate for no reason.
    #[tokio::test]
    async fn success_sends_once_and_never_again() {
        let mock = MockHec::start(vec![Reply::Status(200, r#"{"text":"Success","code":0}"#)]);
        client(&mock, 3).send(events(3)).await.unwrap();
        assert_eq!(mock.requests().len(), 1);
    }

    /// A partial-success 400 says how far it got. The prefix is committed, the
    /// named event is bad, and the rest was never seen — so the right move is
    /// to skip past the bad one and continue, not to resend from the start.
    #[tokio::test]
    async fn partial_success_skips_the_bad_event_and_resumes_after_it() {
        let mock = MockHec::start(vec![Reply::Status(
            400,
            r#"{"text":"Invalid data format","code":6,"invalid-event-number":2}"#,
        )]);
        client(&mock, 3).send(events(5)).await.unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2, "expected the retry to resume, not restart");
        // Second attempt starts at index 3: 0 and 1 committed, 2 was rejected.
        assert!(
            reqs[1].1.starts_with(r#"{"event":3}"#),
            "resumed at the wrong offset: {}",
            reqs[1].1
        );
        assert!(
            !reqs[1].1.contains(r#"{"event":2}"#),
            "the rejected event must not be resent: {}",
            reqs[1].1
        );
    }

    /// Deterministic progress is not a retry, so it must not consume the
    /// budget — otherwise a batch with several bad events gives up early and
    /// discards the good ones after it.
    #[tokio::test]
    async fn skipping_bad_events_does_not_spend_the_retry_budget() {
        let bad = |n: usize| {
            Reply::Status(
                400,
                Box::leak(
                    format!(r#"{{"text":"x","code":6,"invalid-event-number":{n}}}"#)
                        .into_boxed_str(),
                ),
            )
        };
        // Four rejections in a row, with a retry budget of one.
        let mock = MockHec::start(vec![bad(0), bad(0), bad(0), bad(0)]);
        client(&mock, 1).send(events(6)).await.unwrap();
        assert_eq!(
            mock.requests().len(),
            5,
            "each rejection should advance and continue, not exhaust retries"
        );
    }

    /// A bad token cannot be fixed by asking again.
    #[tokio::test]
    async fn an_auth_failure_is_permanent() {
        let mock = MockHec::start(vec![Reply::Status(401, r#"{"text":"Invalid token"}"#)]);
        let err = client(&mock, 3).send(events(2)).await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
        assert_eq!(mock.requests().len(), 1, "401 must not be retried");
    }

    /// A 400 without `invalid-event-number` is a protocol fault, not progress.
    #[tokio::test]
    async fn a_400_without_an_event_number_is_permanent() {
        let mock = MockHec::start(vec![Reply::Status(400, r#"{"text":"No data","code":5}"#)]);
        let err = client(&mock, 3).send(events(2)).await.unwrap_err();
        assert!(err.to_string().contains("code=5"), "{err}");
        assert_eq!(mock.requests().len(), 1);
    }

    /// 413 means the batch was too big, so halve it and send both halves.
    #[tokio::test]
    async fn too_large_splits_the_batch_rather_than_dropping_it() {
        let mock = MockHec::start(vec![Reply::Status(413, "too large")]);
        client(&mock, 3).send(events(4)).await.unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 3, "one rejected attempt, then two halves");
        // Together the halves must carry every event exactly once.
        let resent = format!("{}{}", reqs[1].1, reqs[2].1);
        for i in 0..4 {
            assert_eq!(
                resent.matches(&format!(r#"{{"event":{i}}}"#)).count(),
                1,
                "event {i} appears the wrong number of times after the split"
            );
        }
    }

    /// A 5xx may or may not have landed, so retry — and give up loudly rather
    /// than reporting a success that did not happen.
    #[tokio::test]
    async fn a_server_error_retries_then_reports_failure() {
        let mock = MockHec::start(vec![
            Reply::Status(503, "unavailable"),
            Reply::Status(503, "unavailable"),
            Reply::Status(503, "unavailable"),
        ]);
        let err = client(&mock, 2).send(events(2)).await.unwrap_err();
        assert!(err.to_string().contains("giving up"), "{err}");
        assert_eq!(mock.requests().len(), 3, "one attempt plus two retries");
    }

    /// The same, but the server recovers — the write must then succeed.
    #[tokio::test]
    async fn a_server_error_that_clears_is_not_an_error() {
        let mock = MockHec::start(vec![
            Reply::Status(503, "unavailable"),
            Reply::Status(200, r#"{"text":"Success","code":0}"#),
        ]);
        client(&mock, 3).send(events(2)).await.unwrap();
        assert_eq!(mock.requests().len(), 2);
    }

    /// A request that is accepted and never answered must time out, count as
    /// transient, and be retried — not hang the flush until the server feels
    /// like replying.
    #[tokio::test]
    async fn a_timeout_is_retried_and_the_retry_can_succeed() {
        let mock = MockHec::start(vec![Reply::Hang]);
        let started = std::time::Instant::now();
        client(&mock, 1).send(events(2)).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the 1s request timeout did not apply: {:?}",
            started.elapsed()
        );
        assert_eq!(mock.requests().len(), 2, "one attempt plus one retry");
    }

    /// When every attempt times out, the flush must fail rather than report a
    /// success nobody can back up.
    #[tokio::test]
    async fn a_persistent_timeout_eventually_fails() {
        let mock = MockHec::start(vec![Reply::Hang, Reply::Hang, Reply::Hang]);
        let err = client(&mock, 1).send(events(2)).await.unwrap_err();
        assert!(err.to_string().contains("giving up"), "{err}");
        assert_eq!(mock.requests().len(), 2, "one attempt plus one retry");
    }

    /// With acks on, a request that failed *after* the server committed it
    /// must not be resent. The mock answers `{"0": false}` — not committed —
    /// so this one still resends; the positive case needs a real server and
    /// lives in the live suite.
    #[tokio::test]
    async fn an_unacknowledged_batch_is_resent() {
        let mock = MockHec::start(vec![Reply::Status(503, "unavailable")]);
        let c = HecClient::new(&AglakeConfig {
            url: format!("http://{}", mock.addr),
            write_retries: 1,
            retry_backoff_ms: 1,
            request_timeout_secs: 1,
            gzip: false,
            use_ack: true,
            ..Default::default()
        })
        .unwrap();
        let _ = c.send(events(2)).await;
        let paths: Vec<&str> = mock
            .requests()
            .iter()
            .map(|(p, _)| p.as_str())
            .map(|p| {
                if p.starts_with("/services/collector/event") {
                    "event"
                } else {
                    "other"
                }
            })
            .collect();
        assert_eq!(
            paths,
            vec!["event", "event"],
            "an unacked batch must be resent"
        );
    }
}
