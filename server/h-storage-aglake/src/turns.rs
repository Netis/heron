//! `traces` — write path and the two-step trace reads.
//!
//! The no-JOIN rule shapes both reads here: a trace's calls are found by
//! reading `span_ids_json` off the trace, then fetching those ids from the
//! spans index. Step two inherits the turn's own `[start, end]` as its time
//! window, which is both tighter and more reliable than anything derivable
//! from the ids.

use h_common::error::Result;
use h_storage::convert::parse_json_string_list;
use h_storage::query::{TraceDetail, TraceListItem, TraceSpanItem, TracesPage, TracesQuery};
use h_turn::Trace;

use crate::calls::ms;
use crate::read::Sort;
use crate::rows::{trace_event, Envelope, TraceEvent, ST_TRACE};
use crate::spl::{match_term, Search};
use crate::AglakeBackend;

const TRACE_SORT: &[(&str, &str)] = &[
    ("start_time", "num(ts_us)"),
    ("end_time", "num(end_us)"),
    ("duration_ms", "num(duration_ms)"),
    ("call_count", "num(call_count)"),
    ("total_input_tokens", "num(total_input_tokens)"),
    ("total_output_tokens", "num(total_output_tokens)"),
];

impl AglakeBackend {
    pub(crate) async fn write_traces(&self, turns: Vec<Trace>) -> Result<()> {
        if turns.is_empty() {
            return Ok(());
        }
        let mut events = Vec::with_capacity(turns.len());
        for t in &turns {
            let e = trace_event(t);
            match Envelope::new(t.start_time_us, &t.source_id, ST_TRACE, &self.ix.traces, e)
                .encode()
            {
                Ok(s) => events.push(s),
                Err(err) => tracing::error!(
                    target: "aglake::write", turn_id = %t.turn_id, error = %err,
                    "aglake: failed to encode trace event; skipping it"
                ),
            }
        }
        self.hec.send(events).await
    }

    pub(crate) async fn query_traces(&self, query: &TracesQuery) -> Result<TracesPage> {
        let sort = Sort::new(
            &query.sort_by,
            &query.sort_order,
            TRACE_SORT,
            &["num(ts_us)", "str(turn_id)"],
        )?;

        let mut s = Search::new(&self.ix.traces, ST_TRACE);
        s.any_of("wire_api", &query.filter.wire_apis);
        // `models_used` is stored as an array, and a match against an array
        // field is an any-element match — the same semantics as the SQL
        // backends' `hasAny` / `list_has_any`.
        s.any_of("models_used", &query.filter.models);
        s.any_of("server_ip", &query.filter.server_ips);
        s.any_of("client_ip", &query.client_ips);
        s.any_of("status", &query.statuses);
        s.any_of("agent_kind", &query.agent_kinds);
        if !query.include_proxy_hops {
            // Precomputed at write time; see `TraceEvent::proxy_hidden`.
            s.eq_num("proxy_hidden", 0);
        }
        if !query.server_ports.is_empty() {
            // A trace has no port of its own, so this resolves through its
            // first call, exactly as the SQL backends do — they just get to
            // express it as a subquery.
            let ids = self
                .span_ids_on_ports(&query.server_ports, &query.time_range)
                .await?;
            if ids.is_empty() {
                return Ok(TracesPage {
                    total: 0,
                    items: Vec::new(),
                });
            }
            s.any_of("first_span_id", &ids);
        }

        let (events, total) = self
            .fetch_page::<TraceEvent>(
                "query_traces",
                &s,
                &sort,
                query.page,
                query.page_size,
                &query.time_range,
            )
            .await?;

        Ok(TracesPage {
            total,
            items: events.into_iter().map(trace_list_item).collect(),
        })
    }

    /// Read one trace by its turn id.
    async fn trace_event_by_id(&self, turn_id: &str) -> Result<Option<TraceEvent>> {
        let ix = &self.ix.traces;
        let Some(term) = match_term("turn_id", turn_id) else {
            // A provider-supplied turn id containing `*` would become a glob
            // and could match a different turn. Refusing is the safe answer.
            tracing::warn!(
                target: "aglake::read",
                "aglake: turn id contains a wildcard character and cannot be \
                 looked up as a search term; treating it as not found"
            );
            return Ok(None);
        };
        let search = format!("search index={ix} sourcetype={ST_TRACE} {term}");
        Ok(self
            .fetch_raw_by_id::<TraceEvent>("query_trace_by_id", &search, 1, turn_id)
            .await?
            .into_iter()
            .next())
    }

    pub(crate) async fn query_trace_by_id(&self, turn_id: &str) -> Result<Option<TraceDetail>> {
        let Some(e) = self.trace_event_by_id(turn_id).await? else {
            return Ok(None);
        };
        Ok(Some(trace_detail(e)))
    }

    pub(crate) async fn query_trace_spans(
        &self,
        turn_id: &str,
        include_bodies: bool,
    ) -> Result<Vec<TraceSpanItem>> {
        let Some(t) = self.trace_event_by_id(turn_id).await? else {
            return Ok(Vec::new());
        };
        let span_ids = parse_json_string_list(Some(&t.span_ids_json));
        // The turn's own window, widened by the configured skew. Far better
        // than an id-derived guess: it is the real capture time, so it holds
        // under pcap replay as well as live capture.
        let window = self.window(t.ts_us, t.end_us);
        self.read_spans_by_ids(&span_ids, include_bodies, Some(window))
            .await
    }
}

fn trace_list_item(e: TraceEvent) -> TraceListItem {
    let models_used = parse_json_string_list(Some(&e.models_used_json));
    TraceListItem {
        turn_id: e.turn_id,
        source_id: e.source_id,
        session_id: e.session_id,
        start_time: ms(e.ts_us),
        end_time: ms(e.end_us),
        duration_ms: e.duration_ms,
        wire_api: e.wire_api,
        agent_kind: e.agent_kind,
        client_ip: e.client_ip,
        server_ip: e.server_ip,
        primary_model: models_used.first().cloned(),
        models_used,
        call_count: e.call_count,
        total_input_tokens: e.total_input_tokens,
        total_output_tokens: e.total_output_tokens,
        status: e.status,
        final_finish_reason: e.final_finish_reason,
        user_input_preview: e.user_input_preview,
        final_answer_preview: e.final_answer_preview,
        proxy_role: e.proxy_role,
        proxy_peer_turn_id: e.proxy_peer_turn_id,
        proxy_peer_turn_ids: e
            .proxy_peer_turn_ids_json
            .as_deref()
            .map(|s| parse_json_string_list(Some(s))),
        tool_surfaces: parse_json_string_list(Some(&e.tool_surfaces_json)),
        tool_call_total: e.tool_call_total,
        agent_topology: e.agent_topology,
        suspicious_skills: serde_json::from_str(&e.suspicious_skills_json).unwrap_or_default(),
    }
}

fn trace_detail(e: TraceEvent) -> TraceDetail {
    TraceDetail {
        turn_id: e.turn_id,
        source_id: e.source_id,
        session_id: e.session_id,
        wire_api: e.wire_api,
        agent_kind: e.agent_kind,
        client_ip: e.client_ip,
        server_ip: e.server_ip,
        start_time: ms(e.ts_us),
        end_time: ms(e.end_us),
        duration_ms: e.duration_ms,
        call_count: e.call_count,
        // From the `_json` twin, not the multivalue field: a one-element
        // multivalue collapses to a scalar on the way out, and order is not
        // guaranteed.
        models_used: parse_json_string_list(Some(&e.models_used_json)),
        subagents_used: parse_json_string_list(Some(&e.subagents_used_json)),
        total_input_tokens: e.total_input_tokens,
        total_output_tokens: e.total_output_tokens,
        total_cache_read_input_tokens: e.total_cache_read_input_tokens,
        total_cache_creation_input_tokens: e.total_cache_creation_input_tokens,
        total_cost_usd: e.total_cost_usd,
        status: e.status,
        final_finish_reason: e.final_finish_reason,
        user_call_id: e.user_call_id,
        // Same divergence as the ClickHouse backend: the full user input and
        // final answer would mean re-running the agent profile extractor over
        // the referenced call bodies. The stored previews are what we have,
        // and a truncated one stays truncated.
        user_input: e.user_input_preview,
        final_call_id: e.final_call_id,
        final_answer: e.final_answer_preview,
        span_ids: parse_json_string_list(Some(&e.span_ids_json)),
        metadata: serde_json::from_str(&e.metadata_json).ok(),
        tool_surfaces: parse_json_string_list(Some(&e.tool_surfaces_json)),
        tool_call_total: e.tool_call_total,
        agent_topology: e.agent_topology,
        suspicious_skills: serde_json::from_str(&e.suspicious_skills_json).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::fixtures;

    /// The full write → read shape, without a server: encode a trace, decode
    /// it the way a read would, and check the detail comes back intact.
    #[test]
    fn trace_survives_encode_then_detail_decode() {
        let t = fixtures::full_trace();
        let encoded = serde_json::to_string(&trace_event(&t)).unwrap();
        let back: TraceEvent = serde_json::from_str(&encoded).unwrap();
        let d = trace_detail(back);

        assert_eq!(d.turn_id, t.turn_id);
        assert_eq!(d.session_id, "sess-1");
        assert_eq!(d.status, "complete");
        // Detail timestamps are milliseconds; the event stores microseconds.
        assert_eq!(d.start_time, t.start_time_us / 1000);
        assert_eq!(d.end_time, t.end_time_us / 1000);
        assert_eq!(d.models_used, vec!["claude-sonnet".to_string()]);
        assert_eq!(d.subagents_used, vec!["explore".to_string()]);
        assert_eq!(d.span_ids, vec!["call-a".to_string(), "call-b".to_string()]);
        assert_eq!(d.tool_surfaces, vec!["function_call".to_string()]);
        assert_eq!(d.total_cost_usd, Some(0.0123));
        assert_eq!(d.agent_topology.as_deref(), Some("single_agent"));
        assert_eq!(d.metadata.unwrap()["proxy"]["role"], "outer");
        assert!(d.suspicious_skills.is_empty());
    }

    /// A one-element `models_used` is the case that would come back as a bare
    /// string if it were read from the multivalue field instead of the JSON
    /// twin.
    #[test]
    fn single_element_lists_stay_lists() {
        let mut t = fixtures::full_trace();
        t.models_used = vec!["only-one".into()];
        t.subagents_used = vec![];
        let d = trace_detail(
            serde_json::from_str(&serde_json::to_string(&trace_event(&t)).unwrap()).unwrap(),
        );
        assert_eq!(d.models_used, vec!["only-one".to_string()]);
        assert!(d.subagents_used.is_empty());
    }
}
