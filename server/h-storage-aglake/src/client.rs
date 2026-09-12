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
use std::sync::Arc;
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
// Session authentication
// ---------------------------------------------------------------------------

/// Name of the session cookie aglaked sets on login. Its value *is* the
/// session token — the daemon documents the two as the same string, and
/// presenting it as `Authorization: Bearer` is the form that skips CSRF,
/// which is what a server-side client wants.
const SESSION_COOKIE: &str = "aglake_session";

/// Bearer credentials for aglake's `/api/v1/*` faces.
///
/// aglake 0.3 gained a local user catalog. With one configured, every
/// `/api/v1/*` request — search included — answers `401` without a session,
/// and `/api/v1/admin/*` additionally wants the `admin` role. With no users,
/// which is aglaked's default and what the loopback deployment runs, the whole
/// surface is open and this holds no credentials at all.
///
/// # Why a 401 is routine rather than a misconfiguration
///
/// Sessions live in the daemon's memory with a 12-hour TTL, so they expire on
/// their own, and they vanish outright when aglaked restarts. Any Heron that
/// runs longer than a session will meet a 401 eventually, through no fault of
/// its configuration. The only workable answer is to log in again and retry —
/// see [`send_authenticated`], which does it exactly once so that a stale
/// session (retry succeeds) stays distinguishable from bad credentials (it
/// does not).
pub(crate) struct AuthState {
    http: reqwest::Client,
    login_url: String,
    /// `None` when nothing is configured — the no-auth deployment, where
    /// requests go out bare.
    source: Option<TokenSource>,
    /// The session in hand, once we have one. `None` before first use, and
    /// after a refresh discards a stale one.
    token: tokio::sync::RwLock<Option<String>>,
}

enum TokenSource {
    /// A token supplied verbatim in config. There is nothing to re-derive
    /// when it stops working, so a 401 on one is terminal.
    Fixed(String),
    /// Username + password, exchangeable for a fresh session at any time.
    Login { username: String, password: String },
}

impl AuthState {
    pub(crate) fn new(config: &AglakeConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|e| err("client build", e))?;
        // A configured token wins: it is the more specific instruction, and
        // honouring the password instead would silently ignore what the
        // operator wrote.
        let source = if !config.session_token.is_empty() {
            Some(TokenSource::Fixed(config.session_token.clone()))
        } else if !config.username.is_empty() {
            Some(TokenSource::Login {
                username: config.username.clone(),
                password: config.password.clone(),
            })
        } else {
            None
        };
        Ok(Self {
            http,
            login_url: format!("{}/api/v1/auth/login", config.url.trim_end_matches('/')),
            source,
            token: tokio::sync::RwLock::new(None),
        })
    }

    /// Whether a fresh session can be obtained without operator action. False
    /// for a fixed token and for the no-auth deployment, in both of which
    /// retrying a 401 would just produce the same 401.
    pub(crate) fn can_refresh(&self) -> bool {
        matches!(self.source, Some(TokenSource::Login { .. }))
    }

    /// The token to present, logging in if this is the first request.
    pub(crate) async fn token(&self) -> Result<Option<String>> {
        if self.source.is_none() {
            return Ok(None);
        }
        if let Some(token) = self.token.read().await.clone() {
            return Ok(Some(token));
        }
        self.acquire().await
    }

    /// Drop the session in hand and get another. Called after a 401, where
    /// the token we hold has been shown not to work.
    pub(crate) async fn refresh(&self) -> Result<Option<String>> {
        if !self.can_refresh() {
            return Ok(None);
        }
        self.token.write().await.take();
        self.acquire().await
    }

    /// Log in and cache the result. Holds the write lock across the request so
    /// a burst of concurrent reads produces one login, not one per caller.
    async fn acquire(&self) -> Result<Option<String>> {
        let mut slot = self.token.write().await;
        // Someone may have logged in while we waited for the lock.
        if let Some(token) = slot.clone() {
            return Ok(Some(token));
        }
        let token = match &self.source {
            None => None,
            Some(TokenSource::Fixed(token)) => Some(token.clone()),
            Some(TokenSource::Login { username, password }) => {
                self.login(username, password).await?
            }
        };
        slot.clone_from(&token);
        Ok(token)
    }

    /// `POST /api/v1/auth/login`. Returns `None` when the daemon has no user
    /// catalog: it answers `200` with `auth_enabled=false` and sets no cookie,
    /// which means credentials were configured against a server that does not
    /// want them. That is over-configuration, not an error — the requests work
    /// bare — so it resolves to "no token" rather than a failure.
    async fn login(&self, username: &str, password: &str) -> Result<Option<String>> {
        let resp = self
            .http
            .post(&self.login_url)
            .json(&serde_json::json!({ "username": username, "password": password }))
            .send()
            .await
            .map_err(|e| err("login", e))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(err(
                "login",
                format!(
                    "{status} from {} as {username}: {}",
                    self.login_url,
                    truncate(&text)
                ),
            ));
        }
        match session_cookie(&resp) {
            Some(token) => Ok(Some(token)),
            None => {
                tracing::info!(
                    "aglake: storage.aglake.username is set but aglaked has no user \
                     catalog; continuing without credentials"
                );
                Ok(None)
            }
        }
    }
}

/// Pull the session token out of a login response's `Set-Cookie` headers.
fn session_cookie(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|cookie| {
            let (name, rest) = cookie.split_once('=')?;
            (name.trim() == SESSION_COOKIE)
                .then(|| rest.split(';').next().unwrap_or(rest).to_string())
        })
}

/// Attach the session token, when there is one.
fn with_token(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

/// Send a request, and on `401` log in again and send it once more.
///
/// `build` is called per attempt rather than once, because a `RequestBuilder`
/// is consumed by `send`. The single retry is the whole point: it absorbs the
/// expected 401 — a session that aged out or died with the daemon — without
/// hiding the unexpected one, since wrong credentials fail the retry too and
/// the second 401 is what the caller reports.
async fn send_authenticated<F>(auth: &AuthState, ctx: &str, build: F) -> Result<reqwest::Response>
where
    F: Fn(Option<&str>) -> reqwest::RequestBuilder,
{
    let token = auth.token().await?;
    let resp = build(token.as_deref())
        .send()
        .await
        .map_err(|e| err(ctx, e))?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED || !auth.can_refresh() {
        return Ok(resp);
    }
    let token = auth.refresh().await?;
    build(token.as_deref())
        .send()
        .await
        .map_err(|e| err(ctx, e))
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// One row of a search result. Values are whatever JSON aglake emitted.
pub(crate) type Row = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Deserialize, Default)]
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
/// These endpoints sit behind aglake's session guard: open when the daemon has
/// no user catalog, `401` without a session when it has one. [`AuthState`]
/// holds whichever of those applies.
pub(crate) struct SearchClient {
    http: reqwest::Client,
    auth: Arc<AuthState>,
    endpoint: String,
    ping_url: String,
}

impl SearchClient {
    pub(crate) fn new(config: &AglakeConfig, auth: Arc<AuthState>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.search_timeout_secs))
            .build()
            .map_err(|e| err("client build", e))?;
        let base = config.url.trim_end_matches('/');
        Ok(Self {
            http,
            auth,
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
        let resp = send_authenticated(&self.auth, "search", |token| {
            with_token(self.http.post(&self.endpoint).json(&body), token)
        })
        .await?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| err("search", e))?;
        if !status.is_success() {
            return Err(err(
                "search",
                describe_search_failure(status, &self.endpoint, &text),
            ));
        }
        serde_json::from_str(&text).map_err(|e| err("search decode", e))
    }

    /// Unbounded variant, for the few reads whose trait signature carries no
    /// time range (the filter-dropdown distincts).
    pub(crate) async fn search_all_time(&self, spl: &str) -> Result<SearchResult> {
        self.search(spl, "0", "0").await
    }

    pub(crate) async fn ping(&self) -> Result<()> {
        let resp = send_authenticated(&self.auth, "connect", |token| {
            with_token(self.http.get(&self.ping_url), token)
        })
        .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(err(
                "connect",
                describe_search_failure(status, &self.ping_url, &text),
            ));
        }
        Ok(())
    }
}

/// Turn a search-face failure into a message that names the fix.
///
/// Only the auth statuses get special treatment: a 401 here is the one an
/// operator is most likely to misread, because the HEC token they already
/// configured looks like it should have covered it.
fn describe_search_failure(status: reqwest::StatusCode, url: &str, body: &str) -> String {
    let detail = truncate(body);
    match status.as_u16() {
        401 => format!(
            "{status} from {url}: aglaked has a user catalog and this request \
             carried no valid session. Set storage.aglake.username / password \
             — storage.aglake.hec_token authenticates ingest only, not \
             /api/v1/*. {detail}"
        ),
        403 => format!(
            "{status} from {url}: the configured aglake user may not read \
             these indexes. {detail}"
        ),
        _ => format!("{status} from {url}: {detail}"),
    }
}

// ---------------------------------------------------------------------------
// Index management (retention)
// ---------------------------------------------------------------------------

/// Per-index settings, over aglake's native admin API.
///
/// # Which face this speaks
///
/// `/api/v1/admin/indexes`, and only that one. Before aglake 0.3 this went
/// through the Splunk-compatible `/en-US/splunkd/__raw` namespace, which is
/// mounted **only when the daemon finds vendored Splunk frontend assets** —
/// so a deployment running aglaked purely as an ingest/search engine could not
/// be told about retention at all. The native face is always mounted, which
/// removes that failure mode rather than working around it.
///
/// There is nothing to fall back to: upstream removed the pre-0.3 namespace
/// outright (it answers `410 Gone` naming its successor), so an aglaked old
/// enough to lack this face is simply not a version Heron supports. The
/// resulting 404 is reported like any other failure — see [`Self::list_indexes`],
/// which doubles as the availability probe.
///
/// # Access
///
/// Every method under `/api/v1/admin/` requires the `admin` role. With an
/// empty user catalog — aglaked's development default — reads and index
/// writes are open, so the common loopback deployment needs no credentials.
/// With users configured it needs a session, which [`AuthState`] supplies as a
/// bearer token. Anonymous is `401` and non-admin is `403`; both are reported
/// as themselves, because "no credentials" and "wrong credentials" have
/// different fixes.
pub(crate) struct ManagementClient {
    http: reqwest::Client,
    auth: Arc<AuthState>,
    /// `…/api/v1/admin/indexes`
    indexes_url: String,
}

/// One index as the management API reports it.
#[derive(Debug)]
pub(crate) struct IndexInfo {
    pub name: String,
    /// The TTL after which a bucket is frozen. `None` = no per-index TTL, in
    /// which case the daemon's server-wide retention is what applies.
    pub frozen_after_secs: Option<i64>,
}

#[derive(Deserialize, Default)]
struct IndexFeed {
    #[serde(default)]
    indexes: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    settings: IndexSettings,
}

/// Only the one field Heron sets. The daemon reports more (`disabled`,
/// `archived`, `max_total_mb`, `frozen_dir`, `summary`); ignoring them keeps
/// this from breaking when the set grows.
#[derive(Deserialize, Default)]
struct IndexSettings {
    #[serde(default)]
    frozen_after_secs: Option<i64>,
}

impl ManagementClient {
    pub(crate) fn new(config: &AglakeConfig, auth: Arc<AuthState>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|e| err("client build", e))?;
        let base = config.url.trim_end_matches('/');
        Ok(Self {
            http,
            auth,
            indexes_url: format!("{base}/api/v1/admin/indexes"),
        })
    }

    /// Every index aglake currently knows about, with its retention.
    ///
    /// Doubles as the availability probe — see the type docs. It is also what
    /// keeps 404s unambiguous: this endpoint always answers when the API is
    /// there, so a 404 from [`Self::set_retention`] can only mean the index
    /// itself does not exist yet, never "this API is not mounted".
    pub(crate) async fn list_indexes(&self) -> Result<Vec<IndexInfo>> {
        let resp = send_authenticated(&self.auth, "index list", |token| {
            with_token(self.http.get(&self.indexes_url), token)
        })
        .await?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| err("index list", e))?;
        if !status.is_success() {
            return Err(err(
                "index list",
                describe_admin_failure(status, &self.indexes_url, &text),
            ));
        }
        let feed: IndexFeed = serde_json::from_str(&text).map_err(|e| err("index list", e))?;
        Ok(feed
            .indexes
            .into_iter()
            .map(|e| IndexInfo {
                name: e.name,
                frozen_after_secs: e.settings.frozen_after_secs,
            })
            .collect())
    }

    /// Set one index's retention. `secs` is a TTL from event time, not a
    /// cutoff — aglake freezes a bucket once its newest event is that old.
    ///
    /// The update is partial: naming only `frozen_after_secs` leaves the
    /// index's other settings (`max_total_mb`, `frozen_dir`, `disabled`)
    /// exactly as they were, so this never clobbers something an operator set
    /// by hand.
    pub(crate) async fn set_retention(&self, index: &str, secs: u64) -> Result<()> {
        let url = format!("{}/{index}", self.indexes_url);
        let body = serde_json::json!({ "frozen_after_secs": secs });
        let resp = send_authenticated(&self.auth, "set retention", |token| {
            with_token(self.http.put(&url).json(&body), token)
        })
        .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        Err(err(
            "set retention",
            format!(
                "index {index}: {}",
                describe_admin_failure(status, &url, &text)
            ),
        ))
    }
}

/// Turn an admin-API failure into a message that names the fix.
///
/// The three statuses this surface actually produces mean different things to
/// whoever has to act on them, and a bare "403 from <url>" sends them looking
/// in the wrong place — so each one says what to do instead.
fn describe_admin_failure(status: reqwest::StatusCode, url: &str, body: &str) -> String {
    let detail = truncate(body);
    match status.as_u16() {
        401 => format!(
            "{status} from {url}: aglaked has a user catalog and this request \
             carried no session. Set storage.aglake.username / password (or \
             session_token) — the HEC token does not authenticate this API. \
             {detail}"
        ),
        403 => format!(
            "{status} from {url}: the configured aglake user is authenticated \
             but lacks the admin role, which every /api/v1/admin/ method \
             requires. {detail}"
        ),
        404 => format!(
            "{status} from {url}: no native admin API here. Heron needs the \
             face aglake added in 0.3, where it is always mounted — upstream \
             has since renumbered that line to 1.5, so any current build is \
             new enough. The management namespace it replaced was removed, not \
             aliased, so there is nothing to fall back to: upgrade aglaked, or \
             set storage.aglake.manage_retention = false and give the daemon \
             its own --retention-days. {detail}"
        ),
        _ => format!("{status} from {url}: {detail}"),
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

// ---------------------------------------------------------------------------
// Session auth + the native admin API
// ---------------------------------------------------------------------------

#[cfg(test)]
mod auth_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// One request as the mock saw it. `authorization` is the point of most of
    /// these tests: whether a token was sent, and whether it was a *fresh* one.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        authorization: Option<String>,
        body: String,
    }

    /// A scripted aglaked.
    ///
    /// Sessions are what make this worth mocking: they expire on the server's
    /// schedule, so the 401-then-retry path is reached in production by simply
    /// running for twelve hours, and never by anything a test can ask a real
    /// daemon to do.
    ///
    /// Login is handled internally rather than scripted, minting `session-1`,
    /// `session-2`, … in order — so an assertion on which token came back says
    /// unambiguously whether the client logged in again or reused what it had.
    struct MockAglake {
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl MockAglake {
        /// `script` answers non-login requests in order as `(status, body)`,
        /// falling back to an empty 200 once exhausted. `set_cookie` decides
        /// whether login hands back a session at all — `false` is the daemon
        /// with no user catalog.
        fn start(script: Vec<(u16, &'static str)>, set_cookie: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let seen_bg = Arc::clone(&seen);
            let next = Arc::new(AtomicUsize::new(0));
            let logins = Arc::new(AtomicUsize::new(0));

            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let seen_bg = Arc::clone(&seen_bg);
                    let next = Arc::clone(&next);
                    let logins = Arc::clone(&logins);
                    let script = script.clone();
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut request_line = String::new();
                        if reader.read_line(&mut request_line).is_err() {
                            return;
                        }
                        let mut parts = request_line.split_whitespace();
                        let method = parts.next().unwrap_or_default().to_string();
                        let path = parts.next().unwrap_or_default().to_string();

                        let mut len = 0usize;
                        let mut authorization = None;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                                break;
                            }
                            let lower = line.to_ascii_lowercase();
                            if let Some(v) = lower.strip_prefix("content-length:") {
                                len = v.trim().parse().unwrap_or(0);
                            }
                            if lower.starts_with("authorization:") {
                                authorization =
                                    line.splitn(2, ':').nth(1).map(|v| v.trim().to_string());
                            }
                        }
                        let mut body = vec![0u8; len];
                        let _ = reader.read_exact(&mut body);
                        let body = String::from_utf8_lossy(&body).to_string();

                        seen_bg.lock().unwrap().push(Seen {
                            method,
                            path: path.clone(),
                            authorization,
                            body,
                        });

                        let (code, payload, cookie) = if path == "/api/v1/auth/login" {
                            let n = logins.fetch_add(1, Ordering::SeqCst) + 1;
                            let cookie = set_cookie
                                .then(|| format!("aglake_session=session-{n}; Path=/; HttpOnly"));
                            (200, r#"{"authenticated":true}"#.to_string(), cookie)
                        } else {
                            let i = next.fetch_add(1, Ordering::SeqCst);
                            let (code, payload) = script.get(i).copied().unwrap_or((200, "{}"));
                            (code, payload.to_string(), None)
                        };

                        let cookie_header = cookie
                            .map(|c| format!("Set-Cookie: {c}\r\n"))
                            .unwrap_or_default();
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n\
                                 {cookie_header}Content-Length: {}\r\n\r\n{payload}",
                                payload.len()
                            )
                            .as_bytes(),
                        );
                    });
                }
            });
            Self { addr, seen }
        }

        fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        /// Requests that were not the login round-trip.
        fn api_requests(&self) -> Vec<Seen> {
            self.requests()
                .into_iter()
                .filter(|r| r.path != "/api/v1/auth/login")
                .collect()
        }

        fn config(&self) -> AglakeConfig {
            AglakeConfig {
                url: format!("http://{}", self.addr),
                request_timeout_secs: 5,
                search_timeout_secs: 5,
                ..Default::default()
            }
        }
    }

    fn management(config: &AglakeConfig) -> ManagementClient {
        let auth = Arc::new(AuthState::new(config).unwrap());
        ManagementClient::new(config, auth).unwrap()
    }

    /// The everyday deployment: aglaked with no user catalog, Heron with no
    /// credentials. Nothing is sent, and nothing tries to log in.
    #[tokio::test]
    async fn no_credentials_sends_no_authorization_and_never_logs_in() {
        let mock = MockAglake::start(vec![(200, r#"{"indexes":[]}"#)], true);
        let client = management(&mock.config());

        client.list_indexes().await.unwrap();

        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "a login would be a second request");
        assert_eq!(requests[0].path, "/api/v1/admin/indexes");
        assert_eq!(requests[0].authorization, None);
    }

    /// A session that aged out — the 12-hour TTL, or a daemon restart — is the
    /// expected 401, not a misconfiguration. It must resolve itself: log in
    /// again, retry once, succeed. The second request has to carry the *new*
    /// token, or the retry is just the same failing call.
    #[tokio::test]
    async fn a_stale_session_is_refreshed_and_the_request_retried() {
        let mock = MockAglake::start(
            vec![
                (401, r#"{"error":"authentication required"}"#),
                (200, r#"{"indexes":[{"name":"heron_spans","settings":{}}]}"#),
            ],
            true,
        );
        let config = AglakeConfig {
            username: "heron".into(),
            password: "secret".into(),
            ..mock.config()
        };
        let client = management(&config);

        let indexes = client
            .list_indexes()
            .await
            .expect("the retry should succeed");
        assert_eq!(indexes.len(), 1);

        let api = mock.api_requests();
        assert_eq!(api.len(), 2, "expected one retry");
        assert_eq!(api[0].authorization.as_deref(), Some("Bearer session-1"));
        assert_eq!(
            api[1].authorization.as_deref(),
            Some("Bearer session-2"),
            "the retry must use a freshly minted session"
        );
    }

    /// Credentials that are simply wrong fail the retry too. Reporting the
    /// second 401 rather than retrying forever is what keeps a bad password
    /// distinguishable from an expired session.
    #[tokio::test]
    async fn a_second_401_is_reported_rather_than_retried_again() {
        let mock = MockAglake::start(
            vec![
                (401, r#"{"error":"authentication required"}"#),
                (401, r#"{"error":"authentication required"}"#),
            ],
            true,
        );
        let config = AglakeConfig {
            username: "heron".into(),
            password: "wrong".into(),
            ..mock.config()
        };

        let e = management(&config)
            .list_indexes()
            .await
            .expect_err("a second 401 must surface");
        assert!(e.to_string().contains("401"), "{e}");
        assert_eq!(mock.api_requests().len(), 2, "exactly one retry");
    }

    /// A token supplied verbatim cannot be re-derived, so retrying a 401 on
    /// one would just repeat it. It is sent, and the failure is reported.
    #[tokio::test]
    async fn a_fixed_token_is_sent_but_never_refreshed() {
        let mock = MockAglake::start(vec![(401, r#"{"error":"nope"}"#)], true);
        let config = AglakeConfig {
            session_token: "preminted".into(),
            ..mock.config()
        };

        let e = management(&config).list_indexes().await.expect_err("401");
        assert!(e.to_string().contains("401"), "{e}");

        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "no retry, and no login attempt");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer preminted")
        );
    }

    /// A configured token wins over a configured password: it is the more
    /// specific instruction, and quietly logging in instead would ignore what
    /// the operator wrote.
    #[tokio::test]
    async fn a_fixed_token_takes_precedence_over_a_password() {
        let mock = MockAglake::start(vec![(200, r#"{"indexes":[]}"#)], true);
        let config = AglakeConfig {
            session_token: "preminted".into(),
            username: "heron".into(),
            password: "secret".into(),
            ..mock.config()
        };

        management(&config).list_indexes().await.unwrap();

        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "must not log in");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer preminted")
        );
    }

    /// Credentials configured against a daemon that has no user catalog: it
    /// answers login with 200 and no cookie. That is over-configuration, not a
    /// failure — the requests work bare — so it must proceed without a token.
    #[tokio::test]
    async fn credentials_against_a_no_auth_daemon_proceed_without_a_token() {
        let mock = MockAglake::start(vec![(200, r#"{"indexes":[]}"#)], false);
        let config = AglakeConfig {
            username: "heron".into(),
            password: "secret".into(),
            ..mock.config()
        };

        management(&config)
            .list_indexes()
            .await
            .expect("a cookie-less login must not fail the call");

        let api = mock.api_requests();
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].authorization, None);
    }

    /// One login serves every client and every subsequent call — a login is a
    /// round-trip and a session slot on the server.
    #[tokio::test]
    async fn a_session_is_established_once_and_reused() {
        let mock = MockAglake::start(vec![(200, r#"{"indexes":[]}"#); 3], true);
        let config = AglakeConfig {
            username: "heron".into(),
            password: "secret".into(),
            ..mock.config()
        };
        let auth = Arc::new(AuthState::new(&config).unwrap());
        let management = ManagementClient::new(&config, Arc::clone(&auth)).unwrap();
        let search = SearchClient::new(&config, auth).unwrap();

        management.list_indexes().await.unwrap();
        management.list_indexes().await.unwrap();
        search.ping().await.unwrap();

        let logins = mock
            .requests()
            .iter()
            .filter(|r| r.path == "/api/v1/auth/login")
            .count();
        assert_eq!(logins, 1, "the session must be shared, not re-established");
        for request in mock.api_requests() {
            assert_eq!(request.authorization.as_deref(), Some("Bearer session-1"));
        }
    }

    /// The native admin shape, which is not the EAI envelope this used to
    /// parse: `indexes[]` with the TTL nested under `settings`, and an absent
    /// TTL meaning "no per-index policy" rather than zero.
    #[tokio::test]
    async fn list_indexes_reads_the_native_shape() {
        let mock = MockAglake::start(
            vec![(
                200,
                r#"{"indexes":[
                    {"name":"heron_spans","builtin":false,"events":12,
                     "settings":{"disabled":false,"frozen_after_secs":86400}},
                    {"name":"main","builtin":true,"settings":{}}
                ]}"#,
            )],
            true,
        );

        let indexes = management(&mock.config()).list_indexes().await.unwrap();

        assert_eq!(indexes.len(), 2);
        assert_eq!(indexes[0].name, "heron_spans");
        assert_eq!(indexes[0].frozen_after_secs, Some(86_400));
        assert_eq!(indexes[1].name, "main");
        assert_eq!(
            indexes[1].frozen_after_secs, None,
            "an unset TTL is None, not 0 — 0 would mean freeze everything now"
        );
    }

    /// Retention goes out as a partial JSON update via PUT, so the index's
    /// other settings are left as whoever set them left them.
    #[tokio::test]
    async fn set_retention_puts_only_the_ttl() {
        let mock = MockAglake::start(vec![(200, r#"{"name":"heron_spans"}"#)], true);

        management(&mock.config())
            .set_retention("heron_spans", 604_800)
            .await
            .unwrap();

        let api = mock.api_requests();
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].method, "PUT");
        assert_eq!(api[0].path, "/api/v1/admin/indexes/heron_spans");

        let body: serde_json::Value = serde_json::from_str(&api[0].body).unwrap();
        assert_eq!(body["frozen_after_secs"], 604_800);
        assert_eq!(
            body.as_object().unwrap().len(),
            1,
            "sending anything else would overwrite settings Heron does not own"
        );
    }

    /// Each auth status gets a message naming its own fix — they have
    /// different ones, and "403 from <url>" sends an operator to the wrong
    /// place.
    #[tokio::test]
    async fn admin_failures_name_the_fix() {
        for (status, body, expected) in [
            (401, r#"{"error":"authentication required"}"#, "username"),
            (
                403,
                r#"{"error":"administrator role required"}"#,
                "admin role",
            ),
            (404, "", "added in 0.3"),
        ] {
            let mock = MockAglake::start(vec![(status, body)], true);
            let e = management(&mock.config())
                .list_indexes()
                .await
                .expect_err("non-2xx must be an error");
            assert!(
                e.to_string().contains(expected),
                "a {status} should mention {expected:?}: {e}"
            );
        }
    }

    /// The search face is behind the same guard, and its 401 is the one most
    /// likely to be misread — the HEC token looks like it should have covered
    /// it, and does not.
    #[tokio::test]
    async fn a_search_401_says_the_hec_token_is_not_the_answer() {
        let mock = MockAglake::start(vec![(401, r#"{"error":"authentication required"}"#)], true);
        let config = AglakeConfig {
            hec_token: "t".into(),
            ..mock.config()
        };
        let auth = Arc::new(AuthState::new(&config).unwrap());
        let search = SearchClient::new(&config, auth).unwrap();

        let e = search
            .search("| stats count", "0", "0")
            .await
            .expect_err("401");
        let message = e.to_string();
        assert!(message.contains("hec_token"), "{message}");
        assert!(message.contains("username"), "{message}");
    }

    #[test]
    fn the_session_cookie_is_read_out_of_a_full_set_cookie_header() {
        // Exercised through the mock above; this pins the parsing rules that
        // are easy to get wrong: attributes after the value, and other cookies
        // sharing the header set.
        let parse = |header: &str| -> Option<String> {
            let (name, rest) = header.split_once('=')?;
            (name.trim() == SESSION_COOKIE)
                .then(|| rest.split(';').next().unwrap_or(rest).to_string())
        };

        assert_eq!(
            parse("aglake_session=abc123; Path=/; HttpOnly; SameSite=Strict"),
            Some("abc123".to_string())
        );
        assert_eq!(parse("aglake_session=abc123"), Some("abc123".to_string()));
        assert_eq!(parse("other=abc123; Path=/"), None);
    }
}
