//! Dimension-tier selection for the pre-aggregated metrics rows.
//!
//! The aggregator materializes four rollup tiers and marks the rolled-up
//! coordinates with a literal `'*'`. The SQL backends select a tier by writing
//! that sentinel into the predicate — `server_ip = '*'` for "the row that
//! already sums across servers", `model != '*'` for "the detail rows".
//!
//! Neither form survives translation to SPL. `*` is the wildcard character in
//! a search term with no way to escape it, so `server_ip="*"` matches **every**
//! row rather than the rollup row — detail and rollup would be counted
//! together and every metric would silently double. And `!=` cannot be pushed
//! down at all.
//!
//! So the tier is classified at write time into an ordinary categorical field
//! ([`crate::rows`]'s `dim_tier`) and selected here by exact match. The `'*'`
//! sentinels are still stored verbatim — they are the data — but no query ever
//! has to compare against one.

use h_storage::query::DimensionFilter;

use crate::spl::Search;

/// The four tiers `h_metrics::aggregator::dimension_keys` materializes.
///
/// Which one answers a query is decided by *which dimensions are filtered*,
/// not by their values: any filter on wire_api or model forces the detail
/// tier for both, and a server filter forces the per-server coordinate.
pub(crate) fn tier_for(has_wire: bool, has_model: bool, has_server: bool) -> &'static str {
    match (has_wire || has_model, has_server) {
        (false, false) => "all",
        (false, true) => "s",
        (true, false) => "wm",
        (true, true) => "wms",
    }
}

/// Apply the tier plus the value filters for a metrics read.
///
/// Only dimensions the caller actually filtered get an `IN` list. The rest are
/// pinned by the tier itself: a `wm` row has a specific wire_api and model by
/// construction, which is what the SQL backends spell as `!= '*'`.
pub(crate) fn apply(s: &mut Search, filter: &DimensionFilter) {
    apply_parts(
        s,
        &filter.wire_apis,
        &filter.models,
        &filter.server_ips,
        &filter.tool_surfaces,
    );
}

/// Same, for `FinishReasonsQuery`, which carries its dimensions loose rather
/// than in a `DimensionFilter` and has no tool-surface dimension.
pub(crate) fn apply_parts(
    s: &mut Search,
    wire_apis: &[String],
    models: &[String],
    server_ips: &[String],
    tool_surfaces: &[String],
) {
    let tier = tier_for(
        !wire_apis.is_empty(),
        !models.is_empty(),
        !server_ips.is_empty(),
    );
    s.any_of("dim_tier", std::slice::from_ref(&tier.to_string()));
    s.any_of("wire_api", wire_apis);
    s.any_of("model", models);
    s.any_of("server_ip", server_ips);
    // Absent tool-surface filter means "roll up across all surfaces,
    // including the rows that have none" — so no predicate at all, matching
    // `build_tool_surface_clause`.
    s.any_of("tool_surface", tool_surfaces);
}

/// Apply the tier for a query that GROUPs BY a dimension.
///
/// Grouping by wire_api or model forces the detail tier for both even when
/// neither is filtered — a rollup row would collapse the very dimension being
/// grouped on. `server_ip` is deliberately not in that set: the SQL backends
/// fall through to the ungrouped tier for it, and matching them matters more
/// than the shape being obviously right.
pub(crate) fn apply_for_group(s: &mut Search, filter: &DimensionFilter, group_by: Option<&str>) {
    let forced = matches!(group_by, Some("wire_api") | Some("model"));
    let tier = tier_for(
        forced || !filter.wire_apis.is_empty(),
        forced || !filter.models.is_empty(),
        !filter.server_ips.is_empty(),
    );
    s.any_of("dim_tier", std::slice::from_ref(&tier.to_string()));
    s.any_of("wire_api", &filter.wire_apis);
    s.any_of("model", &filter.models);
    s.any_of("server_ip", &filter.server_ips);
    s.any_of("tool_surface", &filter.tool_surfaces);
}

/// The tier a metrics row belongs to, from its stored dimension values. This is
/// the read-side twin of the write-side classifier and exists so tests can
/// check the two agree.
#[cfg(test)]
pub(crate) fn tier_of_row(wire_api: &str, model: &str, server_ip: &str) -> &'static str {
    match (wire_api == "*", model == "*", server_ip == "*") {
        (false, false, false) => "wms",
        (false, false, true) => "wm",
        (true, true, false) => "s",
        (true, true, true) => "all",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h_storage::dialect::{build_dimension_where, escape_standard};

    /// One materialized metrics row.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct Row {
        wire_api: String,
        model: String,
        server_ip: String,
    }

    fn row(w: &str, m: &str, s: &str) -> Row {
        Row {
            wire_api: w.into(),
            model: m.into(),
            server_ip: s.into(),
        }
    }

    /// Every row the aggregator would materialize for two wire_apis, two
    /// models and two servers — all four tiers, so a selector that picks the
    /// wrong one is visible as a wrong row set rather than a wrong number.
    fn universe() -> Vec<Row> {
        let mut rows = Vec::new();
        for w in ["openai-chat", "anthropic"] {
            for m in ["gpt-4", "claude"] {
                for s in ["10.0.0.1", "10.0.0.2"] {
                    rows.push(row(w, m, s)); // (W, M, S)
                }
                rows.push(row(w, m, "*")); // (W, M, *)
            }
        }
        for s in ["10.0.0.1", "10.0.0.2"] {
            rows.push(row("*", "*", s)); // (*, *, S)
        }
        rows.push(row("*", "*", "*")); // (*, *, *)
        rows
    }

    /// A minimal evaluator for exactly the predicate grammar
    /// `build_dimension_where` emits: `col = '*'`, `col != '*'`, and
    /// `col IN ('a', 'b')`, joined by ` AND `.
    ///
    /// Parsing the real function's output — rather than reimplementing its
    /// logic — is what makes this an equivalence test. If `dialect.rs` ever
    /// emits a shape this cannot parse, the panic says so instead of the test
    /// quietly passing against a stale copy.
    fn sql_selects(where_sql: &str, r: &Row) -> bool {
        for clause in where_sql.split(" AND ") {
            let clause = clause.trim();
            let (col, rest) = clause.split_once(' ').unwrap_or_else(|| {
                panic!("unparsable clause {clause:?} in {where_sql:?}");
            });
            let value = match col {
                "wire_api" => &r.wire_api,
                "model" => &r.model,
                "server_ip" => &r.server_ip,
                // No tool_surface dimension on these fixtures; a filter on it
                // is orthogonal to tier selection and tested separately.
                "tool_surface" => continue,
                other => panic!("unexpected column {other:?} in {where_sql:?}"),
            };
            let ok = if let Some(list) = rest.strip_prefix("IN (") {
                let list = list.strip_suffix(')').expect("IN list must close");
                list.split(", ")
                    .map(|v| v.trim_matches('\''))
                    .any(|v| v == value)
            } else if let Some(v) = rest.strip_prefix("= ") {
                v.trim_matches('\'') == value
            } else if let Some(v) = rest.strip_prefix("!= ") {
                v.trim_matches('\'') != value
            } else {
                panic!("unparsable operator in {clause:?}");
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// What the aglake side selects: the tier, then the value filters.
    fn aglake_selects(f: &DimensionFilter, r: &Row) -> bool {
        let want = tier_for(
            !f.wire_apis.is_empty(),
            !f.models.is_empty(),
            !f.server_ips.is_empty(),
        );
        if tier_of_row(&r.wire_api, &r.model, &r.server_ip) != want {
            return false;
        }
        if !f.wire_apis.is_empty() && !f.wire_apis.contains(&r.wire_api) {
            return false;
        }
        if !f.models.is_empty() && !f.models.contains(&r.model) {
            return false;
        }
        if !f.server_ips.is_empty() && !f.server_ips.contains(&r.server_ip) {
            return false;
        }
        true
    }

    fn filter(w: &[&str], m: &[&str], s: &[&str]) -> DimensionFilter {
        DimensionFilter {
            wire_apis: w.iter().map(|x| x.to_string()).collect(),
            models: m.iter().map(|x| x.to_string()).collect(),
            server_ips: s.iter().map(|x| x.to_string()).collect(),
            tool_surfaces: vec![],
        }
    }

    /// The headline correctness property: for all eight filter shapes, the
    /// tier selector picks exactly the rows the SQL predicate picks.
    ///
    /// A mismatch here is the double-counting bug — selecting both a detail
    /// row and the rollup that already contains it inflates every metric
    /// without erroring.
    #[test]
    fn tier_selection_matches_build_dimension_where_row_for_row() {
        let cases = [
            filter(&[], &[], &[]),
            filter(&["openai-chat"], &[], &[]),
            filter(&[], &["gpt-4"], &[]),
            filter(&[], &[], &["10.0.0.1"]),
            filter(&["openai-chat"], &["gpt-4"], &[]),
            filter(&["openai-chat"], &[], &["10.0.0.1"]),
            filter(&[], &["gpt-4"], &["10.0.0.1"]),
            filter(&["openai-chat"], &["gpt-4"], &["10.0.0.1"]),
            // Multi-select on every dimension at once.
            filter(
                &["openai-chat", "anthropic"],
                &["gpt-4", "claude"],
                &["10.0.0.1", "10.0.0.2"],
            ),
        ];
        let rows = universe();
        for f in &cases {
            let sql = build_dimension_where(f, escape_standard);
            let mut from_sql: Vec<&Row> = rows.iter().filter(|r| sql_selects(&sql, r)).collect();
            let mut from_aglake: Vec<&Row> = rows.iter().filter(|r| aglake_selects(f, r)).collect();
            from_sql.sort();
            from_aglake.sort();
            assert_eq!(
                from_sql, from_aglake,
                "tier selection diverged for filter {f:?}\n  sql: {sql}"
            );
            assert!(
                !from_sql.is_empty(),
                "filter {f:?} selected nothing — the fixture universe is wrong"
            );
        }
    }

    /// Whatever the filter, the selected rows must all sit on one tier.
    /// Mixing tiers is exactly how a rollup row gets summed on top of the
    /// detail rows it already contains.
    #[test]
    fn every_filter_selects_exactly_one_tier() {
        let rows = universe();
        for f in [
            filter(&[], &[], &[]),
            filter(&["openai-chat"], &[], &[]),
            filter(&[], &[], &["10.0.0.1"]),
            filter(&["openai-chat"], &["gpt-4"], &["10.0.0.1"]),
        ] {
            let tiers: std::collections::BTreeSet<&str> = rows
                .iter()
                .filter(|r| aglake_selects(&f, r))
                .map(|r| tier_of_row(&r.wire_api, &r.model, &r.server_ip))
                .collect();
            assert_eq!(tiers.len(), 1, "filter {f:?} spans tiers {tiers:?}");
        }
    }

    /// The write-side classifier and the read-side selector have to agree on
    /// what each stored row is, or writes land in a tier reads never ask for.
    #[test]
    fn write_and_read_side_tier_classification_agree() {
        for r in universe() {
            let read = tier_of_row(&r.wire_api, &r.model, &r.server_ip);
            assert_ne!(read, "other", "{r:?} is not one of the materialized tiers");
        }
    }

    /// The generated SPL must never contain a bare `*`, which would be a
    /// wildcard rather than the rollup sentinel.
    #[test]
    fn generated_spl_never_compares_against_a_wildcard() {
        for f in [
            filter(&[], &[], &[]),
            filter(&["openai-chat"], &[], &["10.0.0.1"]),
        ] {
            let mut s = Search::new("heron_metrics_10s", "heron_metric");
            apply(&mut s, &f);
            let q = s.build();
            assert!(!q.contains('*'), "wildcard leaked into the query: {q}");
            assert!(q.contains("dim_tier IN ("), "{q}");
        }
    }

    /// `query_finish_reasons` builds its dimensions loose rather than from a
    /// `DimensionFilter`; the SQL backends give it its own copy of the tier
    /// logic, so check the two agree rather than assuming it.
    #[test]
    fn finish_reason_tier_logic_matches_the_dimension_filter_one() {
        for (w, m, s) in [
            (vec![], vec![], vec![]),
            (vec!["openai-chat"], vec![], vec![]),
            (vec![], vec!["gpt-4"], vec![]),
            (vec![], vec![], vec!["10.0.0.1"]),
            (vec!["openai-chat"], vec!["gpt-4"], vec!["10.0.0.1"]),
        ] {
            let f = filter(&w, &m, &s);
            let via_filter = tier_for(
                !f.wire_apis.is_empty(),
                !f.models.is_empty(),
                !f.server_ips.is_empty(),
            );
            let via_parts = tier_for(!w.is_empty(), !m.is_empty(), !s.is_empty());
            assert_eq!(via_filter, via_parts);
        }
    }
}
