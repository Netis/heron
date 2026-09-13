//! `spans` — write path and point lookups.
//!
//! One call becomes up to two events in two different indexes: metadata in
//! `<prefix>_spans` and, when there is anything to store, bodies+headers in
//! `<prefix>_bodies`.
//!
//! The metadata event is ordered **before** its body event in the batch. HEC
//! ingests a batch in order and stops at the first malformed event, so a
//! partial failure can leave a span without its body (the console then shows
//! the body as unavailable) but never a body without its span.

use std::collections::{HashMap, HashSet};

use futures::future::try_join_all;
use h_common::error::Result;
use h_common::process::ProcessInfo;
use h_llm::model::LlmCall;
use h_storage::query::{SpanDetail, SpanListItem, SpansPage, SpansQuery, TraceSpanItem};

use crate::read::Sort;
use crate::rows::{span_events, BodyEvent, Envelope, SpanEvent, ST_BODY, ST_SPAN};
use crate::spl::{self, in_list, match_term, Search, ID_CHUNK};
use crate::AglakeBackend;

/// Microseconds → milliseconds, the unit the detail and trace-span types use.
/// (`HttpExchangeDetail` is the one exception and stays in microseconds.)
pub(crate) fn ms(us: i64) -> i64 {
    us / 1000
}

/// Ceiling on the id set used to resolve the traces `server_port` filter.
const PORT_FILTER_ID_CAP: usize = 50_000;

/// Slack on each side of a body lookup's window, in microseconds. A body is
/// written with its span's own timestamp, so this covers nothing but the
/// second-boundary rounding in the search bounds.
const BODY_WINDOW_PAD_US: i64 = 2_000_000;

/// The `_time` extent to look for these spans' bodies in.
///
/// Bodies are written with the same timestamp as the span they belong to, so
/// the spans — already in hand by the time bodies are wanted — bound the search
/// exactly. An empty slice yields an unbounded window, which is only reachable
/// with nothing to look up.
fn body_window(events: &[SpanEvent]) -> (String, String) {
    let (Some(min), Some(max)) = (
        events.iter().map(|e| e.ts_us).min(),
        events.iter().map(|e| e.ts_us).max(),
    ) else {
        return ("0".to_string(), "0".to_string());
    };
    (
        spl::epoch_secs(min.saturating_sub(BODY_WINDOW_PAD_US)),
        spl::epoch_secs(max.saturating_add(BODY_WINDOW_PAD_US)),
    )
}

fn span_list_item(e: SpanEvent) -> SpanListItem {
    SpanListItem {
        id: e.id,
        source_id: e.source_id,
        request_time: ms(e.ts_us),
        wire_api: e.wire_api,
        model: e.model,
        status_code: e.status_code,
        is_stream: e.is_stream,
        finish_reason: e.finish_reason,
        ttft_ms: e.ttft_ms,
        e2e_latency_ms: e.e2e_latency_ms,
        input_tokens: e.input_tokens,
        output_tokens: e.output_tokens,
        // Stored, not derived: the SQL backends compute this from
        // `response_body`, which means the list query has to read bodies it
        // otherwise never touches. Here they live in a different index.
        tokens_estimated: e.tokens_estimated,
        client_ip: e.client_ip,
        server_ip: e.server_ip,
        server_port: e.server_port,
        request_path: e.request_path,
        is_agent_request: e.is_agent_request,
        tool_surface: e.tool_surface,
        agent_topology: e.agent_topology,
        tool_call_count: e.tool_call_count,
        tool_names: h_storage::convert::parse_json_string_list(Some(&e.tool_names_json)),
        process: e.process_pid.map(|pid| ProcessInfo {
            pid,
            comm: e.process_comm.clone().unwrap_or_default(),
            exe: e.process_exe.clone(),
        }),
    }
}

/// `sort_by` values the API may send, and the SPL key each maps to. `num()`
/// is required because extracted field values are strings; without it the
/// comparison would be lexicographic and `9` would sort after `100`.
const SPAN_SORT: &[(&str, &str)] = &[
    ("request_time", "num(ts_us)"),
    ("status_code", "num(status_code)"),
    ("ttft_ms", "num(ttft_ms)"),
    ("e2e_latency_ms", "num(e2e_latency_ms)"),
    ("input_tokens", "num(input_tokens)"),
    ("output_tokens", "num(output_tokens)"),
];

impl AglakeBackend {
    pub(crate) async fn write_spans(&self, calls: Vec<LlmCall>) -> Result<()> {
        if calls.is_empty() {
            return Ok(());
        }
        let mut events = Vec::with_capacity(calls.len() * 2);
        for call in &calls {
            let (meta, body) = span_events(call, self.store_bodies);
            let ts = call.request_time;
            let host = call.source_id.clone();

            match Envelope::new(ts, &host, ST_SPAN, &self.ix.spans, meta).encode() {
                Ok(s) => events.push(s),
                Err(e) => {
                    // Encoding a span cannot normally fail; if it does, drop
                    // just this one rather than the whole batch.
                    tracing::error!(
                        target: "aglake::write", id = %call.id, error = %e,
                        "aglake: failed to encode span event; skipping it"
                    );
                    continue;
                }
            }
            if let Some(body) = body {
                match crate::rows::raw_envelope(ts, &host, ST_BODY, &self.ix.bodies, &body) {
                    Ok(s) => events.push(s),
                    Err(e) => tracing::error!(
                        target: "aglake::write", id = %call.id, error = %e,
                        "aglake: failed to encode body event; span metadata still written"
                    ),
                }
            }
        }
        self.hec.send(events).await
    }

    pub(crate) async fn query_spans(&self, query: &SpansQuery) -> Result<SpansPage> {
        let sort = Sort::new(
            &query.sort_by,
            &query.sort_order,
            SPAN_SORT,
            &["num(ts_us)", "str(id)"],
        )?;

        let mut s = Search::new(&self.ix.spans, ST_SPAN);
        s.any_of("wire_api", &query.filter.wire_apis);
        s.any_of("model", &query.filter.models);
        s.any_of("server_ip", &query.filter.server_ips);
        s.any_of("client_ip", &query.client_ips);
        s.any_of("finish_reason", &query.finish_reasons);
        s.any_of_nums("status_code", &query.status_codes);
        s.any_of_nums("server_port", &query.server_ports);
        if let Some(sub) = query
            .request_path_contains
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            s.contains("request_path", sub);
        }
        if let Some(stream) = query.is_stream {
            // The 0/1 twin, not the boolean: an exact posting rather than a
            // match against the string "true".
            s.eq_num("strm", u8::from(stream));
        }

        let (events, total) = self
            .fetch_page::<SpanEvent>(
                "query_spans",
                &s,
                &sort,
                query.page,
                query.page_size,
                &query.time_range,
            )
            .await?;

        Ok(SpansPage {
            total,
            items: events.into_iter().map(span_list_item).collect(),
        })
    }

    pub(crate) async fn query_span_by_id(&self, id: &str) -> Result<Option<SpanDetail>> {
        let ix = &self.ix.spans;
        let Some(term) = match_term("id", id) else {
            // An id containing `*` cannot be a term without becoming a glob,
            // and Heron never mints one. Treating it as "not found" is both
            // correct and refuses to run a full-index wildcard scan.
            return Ok(None);
        };
        let search = format!("search index={ix} sourcetype={ST_SPAN} {term}");
        let Some(e) = self
            .fetch_raw_by_id::<SpanEvent>("query_span_by_id", &search, 1, id)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };

        let body = if e.has_body {
            self.fetch_bodies(&[id.to_string()], body_window(std::slice::from_ref(&e)))
                .await?
                .remove(id)
        } else {
            None
        };
        let (request_body, response_body, request_headers, response_headers) = match body {
            Some(b) => (
                b.request_body,
                b.response_body,
                b.request_headers,
                b.response_headers,
            ),
            None => (None, None, None, None),
        };

        Ok(Some(SpanDetail {
            id: e.id,
            source_id: e.source_id,
            request_time: ms(e.ts_us),
            response_time: e.resp_us.map(ms),
            complete_time: e.done_us.map(ms),
            wire_api: e.wire_api,
            model: e.model,
            api_type: e.api_type,
            is_stream: e.is_stream,
            request_path: e.request_path,
            status_code: e.status_code,
            finish_reason: e.finish_reason,
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            total_tokens: e.total_tokens,
            tokens_estimated: e.tokens_estimated,
            ttft_ms: e.ttft_ms,
            e2e_latency_ms: e.e2e_latency_ms,
            response_id: e.response_id,
            client_ip: e.client_ip,
            client_port: e.client_port,
            server_ip: e.server_ip,
            server_port: e.server_port,
            request_body,
            response_body,
            request_headers,
            response_headers,
            is_agent_request: e.is_agent_request,
            tool_surface: e.tool_surface,
            agent_topology: e.agent_topology,
            tool_call_count: e.tool_call_count,
            tool_names: h_storage::convert::parse_json_string_list(Some(&e.tool_names_json)),
            process: e.process_pid.map(|pid| ProcessInfo {
                pid,
                comm: e.process_comm.clone().unwrap_or_default(),
                exe: e.process_exe.clone(),
            }),
        }))
    }

    /// Fetch calls by id list.
    ///
    /// Shared by `query_trace_spans` (ids from the persisted trace) and
    /// `query_spans_by_ids` (ids from the in-memory active-turn registry).
    /// Calls not yet flushed simply do not come back, same as on the SQL
    /// backends.
    ///
    /// `window` is the caller's time bounds when it has better information
    /// than the ids do — `query_trace_spans` knows the turn's real start and
    /// end, which beats anything derivable from a UUID and stays correct
    /// under pcap replay.
    pub(crate) async fn read_spans_by_ids(
        &self,
        span_ids: &[String],
        include_bodies: bool,
        window: Option<(String, String)>,
    ) -> Result<Vec<TraceSpanItem>> {
        if span_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Chunks run concurrently. The whole point of a small `ID_CHUNK` is
        // that N id probes overlap instead of queueing; `SearchClient`'s
        // permits are what bound the fan-out.
        let window = window.as_ref().map(|(e, l)| (e.as_str(), l.as_str()));
        let per_chunk = span_ids.chunks(ID_CHUNK).map(|chunk| async move {
            let ix = &self.ix.spans;
            let Some(list) = in_list("id", chunk) else {
                return Result::<Vec<SpanEvent>>::Ok(Vec::new());
            };
            let search = format!("search index={ix} sourcetype={ST_SPAN} {list}");
            match window {
                Some((earliest, latest)) => {
                    self.fetch_raw("read_spans_by_ids", &search, chunk.len(), earliest, latest)
                        .await
                }
                // No caller window: bound by the ids themselves, unbounded on
                // a miss. `chunk[0]` is representative — a trace's calls are
                // minted within seconds of each other, well inside the skew.
                None => {
                    self.fetch_raw_by_id("read_spans_by_ids", &search, chunk.len(), &chunk[0])
                        .await
                }
            }
        });
        let mut events: Vec<SpanEvent> = Vec::with_capacity(span_ids.len());
        for found in try_join_all(per_chunk).await? {
            events.extend(found);
        }

        // Ordering is done here rather than in SPL. The list is bounded by the
        // caller's id set, the values are already decoded as integers, and
        // sorting in the query would mean comparing `done_us` as an extracted
        // string field. The trailing `id` key is a deterministic tie-break the
        // SQL backends lack: equal timestamps there order arbitrarily.
        events.sort_by(|a, b| {
            a.ts_us
                .cmp(&b.ts_us)
                .then(a.done_us.cmp(&b.done_us))
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut bodies = if include_bodies {
            let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
            // Exact window, taken from the spans just read: a body carries the
            // same `_time` as its span, so their extent is the only bound the
            // lookup needs. Deriving one from the ids instead would be a guess
            // — and a guess that fails partially, dropping the bodies of spans
            // outside it while the ones inside make the query look successful.
            let span = body_window(&events);
            self.fetch_bodies(&ids, span).await?
        } else {
            HashMap::new()
        };

        Ok(events
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let b = bodies.remove(&e.id).unwrap_or_default();
                TraceSpanItem {
                    id: e.id,
                    sequence: (i as u32) + 1,
                    request_time: ms(e.ts_us),
                    response_time: e.resp_us.map(ms),
                    complete_time: e.done_us.map(ms),
                    wire_api: e.wire_api,
                    model: e.model,
                    status_code: e.status_code,
                    is_stream: e.is_stream,
                    finish_reason: e.finish_reason,
                    ttft_ms: e.ttft_ms,
                    e2e_latency_ms: e.e2e_latency_ms,
                    input_tokens: e.input_tokens,
                    output_tokens: e.output_tokens,
                    // Precomputed at write time, so this is the same answer
                    // with and without bodies. The SQL backends derive it from
                    // the response body and therefore disagree in lite mode.
                    tokens_estimated: e.tokens_estimated,
                    request_path: e.request_path,
                    client_ip: e.client_ip,
                    client_port: e.client_port,
                    server_ip: e.server_ip,
                    server_port: e.server_port,
                    request_body: b.request_body,
                    response_body: b.response_body,
                    request_headers: b.request_headers,
                    response_headers: b.response_headers,
                }
            })
            .collect())
    }

    /// Resolve span ids for a set of server ports — the indirection
    /// `query_traces` needs, since a trace carries no port of its own.
    ///
    /// Bounded on purpose. Ports are low-cardinality, so this filter is
    /// usually barely selective and the id set can be enormous; past the cap
    /// there is no honest answer to give, and a truncated one would look like
    /// a complete page. See the crate docs.
    pub(crate) async fn span_ids_on_ports(
        &self,
        ports: &[u16],
        range: &h_storage::query::TimeRange,
    ) -> Result<Vec<String>> {
        let mut s = Search::new(&self.ix.spans, ST_SPAN);
        s.any_of_nums("server_port", ports);
        let limit = PORT_FILTER_ID_CAP + 1;
        let spl = format!("{} | head {limit} | table id", s.build());
        let rows = self
            .search
            .search(
                &spl,
                &crate::spl::epoch_secs(range.start_us),
                &crate::spl::epoch_secs(range.end_us),
            )
            .await?
            .rows();
        if rows.len() > PORT_FILTER_ID_CAP {
            return Err(h_common::error::AppError::Storage(format!(
                "aglake backend: the server_port filter on traces matches more \
                 than {PORT_FILTER_ID_CAP} calls in this window. A trace stores \
                 no port of its own, so the filter is resolved through its \
                 calls; narrow the time range or drop the port filter."
            )));
        }
        Ok(rows
            .into_iter()
            .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect())
    }

    /// Second hop of the no-JOIN read: bodies for a set of span ids, keyed by
    /// span id. Missing ids are simply absent from the map.
    ///
    /// Matched by raw term, not by field — see [`spl::body_term`] for why a
    /// field predicate silently matches nothing here. The term is anchored on
    /// the leading key, and the deserialized `span_id` is checked against the
    /// requested set anyway, so nothing that merely quotes an id can be
    /// mistaken for the body of that span.
    ///
    /// `window` is the `_time` extent of the spans these bodies belong to, and
    /// the caller always knows it, because it has just read those spans. That
    /// matters more than it looks: the id-derived window is a *hint* whose miss
    /// path is "retry unbounded", and a hint that half-misses over a set of ids
    /// returns a partial answer that no caller can distinguish from a complete
    /// one.
    async fn fetch_bodies(
        &self,
        span_ids: &[String],
        window: (String, String),
    ) -> Result<HashMap<String, BodyEvent>> {
        let (earliest, latest) = window;
        // The dominant cost of opening a large turn, and the reason the chunks
        // are concurrent: bodies are matched by raw term, one term per span,
        // over the largest index Heron writes.
        let (earliest, latest) = (earliest.as_str(), latest.as_str());
        let per_chunk = span_ids.chunks(ID_CHUNK).map(|chunk| async move {
            let ix = &self.ix.bodies;
            let Some(terms) = spl::body_terms(chunk) else {
                return Result::<Vec<BodyEvent>>::Ok(Vec::new());
            };
            let search = format!("search index={ix} sourcetype={ST_BODY} {terms}");
            let found: Vec<BodyEvent> = self
                .fetch_raw("fetch_bodies", &search, chunk.len(), earliest, latest)
                .await?;
            // A raw-term match can land on an event that merely quotes the id,
            // so the decoded `span_id` is checked against this chunk's set.
            let wanted: HashSet<&str> = chunk.iter().map(String::as_str).collect();
            Ok(found
                .into_iter()
                .filter(|b| wanted.contains(b.span_id.as_str()))
                .collect())
        });
        let mut out = HashMap::with_capacity(span_ids.len());
        for found in try_join_all(per_chunk).await? {
            for b in found {
                out.insert(b.span_id.clone(), b);
            }
        }
        Ok(out)
    }
}
