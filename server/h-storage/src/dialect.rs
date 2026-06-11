//! Backend-neutral SQL fragment builders for dimension filtering on the
//! pre-aggregated `llm_metrics` table. The wildcard-tier logic (`'*'` rollup
//! rows vs specific `IN (...)` values) and the `!=`/`IN`/`=` operators are
//! identical across DuckDB and ClickHouse, so both backends share one copy.
//!
//! These builders interpolate the `'*'` wildcard literals (not user data)
//! directly; user-supplied IN-lists are quoted via [`sql_in_list`]. ClickHouse
//! call sites that bind user input as parameters instead can still reuse the
//! wildcard-tier selection logic here.

use crate::query::DimensionFilter;

/// A backend-specific SQL string-literal escaper.
///
/// String-literal escaping is NOT uniform across our backends: DuckDB and
/// Postgres use standard-conforming strings where the only special character
/// inside `'...'` is the single quote (doubled to `''`) and a backslash is an
/// ordinary literal character. ClickHouse instead processes C-style backslash
/// escapes inside single-quoted literals, so a backslash must itself be
/// escaped — otherwise a value ending in `\` consumes the closing quote we
/// emit and lets attacker-supplied input break out of the literal (SQL
/// injection). Every builder here that interpolates user data takes the
/// caller's escaper so each backend gets the correct semantics.
pub type LiteralEscaper = fn(&str) -> String;

/// Standard-conforming SQL literal escaping (DuckDB / Postgres): double single
/// quotes; backslash is a literal character and is left untouched.
pub fn escape_standard(s: &str) -> String {
    s.replace('\'', "''")
}

/// ClickHouse literal escaping: escape backslashes *before* doubling single
/// quotes, because ClickHouse treats `\'` as an escaped quote in addition to
/// the SQL-standard `''`. Order matters — escaping quotes first would leave
/// the freshly inserted backslashes unescaped.
pub fn escape_clickhouse(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "''")
}

/// Format a list of string values as a SQL IN list, escaping each value with
/// the supplied backend escaper.
pub fn sql_in_list_with(values: &[String], escape: LiteralEscaper) -> String {
    values
        .iter()
        .map(|s| format!("'{}'", escape(s)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// IN-list builder using standard (DuckDB / Postgres) escaping. ClickHouse
/// call sites must NOT use this — they go through `crate::sql::sql_in_list`
/// (backslash-aware) instead.
pub fn sql_in_list(values: &[String]) -> String {
    sql_in_list_with(values, escape_standard)
}

/// Build a WHERE clause segment for dimension filters on an ungrouped query.
///
/// The aggregator (see `h-metrics/src/aggregator.rs:dimension_keys`) only
/// materializes 4 of the 8 possible wildcard combinations for
/// `(wire_api, model, server_ip)`:
///
/// - `(W, M, S)` — finest
/// - `(W, M, *)` — per (wire_api, model), summed across servers
/// - `(*, *, S)` — per server_ip only
/// - `(*, *, *)` — grand total
///
/// The mapping below picks the coarsest tier that covers the user's filter
/// and SUMs across the remaining rows. A filter on wire_api or model forces
/// us below the `(*, *, ·)` tier; a filter on server_ip forces us off the
/// `server_ip = '*'` coordinate.
pub fn build_dimension_where(filter: &DimensionFilter, escape: LiteralEscaper) -> String {
    let has_wire = !filter.wire_apis.is_empty();
    let has_model = !filter.models.is_empty();
    let has_server = !filter.server_ips.is_empty();

    let (wire_clause, model_clause) = if !has_wire && !has_model {
        // Stay on (*, *, ·) tier.
        ("wire_api = '*'".to_string(), "model = '*'".to_string())
    } else {
        // Drop to (W, M, ·) tier — either IN-list or all specific values.
        let w = if has_wire {
            format!("wire_api IN ({})", sql_in_list_with(&filter.wire_apis, escape))
        } else {
            "wire_api != '*'".to_string()
        };
        let m = if has_model {
            format!("model IN ({})", sql_in_list_with(&filter.models, escape))
        } else {
            "model != '*'".to_string()
        };
        (w, m)
    };

    let server_clause = if has_server {
        format!("server_ip IN ({})", sql_in_list_with(&filter.server_ips, escape))
    } else {
        "server_ip = '*'".to_string()
    };

    let surface_clause = build_tool_surface_clause(&filter.tool_surfaces, escape);
    format!("{wire_clause} AND {model_clause} AND {server_clause}{surface_clause}")
}

/// Build WHERE clause for queries that GROUP BY `wire_api` or `model`. The
/// group dimension is always forced to a specific value (never `'*'`); the
/// remaining dimensions follow the same filter/tier rules as
/// [`build_dimension_where`]. Any non-recognized `group_by` falls through to
/// the ungrouped builder.
pub fn build_dimension_where_for_group(
    filter: &DimensionFilter,
    group_by: &str,
    escape: LiteralEscaper,
) -> String {
    match group_by {
        "wire_api" | "model" => {
            let wire_clause = if !filter.wire_apis.is_empty() {
                format!("wire_api IN ({})", sql_in_list_with(&filter.wire_apis, escape))
            } else {
                "wire_api != '*'".to_string()
            };
            let model_clause = if !filter.models.is_empty() {
                format!("model IN ({})", sql_in_list_with(&filter.models, escape))
            } else {
                "model != '*'".to_string()
            };
            let server_clause = if !filter.server_ips.is_empty() {
                format!("server_ip IN ({})", sql_in_list_with(&filter.server_ips, escape))
            } else {
                "server_ip = '*'".to_string()
            };
            let surface_clause = build_tool_surface_clause(&filter.tool_surfaces, escape);
            format!("{wire_clause} AND {model_clause} AND {server_clause}{surface_clause}")
        }
        _ => build_dimension_where(filter, escape),
    }
}

/// Optional `tool_surface IN (...)` segment, prefixed with ` AND ` when
/// present. Returns an empty string when no filter is set so the query stays
/// unchanged and rolls up across all surfaces (including NULL).
fn build_tool_surface_clause(surfaces: &[String], escape: LiteralEscaper) -> String {
    if surfaces.is_empty() {
        String::new()
    } else {
        format!(" AND tool_surface IN ({})", sql_in_list_with(surfaces, escape))
    }
}

#[cfg(test)]
mod build_dimension_where_tests {
    use super::*;

    #[test]
    fn test_build_dimension_where_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api = '*' AND model = '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_server_only() {
        let f = DimensionFilter {
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api = '*' AND model = '*' AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_only() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api IN ('openai-chat') AND model != '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_model_only() {
        let f = DimensionFilter {
            models: vec!["gpt-4".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api != '*' AND model IN ('gpt-4') AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_and_model() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            models: vec!["gpt-4".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api IN ('openai-chat') AND model IN ('gpt-4') AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_wire_and_server() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api IN ('openai-chat') AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_model_and_server() {
        let f = DimensionFilter {
            models: vec!["gpt-4".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api != '*' AND model IN ('gpt-4') AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_all_three() {
        let f = DimensionFilter {
            wire_apis: vec!["openai-chat".into()],
            models: vec!["gpt-4".into()],
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where(&f, escape_standard),
            "wire_api IN ('openai-chat') AND model IN ('gpt-4') AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_for_group_wire_api_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where_for_group(&f, "wire_api", escape_standard),
            "wire_api != '*' AND model != '*' AND server_ip = '*'"
        );
    }

    #[test]
    fn test_build_dimension_where_for_group_with_server_filter() {
        let f = DimensionFilter {
            server_ips: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert_eq!(
            build_dimension_where_for_group(&f, "wire_api", escape_standard),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
        assert_eq!(
            build_dimension_where_for_group(&f, "model", escape_standard),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
    }
}

#[cfg(test)]
mod escaper_tests {
    use super::*;

    #[test]
    fn escape_clickhouse_neutralizes_backslash_quote_breakout() {
        // The ClickHouse SQL-injection payload: a trailing backslash before
        // the closing quote. With quote-only escaping this yields `'\'')...`,
        // where ClickHouse reads `\'` as an escaped quote and the literal
        // terminates early, letting the rest parse as SQL. Backslash-aware
        // escaping must double the backslash first.
        let payload = r"\') OR 1=1 --";
        assert_eq!(escape_clickhouse(payload), r"\\'') OR 1=1 --");
        // Embedded in a literal: '\\'') OR 1=1 --' — ClickHouse decodes
        // `\\` -> `\` and `''` -> `'`, so the literal content equals the input
        // and the trailing quote stays the closing quote (no breakout).
        let in_list = sql_in_list_with(&[payload.to_string()], escape_clickhouse);
        assert_eq!(in_list, r"'\\'') OR 1=1 --'");
    }

    #[test]
    fn escape_standard_leaves_backslash_untouched() {
        // DuckDB / Postgres: backslash is an ordinary character; doubling it
        // would corrupt the stored/compared value. Only the quote is doubled.
        assert_eq!(escape_standard(r"a\b"), r"a\b");
        assert_eq!(escape_standard("o'brien"), "o''brien");
    }
}
