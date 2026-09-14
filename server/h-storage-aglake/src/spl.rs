//! SPL construction helpers: quoting, IN-lists, time windows, pagination.
//!
//! Two hazards drive everything in this module.
//!
//! **Injection.** Every value that reaches a query is attacker-influenced in
//! principle (model names and request paths come off the wire). SPL's
//! double-quoted string understands exactly two escapes, `\"` and `\\`, and
//! keeps unknown ones verbatim (`aglake-spl/src/cursor.rs:130`), so [`quote`]
//! escapes those two and nothing else.
//!
//! **`*` is a wildcard with no escape.** `search field="*"` matches *every*
//! non-empty value rather than a literal asterisk, and there is no way to
//! escape it in a search term — `plan.rs:619` routes any value containing `*`
//! to `wildcard_match`, which has no `\*` branch. A value that happens to
//! contain `*` therefore silently becomes a pattern. [`match_term`] detects
//! this and falls back to `| where field == "..."`, which compares literally
//! (`aglake-eval/src/compile.rs` `BinOp::Eq`). That costs index pushdown, so it
//! only kicks in for the values that actually need it.

/// Quote a value as an SPL double-quoted string literal.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// True when a value cannot be used in a `search` term without being
/// reinterpreted as a glob.
pub(crate) fn needs_literal_compare(v: &str) -> bool {
    v.contains('*')
}

/// A single equality predicate for the `search` command, or `None` when the
/// value must be compared literally in a later `| where` stage instead.
pub(crate) fn match_term(field: &str, value: &str) -> Option<String> {
    if needs_literal_compare(value) {
        None
    } else {
        Some(format!("{field}={}", quote(value)))
    }
}

/// A body-index lookup term for one span id.
///
/// The body indexes are the one place a field predicate does not work. Their
/// events are delivered as pre-serialized JSON *strings* with `auto_json =
/// false`, so aglake extracts no fields from them at all — `span_id="…"`
/// compares against a field that does not exist and matches nothing, silently,
/// while still paying for the scan. (Whether it matches also depends on whether
/// a props.toml is loaded, which is why this survived a test suite that ran
/// without one.)
///
/// So the lookup is a raw term instead, anchored on the leading key. The body
/// event is written with `span_id` first precisely so this anchor exists: the
/// raw text always begins `{"span_id":"<id>"`, and a bare id term would also
/// match a body whose *content* happens to quote that id — this cannot, because
/// inside a nested body the quotes are escaped.
///
/// `None` when the id contains `*`, which no UUID does; the caller must treat
/// that as "no lookup" rather than "match everything".
pub(crate) fn body_term(span_id: &str) -> Option<String> {
    if needs_literal_compare(span_id) {
        return None;
    }
    Some(quote(&format!("{{\"span_id\":\"{span_id}\"")))
}

/// `("<a>" OR "<b>")` over [`body_term`], for the chunked second hop. Ids that
/// cannot be termed are dropped; `None` when that leaves nothing to search for.
pub(crate) fn body_terms(span_ids: &[String]) -> Option<String> {
    let terms: Vec<String> = span_ids.iter().filter_map(|id| body_term(id)).collect();
    match terms.len() {
        0 => None,
        1 => terms.into_iter().next(),
        _ => Some(format!("({})", terms.join(" OR "))),
    }
}

/// `field IN ("a", "b")` — equivalent to an OR chain but parsed as one node.
/// Returns `None` for an empty list (the caller must decide whether that means
/// "no filter" or "match nothing"; those are different and must not be
/// conflated).
pub(crate) fn in_list(field: &str, values: &[String]) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    let items: Vec<String> = values.iter().map(|v| quote(v)).collect();
    Some(format!("{field} IN ({})", items.join(", ")))
}

/// A literal `==` comparison for the `| where` stage. Use when the value may
/// contain `*`, or when comparing against aglake's own rollup sentinels.
pub(crate) fn where_eq(field: &str, value: &str) -> String {
    format!("{field} == {}", quote(value))
}

/// A search and the post-search stages that go with it.
///
/// Filters land in one of two places. Most become `search` terms, which push
/// down to postings. A value containing `*` cannot: there is no escape for it
/// in a search term, so it would silently turn into a glob and match rows the
/// user never asked for. Those get routed into a `| where` stage instead,
/// which compares literally. The split is per-filter, so one awkward model
/// name costs index pushdown on that filter alone.
pub(crate) struct Search {
    head: String,
    terms: Vec<String>,
    wheres: Vec<String>,
    evals: Vec<String>,
    dedup: Option<String>,
}

impl Search {
    pub(crate) fn new(index: &str, sourcetype: &str) -> Self {
        Self {
            head: format!("search index={index} sourcetype={sourcetype}"),
            terms: Vec::new(),
            wheres: Vec::new(),
            evals: Vec::new(),
            dedup: None,
        }
    }

    /// `field` matches any of `values`. An empty list is *no filter at all* —
    /// never "match nothing".
    ///
    /// On a field holding an array (`models_used`), this is an any-element
    /// match, which is what the SQL backends express as `hasAny` /
    /// `list_has_any`.
    pub(crate) fn any_of(&mut self, field: &str, values: &[String]) {
        if values.is_empty() {
            return;
        }
        if values.iter().any(|v| needs_literal_compare(v)) {
            let ors: Vec<String> = values.iter().map(|v| where_eq(field, v)).collect();
            self.wheres.push(format!("({})", ors.join(" OR ")));
        } else if let Some(t) = in_list(field, values) {
            self.terms.push(t);
        }
    }

    /// Same, for numeric values — no quoting hazard, so always a term.
    pub(crate) fn any_of_nums<T: std::fmt::Display>(&mut self, field: &str, values: &[T]) {
        if values.is_empty() {
            return;
        }
        let items: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        self.terms
            .push(format!("{field} IN ({})", items.join(", ")));
    }

    pub(crate) fn eq_num(&mut self, field: &str, value: impl std::fmt::Display) {
        self.terms.push(format!("{field}={value}"));
    }

    /// Substring match. This is the one predicate that cannot use an index —
    /// a non-prefix wildcard scans. Measured at ~18× a term match, which is
    /// slow but not disqualifying, so it stays rather than being restricted to
    /// a prefix.
    pub(crate) fn contains(&mut self, field: &str, substr: &str) {
        // The value is spliced into a glob, so `*` in user input would widen
        // the match rather than narrow it. Comparing literally is not an
        // option here (there is no substring operator on the search side), so
        // the wildcards the user typed are the wildcards they get — the same
        // thing `LIKE '%…%'` does with `%` on the SQL backends.
        self.terms
            .push(format!("{field}=\"*{}*\"", glob_body(substr)));
    }

    /// Half-open `[lo, hi)` on a numeric field. Range predicates cannot push
    /// down, hence the `| where`.
    pub(crate) fn range(&mut self, field: &str, lo: i64, hi: i64) {
        self.wheres.push(format!("{field}>={lo} AND {field}<{hi}"));
    }

    /// Add a computed field, available to later `sort` stages.
    pub(crate) fn eval(&mut self, name: &str, expr: &str) {
        self.evals.push(format!("{name}={expr}"));
    }

    /// Collapse rows sharing `field` to the first of each.
    ///
    /// Two costs, both real. `dedup` has no streaming form here — it has to
    /// materialize and sort — and it is not one of the stages the columnar
    /// fast path tolerates between the search and the aggregate, so adding it
    /// drops the whole query back to the row path. That is why the caller of
    /// this is behind a config flag rather than always on: it buys correctness
    /// against an event (a duplicated write) that most deployments never see.
    ///
    /// `field` is interpolated as a bare identifier — callers pass literals.
    pub(crate) fn dedup(&mut self, field: &'static str) {
        self.dedup = Some(field.to_string());
    }

    /// The full prefix: search terms, then `| where`, then `| dedup`, then
    /// `| eval`.
    ///
    /// `dedup` goes before `eval` so the per-row arithmetic runs on the rows
    /// that survive rather than the ones that do not.
    pub(crate) fn build(&self) -> String {
        let mut q = self.head.clone();
        for t in &self.terms {
            q.push(' ');
            q.push_str(t);
        }
        if !self.wheres.is_empty() {
            q.push_str(" | where ");
            q.push_str(&self.wheres.join(" AND "));
        }
        if let Some(f) = &self.dedup {
            q.push_str(" | dedup ");
            q.push_str(f);
        }
        for e in &self.evals {
            q.push_str(" | eval ");
            q.push_str(e);
        }
        q
    }
}

/// Escape a value for use inside a `"*…*"` glob, leaving `*` alone (see
/// [`Search::contains`]).
fn glob_body(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Convert Heron's microsecond timestamp to the epoch-seconds string the
/// search API takes for `earliest` / `latest`.
///
/// These bound bucket pruning, which is the single most effective thing a
/// query can do here, so they should always be set. `"0"` means unbounded and
/// forces a full scan.
///
/// Negative instants are clamped to the epoch, because aglake's time parser
/// rejects them outright — `bad time "-86400.000000"` — and a rejected query is
/// an HTTP 500 on a page that had a perfectly answerable question. The reads
/// that widen their window backwards to catch a turn that started before it
/// (see [`crate::AglakeBackend::window`]) produce exactly this when the caller
/// asks from `start=0`, which the API allows and which pcap replay reaches in
/// ordinary use. Clamping loses nothing: no event predates 1970, so the epoch
/// and "unbounded" select the same rows.
pub(crate) fn epoch_secs(us: i64) -> String {
    let us = us.max(0);
    format!(
        "{}.{:06}",
        us.div_euclid(1_000_000),
        us.rem_euclid(1_000_000)
    )
}

/// Chunk size for `id IN (...)` point lookups.
///
/// Not a query-string bound — a cost bound. An N-id lookup is N term probes
/// however it is spelled, and the wall clock is what a user waits on, so the
/// chunks exist to get those probes running concurrently. One search carrying
/// every id is the slowest available shape.
///
/// 64 is a **measured optimum, and a sharp one**. 400 span ids against a hot
/// bucket, with [`AglakeConfig::max_concurrent_searches`] = 8, five interleaved
/// reps on one host:
///
/// | chunk | searches | aglaked 1.5.0.2674 | with `sglog-ystd` fixed |
/// |---|---|---|---|
/// | 400 (one search) | 1 | 6423 ms | 1596 ms |
/// | 128 | 4 | 1670 ms | 679 ms |
/// | 96 | 5 | 1254 ms | 555 ms |
/// | **64** | **7** | **887 ms** | **455 ms** |
/// | 48 | 9 | 1047 ms | 655 ms |
/// | 32 | 13 | 1051 ms | 658 ms |
///
/// Both directions away from 64 are worse, for different reasons: fewer, bigger
/// searches leave the permits idle, and more, smaller ones pay aglaked's
/// per-search overhead (opening the window, pruning buckets) more times than the
/// terms save. Chunking below ~16 is slower than not chunking at all.
///
/// Note what the two columns do *not* say. Upstream fixed the per-term cost on
/// dictionary-less (unsealed) buckets — it was quadratic per event, `sglog-ystd`
/// — which is the difference between the columns and is worth 4x on its own. It
/// does not make the chunking redundant: concurrency is still worth **3.5x**
/// after the fix, and 64 is still the optimum. Per-term cost falls with term
/// count on a fixed daemon (4.02 ms/term at 400 vs 10.1 at 32), so the marginal
/// term is cheap — but the total still grows with N, and only the fan-out
/// shortens the wall clock.
///
/// 64 also pairs with the default concurrency: 8 permits x 64 ids means up to
/// 512 ids resolve in a single wave, which covers every agent turn observed so
/// far (the largest was 418 calls).
pub(crate) const ID_CHUNK: usize = 64;

/// Chunk size for id lookups that fan *out* — many rows per id, not one.
///
/// `session_aggregates` is the only one: it matches turn rows by `session_id`,
/// where a session has tens to thousands of turns. Two reasons it keeps the
/// large chunk. The per-term cost is amortized against real row volume rather
/// than being the whole cost (one page of 100 sessions measures 293 ms end to
/// end), and its `| head max_sessions_scan` row budget is written per search —
/// so splitting the id set into k chunks would quietly multiply the budget by
/// k.
pub(crate) const FANOUT_ID_CHUNK: usize = 512;

/// Build the offset-pagination pipeline.
///
/// `sort <offset+limit>` immediately after `search` keeps the executor on its
/// bounded top-k path (O(2N) memory) instead of materializing the window, and
/// the trailing `| table` switches the response into "results" mode, which is
/// **not** subject to `max_events`. The sort limit is always written
/// explicitly — a bare `| sort f` means `sort 10000 f` in current aglake
/// (`aglake-spl/src/commands.rs`), and older builds do not apply that cap at
/// all, so relying on either behaviour would be version-dependent.
///
/// The window itself is cut with `streamstats` + `where` on the row number
/// rather than the more obvious `| tail <limit>`, which is wrong three ways:
/// it **reverses** the rows it returns; past the end of the result set it
/// returns the last page over and over instead of nothing, so paging never
/// terminates; and on a partial last page it returns a full page reaching back
/// into rows the previous page already showed. Numbering the rows and taking
/// `(offset, offset+limit]` has none of those problems and costs one streaming
/// counter.
///
/// `sort_keys` must already include a deterministic tie-break (`ts_us` then
/// `id`): equal sort values otherwise order by storage order, which changes
/// when hot buckets are sealed or merged, and unstable ordering makes
/// pagination drop or repeat rows.
pub(crate) fn paginate(
    search: &str,
    sort_keys: &str,
    offset: u64,
    limit: u64,
    columns: &[&str],
) -> String {
    let end = offset + limit;
    format!(
        "{search} | sort {end} {sort_keys} \
         | streamstats count as _rn | where _rn>{offset} AND _rn<={end} | table {}",
        columns.join(", ")
    )
}

/// The companion count query. A pipeline's `total` is the number of rows it
/// *emitted*, not the number matched, so the page size and the total have to
/// come from two different queries.
pub(crate) fn count_query(search: &str) -> String {
    format!("{search} | stats count as n | table n")
}

/// Fetch whole events rather than columns.
///
/// Reads always go through `_raw` and are deserialized in Rust, never assembled
/// from the extracted fields beside it. Search output is lossy in three ways
/// that all corrupt a struct silently: a null field is dropped rather than
/// returned as null, a single-element multivalue collapses into a bare scalar,
/// and an integer past 2^53 comes back as a string. `_raw` has none of those
/// problems — it is the event as written.
pub(crate) fn raw_query(search: &str, limit: usize) -> String {
    format!("{search} | head {limit} | table _raw")
}

/// How far a point lookup widens its time window, in microseconds, tried in
/// this order until one produces a hit.
///
/// The window is only ever a hint — [`id_windows`] explains why it can miss —
/// so the question is not "how wide must it be to be right" but "how wide
/// before it stops paying for itself". Measured against a live store, a point
/// lookup costs what its window costs and almost nothing else: ±60s answered in
/// 0.04s, ±5m in 0.21s, ±6h in 1.17s. A single wide window therefore put ~2s of
/// floor under every span detail (metadata lookup plus body lookup) to buy skew
/// tolerance that essentially no deployment needs.
///
/// Narrow first, then wide, then unbounded. A live capture — where the id is
/// minted milliseconds after the packet — hits the narrow window and pays 0.2s.
/// A host with badly skewed clocks pays the narrow miss and then hits the wide
/// one, which is what it used to pay anyway. Replay, where id time and packet
/// time are years apart, misses both and falls to the unbounded retry, which is
/// also what it used to do — the ±6h window never covered that case either.
const ID_WINDOW_SKEW_US: &[i64] = &[5 * 60_000_000, 6 * 3_600_000_000];

/// Best-effort `(earliest, latest)` derived from a UUIDv7's embedded
/// millisecond timestamp.
///
/// Every id Heron mints is a UUIDv7, so a by-id lookup can usually prune to a
/// handful of buckets instead of consulting every bucket in the retention
/// window. Two cases make this a hint rather than a fact, and both are why
/// callers must fall back to an unbounded retry when the window comes up
/// empty:
///
/// * `turn_id` can come from the provider (Codex sends its own), in which case
///   it is not a UUIDv7 at all and this returns `None`.
/// * The id is minted when the pipeline sees the record, but `_time` comes
///   from the packet. Replaying a captured pcap makes those years apart, so
///   the derived window would exclude the very event it is looking for —
///   which is exactly how the test corpus and the staging soak run.
pub(crate) fn id_windows(id: &str) -> Vec<(String, String)> {
    let Some(us) = uuid::Uuid::parse_str(id)
        .ok()
        .and_then(|u| u.get_timestamp())
        .and_then(|ts| {
            let (secs, nanos) = ts.to_unix();
            Some(i64::try_from(secs).ok()?.checked_mul(1_000_000)? + (nanos / 1000) as i64)
        })
    else {
        return Vec::new();
    };
    ID_WINDOW_SKEW_US
        .iter()
        .map(|skew| {
            (
                epoch_secs(us.saturating_sub(*skew)),
                epoch_secs(us.saturating_add(*skew)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body term is anchored on the leading key, so an id that merely
    /// appears *inside* a stored body cannot match it: within a nested body the
    /// quotes are escaped, so the literal `{"span_id":"<id>"` never occurs
    /// there. Losing the anchor would reintroduce that confusion silently.
    #[test]
    fn body_term_anchors_on_the_leading_key() {
        let t = body_term("01a0-7bf0").expect("a uuid is termable");
        assert_eq!(t, r#""{\"span_id\":\"01a0-7bf0\"""#);
        assert!(
            !t.contains("span_id=") && !t.contains(" IN "),
            "must be a raw term, not a field predicate — the body indexes have \
             no extracted fields to compare against: {t}"
        );
    }

    #[test]
    fn body_terms_ors_the_chunk_and_refuses_an_empty_one() {
        assert_eq!(body_terms(&[]), None);
        let one = body_terms(&["a".to_string()]).unwrap();
        assert!(
            !one.starts_with('('),
            "a single id needs no OR group: {one}"
        );
        let two = body_terms(&["a".to_string(), "b".to_string()]).unwrap();
        assert!(two.starts_with('(') && two.contains(" OR "), "{two}");
    }

    /// A `*` cannot be a search term, and answering `None` must mean "look
    /// nothing up" rather than degrading into a term that matches every body.
    #[test]
    fn body_term_refuses_a_wildcard_id() {
        assert_eq!(body_term("01a0-*"), None);
        assert_eq!(body_terms(&["01a0-*".to_string()]), None);
        let mixed = body_terms(&["01a0-*".to_string(), "real".to_string()]).unwrap();
        assert!(
            mixed.contains("real") && !mixed.contains('*'),
            "the termable id survives, the wildcard one is dropped: {mixed}"
        );
    }

    #[test]
    fn quote_escapes_quote_and_backslash_only() {
        assert_eq!(quote("plain"), r#""plain""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r"a\b"), r#""a\\b""#);
        // Unknown escapes stay verbatim on the parser side, so we must not
        // touch them here either.
        assert_eq!(quote(r"\d+"), r#""\\d+""#);
        assert_eq!(quote("中文 模型"), r#""中文 模型""#);
    }

    /// The breakout attempt: close the string, then append another clause.
    /// After quoting, the payload must remain a single string literal.
    #[test]
    fn quote_neutralizes_breakout() {
        let payload = r#"x" OR index=* | delete "#;
        let q = quote(payload);
        assert!(q.starts_with('"') && q.ends_with('"'));
        // The only unescaped quotes are the delimiters.
        let inner = &q[1..q.len() - 1];
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                chars.next(); // consume the escaped char
            } else {
                assert_ne!(c, '"', "unescaped quote leaked into {q}");
            }
        }
    }

    /// A trailing backslash must not escape the closing delimiter.
    #[test]
    fn quote_handles_trailing_backslash() {
        assert_eq!(quote(r"end\"), r#""end\\""#);
    }

    #[test]
    fn match_term_defers_wildcard_values_to_where() {
        assert_eq!(match_term("model", "gpt-4").unwrap(), r#"model="gpt-4""#);
        // `*` would silently become a glob in a search term.
        assert!(match_term("wire_api", "*").is_none());
        assert!(match_term("model", "gpt*").is_none());
        assert_eq!(where_eq("wire_api", "*"), r#"wire_api == "*""#);
    }

    #[test]
    fn in_list_quotes_each_value() {
        assert_eq!(in_list("id", &[]), None);
        assert_eq!(
            in_list("id", &["a".into(), r#"b"c"#.into()]).unwrap(),
            r#"id IN ("a", "b\"c")"#
        );
    }

    /// aglake's time parser rejects a negative instant, so a query whose
    /// window was widened backwards past 1970 must not produce one — it would
    /// surface as a 500 on a question that had an answer.
    #[test]
    fn epoch_secs_clamps_below_the_epoch() {
        assert_eq!(epoch_secs(-86_400_000_000), "0.000000");
        assert_eq!(epoch_secs(-1), "0.000000");
        assert_eq!(epoch_secs(0), "0.000000");
        assert!(!epoch_secs(i64::MIN).starts_with('-'));
    }

    #[test]
    fn epoch_secs_keeps_microsecond_precision() {
        assert_eq!(epoch_secs(1_785_638_114_914_200), "1785638114.914200");
        assert_eq!(epoch_secs(1_000_000), "1.000000");
        assert_eq!(epoch_secs(0), "0.000000");
        // This used to assert `-1` rendered as the well-formed `"-1.999999"`.
        // Well-formed was the wrong bar: aglake rejects the value regardless of
        // how it is spelled. See `epoch_secs_clamps_below_the_epoch`.
    }

    /// The window must be cut by row number, not by `| tail` — see
    /// [`paginate`] for the three ways `tail` gets this wrong.
    #[test]
    fn paginate_cuts_the_window_by_row_number() {
        let q = paginate(
            "search index=x",
            "-num(ts_us), -str(id)",
            100,
            50,
            &["id", "ts_us"],
        );
        assert!(q.contains("| sort 150 -num(ts_us), -str(id)"), "{q}");
        assert!(q.contains("| streamstats count as _rn"), "{q}");
        assert!(q.contains("| where _rn>100 AND _rn<=150"), "{q}");
        assert!(!q.contains("| tail"), "tail reverses and overruns: {q}");
        assert!(q.ends_with("| table id, ts_us"), "{q}");
    }

    /// The first page starts at row 1, so the lower bound must be exclusive-0
    /// rather than something that skips it.
    #[test]
    fn paginate_first_page_starts_at_row_one() {
        let q = paginate("search index=x", "-num(ts_us)", 0, 20, &["_raw"]);
        assert!(q.contains("| sort 20 "), "{q}");
        assert!(q.contains("| where _rn>0 AND _rn<=20"), "{q}");
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_values_become_pushdown_terms() {
        let mut s = Search::new("heron_spans", "heron_span");
        s.any_of("model", &v(&["gpt-4", "claude"]));
        s.any_of_nums("status_code", &[200u16, 404]);
        s.eq_num("strm", 1);
        let q = s.build();
        assert_eq!(
            q,
            r#"search index=heron_spans sourcetype=heron_span model IN ("gpt-4", "claude") status_code IN (200, 404) strm=1"#
        );
        assert!(!q.contains("| where"), "no where stage needed: {q}");
    }

    /// An empty filter list means "no filter", never "match nothing" —
    /// conflating those silently empties every page.
    #[test]
    fn empty_lists_add_no_predicate() {
        let mut s = Search::new("i", "st");
        s.any_of("model", &[]);
        s.any_of_nums::<u16>("status_code", &[]);
        assert_eq!(s.build(), "search index=i sourcetype=st");
    }

    /// A value containing `*` would become a glob in a search term and match
    /// rows the user never asked for. It has to move to a literal compare.
    #[test]
    fn wildcard_values_move_to_a_literal_where_stage() {
        let mut s = Search::new("i", "st");
        s.any_of("model", &v(&["gpt-4", "weird*name"]));
        let q = s.build();
        assert!(!q.contains("model IN"), "must not push down as a glob: {q}");
        assert_eq!(
            q,
            r#"search index=i sourcetype=st | where (model == "gpt-4" OR model == "weird*name")"#
        );
    }

    /// One awkward value must not cost pushdown on unrelated filters.
    #[test]
    fn wildcard_routing_is_per_filter() {
        let mut s = Search::new("i", "st");
        s.any_of("model", &v(&["a*b"]));
        s.any_of("wire_api", &v(&["openai-chat"]));
        let q = s.build();
        assert!(q.contains(r#"wire_api IN ("openai-chat")"#), "{q}");
        assert!(q.contains(r#"| where (model == "a*b")"#), "{q}");
    }

    /// The injection payload: close the string and append another clause.
    ///
    /// The payload text still appears in the query — it has to, it is the
    /// value being matched. What matters is that its `"` is escaped, so the
    /// whole thing stays one string literal and `| delete` is data rather than
    /// a pipeline stage.
    #[test]
    fn breakout_payloads_stay_inside_their_literal() {
        let mut s = Search::new("i", "st");
        s.any_of("model", &v(&[r#"x" OR index=secret | delete "#]));
        assert_eq!(
            s.build(),
            r#"search index=i sourcetype=st model IN ("x\" OR index=secret | delete ")"#
        );

        // Every `"` after the opening delimiter must be escaped.
        let mut s2 = Search::new("i", "st");
        s2.any_of("model", &v(&[r#"a" b" c"#]));
        let q = s2.build();
        let literal = &q[q.find('(').unwrap() + 2..q.rfind('"').unwrap()];
        let mut chars = literal.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                chars.next();
            } else {
                assert_ne!(c, '"', "unescaped quote leaked into {q}");
            }
        }

        // A trailing backslash must not escape the closing delimiter.
        let mut s3 = Search::new("i", "st");
        s3.any_of("model", &v(&[r#"a\"#]));
        assert!(s3.build().contains(r#""a\\""#), "{}", s3.build());
    }

    #[test]
    fn stages_are_ordered_search_then_where_then_eval() {
        let mut s = Search::new("i", "st");
        s.any_of("model", &v(&["m"]));
        s.range("end_us", 10, 20);
        s.eval("dur_ms", "(done_us - ts_us) / 1000");
        assert_eq!(
            s.build(),
            r#"search index=i sourcetype=st model IN ("m") | where end_us>=10 AND end_us<20 | eval dur_ms=(done_us - ts_us) / 1000"#
        );
    }

    #[test]
    fn contains_wraps_in_a_glob_and_escapes_quotes() {
        let mut s = Search::new("i", "st");
        s.contains("request_path", r#"/v1/"chat"#);
        assert!(
            s.build().ends_with(r#"request_path="*/v1/\"chat*""#),
            "{}",
            s.build()
        );
    }
}
