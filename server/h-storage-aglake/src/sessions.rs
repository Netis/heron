//! Session-scoped queries. Sessions are a view over traces grouped by
//! `(source_id, session_id)` — there is no session entity of their own.
//!
//! # Why the window and the aggregate are two different queries
//!
//! A session is *included* by its turns in the requested window, but its
//! numbers are computed over its **whole lifetime**. Those are different row
//! sets, so this runs as two steps: find the keys in the window, then
//! aggregate those keys unbounded. The SQL backends do the same thing for the
//! same reason.
//!
//! # Why the time window is wider than it looks
//!
//! Inclusion is decided by `end_us` — a turn counts if it *ended* in the
//! window. `_time` carries `start_us`, so a long turn that started before the
//! window can still belong to it. The search bounds are therefore widened
//! backwards by `trace_time_skew_hours` (bucket pruning only, never the
//! answer), and a `| where` on `end_us` decides membership exactly.
//!
//! # Cursor comparison happens in Rust
//!
//! The cursor is a `(last_turn_at_ms, source_id, session_id)` tuple compared
//! lexicographically. SPL has no tuple comparison, and hand-expanding it into
//! nested string `OR`s is both unreadable and easy to get subtly wrong, so SPL
//! only produces the ordered key list and the comparison is done here.
//! `max_sessions_scan` bounds what that can cost.

use h_common::error::Result;
use h_storage::convert::parse_json_string_list;
use h_storage::query::*;

use crate::calls::ms;
use crate::read::{decode_rows, Sort};
use crate::rows::{TraceEvent, ST_TRACE};
use crate::spl::{self, Search};
use crate::AglakeBackend;

/// One `(source_id, session_id)` key with its windowed max end time.
struct Key {
    source_id: String,
    session_id: String,
    last_us: i64,
}

/// Full-lifetime totals for one session, folded from its turns.
#[derive(Default)]
struct Agg {
    first_us: i64,
    last_us: i64,
    turn_count: u64,
    call_count: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_input_tokens: u64,
    total_cache_creation_input_tokens: u64,
    total_cost_usd: Option<f64>,
    agent_kind: String,
    /// The earliest turn's preview, by `start_us`.
    first_start_us: i64,
    first_user_input_preview: Option<String>,
    first_user_call_id: Option<String>,
}

impl Agg {
    fn fold(&mut self, t: &TraceEvent) {
        if self.turn_count == 0 || t.ts_us < self.first_us {
            self.first_us = t.ts_us;
        }
        if self.turn_count == 0 || t.end_us > self.last_us {
            self.last_us = t.end_us;
        }
        // The opening prompt of the session is the earliest turn's, so this
        // tracks min(start) separately from the preview it carries.
        if self.turn_count == 0 || t.ts_us < self.first_start_us {
            self.first_start_us = t.ts_us;
            self.first_user_input_preview = t.user_input_preview.clone();
            self.first_user_call_id = t.user_call_id.clone();
        }
        if self.agent_kind.is_empty() {
            self.agent_kind = t.agent_kind.clone();
        }
        self.turn_count += 1;
        self.call_count += t.call_count as u64;
        self.total_input_tokens += t.total_input_tokens;
        self.total_output_tokens += t.total_output_tokens;
        self.total_cache_read_input_tokens += t.total_cache_read_input_tokens;
        self.total_cache_creation_input_tokens += t.total_cache_creation_input_tokens;
        if let Some(c) = t.total_cost_usd {
            *self.total_cost_usd.get_or_insert(0.0) += c;
        }
    }
}

const SESSION_TRACE_SORT: &[(&str, &str)] = &[("start_time", "num(ts_us)")];

impl AglakeBackend {
    pub(crate) async fn query_sessions(&self, query: &SessionListQuery) -> Result<SessionsPage> {
        let page_size = query.page_size.max(1) as usize;

        let mut s = Search::new(&self.ix.traces, ST_TRACE);
        if let Some(sid) = &query.source_id {
            s.any_of("source_id", std::slice::from_ref(sid));
        }
        s.any_of("agent_kind", &query.agent_kinds);
        s.range("end_us", query.time_range.start_us, query.time_range.end_us);

        // One row per session in the window, ordered by the same key the
        // cursor uses.
        let scan_cap = self.max_sessions_scan;
        let spl = format!(
            "{} | stats max(end_us) as last_us by source_id, session_id \
             | sort {scan_cap} -num(last_us), -str(source_id), -str(session_id) \
             | table source_id, session_id, last_us",
            s.build()
        );
        // Widened backwards only: a turn that ended in the window may have
        // started before it.
        let (earliest, _) = self.window(query.time_range.start_us, 0);
        let rows = self
            .search
            .search(&spl, &earliest, &spl::epoch_secs(query.time_range.end_us))
            .await?
            .rows();

        if rows.len() as u64 >= scan_cap {
            return Err(h_common::error::AppError::Storage(format!(
                "aglake backend: more than {scan_cap} sessions in this window \
                 (storage.aglake.max_sessions_scan); narrow the time range"
            )));
        }

        let mut keys: Vec<Key> = rows
            .into_iter()
            .filter_map(|r| {
                Some(Key {
                    source_id: r.get("source_id")?.as_str()?.to_string(),
                    session_id: r.get("session_id")?.as_str()?.to_string(),
                    last_us: as_i64(r.get("last_us"))?,
                })
            })
            .collect();

        // Cursor: strictly-less-than on (last_turn_at_ms, source_id,
        // session_id), descending.
        //
        // The cursor stores milliseconds while the sort key is microseconds,
        // so the comparison is against `cursor_ms * 1000`. That is exactly
        // what both SQL backends do, and it is reproduced rather than fixed so
        // the three backends page identically — but it does mean a session
        // whose windowed end falls in the sub-millisecond remainder of the
        // previous page's last row is skipped. Narrow, shared, and worth
        // fixing across all three at once.
        if let Some(c) = &query.cursor {
            let cut = c.last_turn_at_ms.saturating_mul(1000);
            keys.retain(|k| {
                (k.last_us, k.source_id.as_str(), k.session_id.as_str())
                    < (cut, c.source_id.as_str(), c.session_id.as_str())
            });
        }

        let has_more = keys.len() > page_size;
        keys.truncate(page_size);
        if keys.is_empty() {
            return Ok(SessionsPage {
                items: Vec::new(),
                next_cursor: None,
            });
        }

        // Step 2: whole-lifetime aggregate for just this page's sessions.
        // Unbounded on purpose — a session's earlier turns are outside the
        // requested window by definition.
        let session_ids: Vec<String> = keys.iter().map(|k| k.session_id.clone()).collect();
        let by_key = self
            .session_aggregates(&session_ids, query.source_id.as_deref())
            .await?;

        let mut items = Vec::with_capacity(keys.len());
        for k in &keys {
            let Some(a) = by_key.get(&(k.source_id.clone(), k.session_id.clone())) else {
                continue;
            };
            items.push(SessionListItem {
                source_id: k.source_id.clone(),
                session_id: k.session_id.clone(),
                agent_kind: a.agent_kind.clone(),
                last_turn_at_in_window: ms(k.last_us),
                first_turn_at: ms(a.first_us),
                last_turn_at: ms(a.last_us),
                turn_count: a.turn_count,
                call_count: a.call_count,
                total_input_tokens: a.total_input_tokens,
                total_output_tokens: a.total_output_tokens,
                total_cache_read_input_tokens: a.total_cache_read_input_tokens,
                total_cache_creation_input_tokens: a.total_cache_creation_input_tokens,
                total_cost_usd: a.total_cost_usd,
                first_user_input_preview: a.first_user_input_preview.clone(),
                first_user_call_id: a.first_user_call_id.clone(),
            });
        }

        let next_cursor = if has_more {
            items.last().map(|it| {
                encode_session_cursor(&SessionListCursor {
                    last_turn_at_ms: it.last_turn_at_in_window,
                    source_id: it.source_id.clone(),
                    session_id: it.session_id.clone(),
                })
            })
        } else {
            None
        };

        Ok(SessionsPage { items, next_cursor })
    }

    pub(crate) async fn query_session_by_id(
        &self,
        source_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionDetail>> {
        let by_key = self
            .session_aggregates(
                std::slice::from_ref(&session_id.to_string()),
                Some(source_id),
            )
            .await?;
        Ok(by_key
            .get(&(source_id.to_string(), session_id.to_string()))
            .map(|a| SessionDetail {
                source_id: source_id.to_string(),
                session_id: session_id.to_string(),
                agent_kind: a.agent_kind.clone(),
                first_turn_at: ms(a.first_us),
                last_turn_at: ms(a.last_us),
                turn_count: a.turn_count,
                call_count: a.call_count,
                total_input_tokens: a.total_input_tokens,
                total_output_tokens: a.total_output_tokens,
                total_cache_read_input_tokens: a.total_cache_read_input_tokens,
                total_cache_creation_input_tokens: a.total_cache_creation_input_tokens,
                total_cost_usd: a.total_cost_usd,
                first_user_input_preview: a.first_user_input_preview.clone(),
                first_user_call_id: a.first_user_call_id.clone(),
            }))
    }

    /// Fold whole-lifetime totals for a set of sessions.
    ///
    /// The aggregation is done in Rust rather than by `stats`. A session has
    /// tens to thousands of turns, so the rows are cheap to pull, and folding
    /// them here means the previews come from the same pass as the sums
    /// instead of needing the `ROW_NUMBER()` window function the SQL backends
    /// use — which aglake has no equivalent for.
    async fn session_aggregates(
        &self,
        session_ids: &[String],
        source_id: Option<&str>,
    ) -> Result<std::collections::HashMap<(String, String), Agg>> {
        use std::collections::HashMap;
        let mut out: HashMap<(String, String), Agg> = HashMap::new();
        if session_ids.is_empty() {
            return Ok(out);
        }
        for chunk in session_ids.chunks(spl::FANOUT_ID_CHUNK) {
            let mut s = Search::new(&self.ix.traces, ST_TRACE);
            s.any_of("session_id", chunk);
            if let Some(sid) = source_id {
                s.any_of("source_id", std::slice::from_ref(&sid.to_string()));
            }
            let spl_q = format!(
                "{} | head {} | table _raw",
                s.build(),
                self.max_sessions_scan
            );
            // Unbounded: lifetime totals span every turn ever recorded for
            // the session, which is precisely what the caller's window excludes.
            let rows = self.search.search(&spl_q, "0", "0").await?.rows();
            for t in decode_rows::<TraceEvent>("session_aggregates", rows) {
                out.entry((t.source_id.clone(), t.session_id.clone()))
                    .or_default()
                    .fold(&t);
            }
        }
        Ok(out)
    }

    pub(crate) async fn query_session_traces(
        &self,
        query: &SessionTracesQuery,
    ) -> Result<SessionTracesPage> {
        let page_size = query.page_size.max(1) as usize;

        let mut s = Search::new(&self.ix.traces, ST_TRACE);
        s.any_of("source_id", std::slice::from_ref(&query.source_id));
        s.any_of("session_id", std::slice::from_ref(&query.session_id));

        let sort = Sort::new("start_time", "DESC", SESSION_TRACE_SORT, &["str(turn_id)"])?;
        // Fetch one extra to learn whether another page exists, without a
        // second count query.
        let want = page_size + 1;
        // The cursor is a (start_time_us, turn_id) tuple compared descending;
        // like the session-list cursor it is applied here rather than in SPL.
        // Over-fetch so the rows the cursor drops do not eat into the page.
        let fetch = if query.cursor.is_some() {
            want.saturating_add(page_size)
        } else {
            want
        };
        let spl_q = format!("{} | sort {fetch} {} | table _raw", s.build(), sort.keys);
        let rows = self.search.search(&spl_q, "0", "0").await?.rows();
        let mut events: Vec<TraceEvent> = decode_rows("query_session_traces", rows);
        events.sort_by(|a, b| {
            b.ts_us
                .cmp(&a.ts_us)
                .then_with(|| b.turn_id.cmp(&a.turn_id))
        });

        if let Some(c) = &query.cursor {
            events
                .retain(|t| (t.ts_us, t.turn_id.as_str()) < (c.start_time_us, c.turn_id.as_str()));
        }

        let has_more = events.len() > page_size;
        events.truncate(page_size);

        let next_cursor = if has_more {
            events.last().map(|t| {
                encode_session_turns_cursor(&SessionTracesCursor {
                    start_time_us: t.ts_us,
                    turn_id: t.turn_id.clone(),
                })
            })
        } else {
            None
        };

        Ok(SessionTracesPage {
            items: events.into_iter().map(session_trace_item).collect(),
            next_cursor,
        })
    }
}

/// Read an integer that search may have rendered as a JSON number or, past
/// 2^53, as a string.
fn as_i64(v: Option<&serde_json::Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

fn session_trace_item(e: TraceEvent) -> SessionTraceItem {
    let models_used = parse_json_string_list(Some(&e.models_used_json));
    SessionTraceItem {
        turn_id: e.turn_id,
        source_id: e.source_id,
        session_id: e.session_id,
        start_time: ms(e.ts_us),
        end_time: ms(e.end_us),
        duration_ms: e.duration_ms,
        wire_api: e.wire_api,
        agent_kind: e.agent_kind,
        primary_model: models_used.first().cloned(),
        models_used,
        call_count: e.call_count,
        total_input_tokens: e.total_input_tokens,
        total_output_tokens: e.total_output_tokens,
        status: e.status,
        final_finish_reason: e.final_finish_reason,
        // Same divergence as ClickHouse: the DuckDB backend re-runs each agent
        // profile's body extractor to rebuild the full text. The stored
        // previews are what is available here, and truncated ones stay
        // truncated.
        user_input: e.user_input_preview,
        final_answer: e.final_answer_preview,
        tool_surfaces: parse_json_string_list(Some(&e.tool_surfaces_json)),
        tool_call_total: e.tool_call_total,
        agent_topology: e.agent_topology,
        suspicious_skills: serde_json::from_str(&e.suspicious_skills_json).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{fixtures, trace_event};

    fn ev(turn: &str, start_us: i64, end_us: i64, calls: u32, tokens: u64) -> TraceEvent {
        let mut t = fixtures::full_trace();
        t.turn_id = turn.into();
        t.start_time_us = start_us;
        t.end_time_us = end_us;
        t.call_count = calls;
        t.total_input_tokens = tokens;
        t.total_cost_usd = Some(1.5);
        trace_event(&t)
    }

    /// Lifetime totals sum across turns, and the previews come from the
    /// *earliest* turn — not the first one the fold happened to see.
    #[test]
    fn agg_folds_lifetime_totals_and_earliest_preview() {
        let mut a = Agg::default();
        // Deliberately folded newest-first, which is the order a DESC scan
        // would deliver them in.
        let mut late = ev("t2", 2_000, 3_000, 3, 70);
        late.user_input_preview = Some("later".into());
        late.user_call_id = Some("call-late".into());
        let mut early = ev("t1", 1_000, 1_500, 2, 30);
        early.user_input_preview = Some("earliest".into());
        early.user_call_id = Some("call-early".into());

        a.fold(&late);
        a.fold(&early);

        assert_eq!(a.turn_count, 2);
        assert_eq!(a.call_count, 5);
        assert_eq!(a.total_input_tokens, 100);
        assert_eq!(a.first_us, 1_000, "min start across the lifetime");
        assert_eq!(a.last_us, 3_000, "max end across the lifetime");
        assert_eq!(a.total_cost_usd, Some(3.0));
        assert_eq!(a.first_user_input_preview.as_deref(), Some("earliest"));
        assert_eq!(a.first_user_call_id.as_deref(), Some("call-early"));
    }

    /// A session with no cost data must report `None`, not `Some(0.0)` —
    /// "we do not know the price" and "it was free" are different answers.
    #[test]
    fn agg_keeps_unknown_cost_as_none() {
        let mut a = Agg::default();
        let mut t = ev("t1", 1, 2, 1, 1);
        t.total_cost_usd = None;
        a.fold(&t);
        assert_eq!(a.total_cost_usd, None);
        assert_eq!(a.turn_count, 1);
    }

    /// Search renders integers past 2^53 as strings; both forms have to read
    /// back as the same number.
    #[test]
    fn as_i64_accepts_numbers_and_stringified_numbers() {
        assert_eq!(
            as_i64(Some(&serde_json::json!(1785638114914200_i64))),
            Some(1785638114914200)
        );
        assert_eq!(
            as_i64(Some(&serde_json::json!("9007199254740999"))),
            Some(9007199254740999)
        );
        assert_eq!(as_i64(Some(&serde_json::json!("nope"))), None);
        assert_eq!(as_i64(None), None);
    }
}
