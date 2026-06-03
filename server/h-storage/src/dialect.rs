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

/// Format a list of string values as a SQL IN list with single-quote escaping.
pub fn sql_in_list(values: &[String]) -> String {
    values
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Split a comma-separated multi-select filter value (e.g. the `agent_kind`
/// query param `"claude-cli,codex-cli"`) into trimmed, non-empty parts.
///
/// Shared by every storage backend so the CSV → IN-list filter behaves
/// identically across them — the `agent_kind` multi-select bug recurred
/// precisely because each backend (and the turns vs sessions paths) kept its
/// own copy and a fix missed one. One definition, no drift. The API layer has
/// its own equivalent (`h-api`) for params it parses before they reach storage.
pub fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
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
pub fn build_dimension_where(filter: &DimensionFilter) -> String {
    let has_wire = !filter.wire_apis.is_empty();
    let has_model = !filter.models.is_empty();
    let has_server = !filter.server_ips.is_empty();

    let (wire_clause, model_clause) = if !has_wire && !has_model {
        // Stay on (*, *, ·) tier.
        ("wire_api = '*'".to_string(), "model = '*'".to_string())
    } else {
        // Drop to (W, M, ·) tier — either IN-list or all specific values.
        let w = if has_wire {
            format!("wire_api IN ({})", sql_in_list(&filter.wire_apis))
        } else {
            "wire_api != '*'".to_string()
        };
        let m = if has_model {
            format!("model IN ({})", sql_in_list(&filter.models))
        } else {
            "model != '*'".to_string()
        };
        (w, m)
    };

    let server_clause = if has_server {
        format!("server_ip IN ({})", sql_in_list(&filter.server_ips))
    } else {
        "server_ip = '*'".to_string()
    };

    let surface_clause = build_tool_surface_clause(&filter.tool_surfaces);
    format!("{wire_clause} AND {model_clause} AND {server_clause}{surface_clause}")
}

/// Build WHERE clause for queries that GROUP BY `wire_api` or `model`. The
/// group dimension is always forced to a specific value (never `'*'`); the
/// remaining dimensions follow the same filter/tier rules as
/// [`build_dimension_where`]. Any non-recognized `group_by` falls through to
/// the ungrouped builder.
pub fn build_dimension_where_for_group(filter: &DimensionFilter, group_by: &str) -> String {
    match group_by {
        "wire_api" | "model" => {
            let wire_clause = if !filter.wire_apis.is_empty() {
                format!("wire_api IN ({})", sql_in_list(&filter.wire_apis))
            } else {
                "wire_api != '*'".to_string()
            };
            let model_clause = if !filter.models.is_empty() {
                format!("model IN ({})", sql_in_list(&filter.models))
            } else {
                "model != '*'".to_string()
            };
            let server_clause = if !filter.server_ips.is_empty() {
                format!("server_ip IN ({})", sql_in_list(&filter.server_ips))
            } else {
                "server_ip = '*'".to_string()
            };
            let surface_clause = build_tool_surface_clause(&filter.tool_surfaces);
            format!("{wire_clause} AND {model_clause} AND {server_clause}{surface_clause}")
        }
        _ => build_dimension_where(filter),
    }
}

/// Optional `tool_surface IN (...)` segment, prefixed with ` AND ` when
/// present. Returns an empty string when no filter is set so the query stays
/// unchanged and rolls up across all surfaces (including NULL).
fn build_tool_surface_clause(surfaces: &[String]) -> String {
    if surfaces.is_empty() {
        String::new()
    } else {
        format!(" AND tool_surface IN ({})", sql_in_list(surfaces))
    }
}

#[cfg(test)]
mod parse_csv_tests {
    use super::parse_csv;

    #[test]
    fn splits_trims_and_drops_empties() {
        assert_eq!(parse_csv("a,b,c"), vec!["a", "b", "c"]);
        assert_eq!(parse_csv("a, b , c"), vec!["a", "b", "c"]);
        assert_eq!(parse_csv("a,,b"), vec!["a", "b"]);
        assert_eq!(parse_csv("single"), vec!["single"]);
        assert_eq!(parse_csv(""), Vec::<String>::new());
        assert_eq!(parse_csv("  "), Vec::<String>::new());
    }
}

#[cfg(test)]
mod build_dimension_where_tests {
    use super::*;

    #[test]
    fn test_build_dimension_where_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where(&f),
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
            build_dimension_where(&f),
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
            build_dimension_where(&f),
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
            build_dimension_where(&f),
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
            build_dimension_where(&f),
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
            build_dimension_where(&f),
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
            build_dimension_where(&f),
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
            build_dimension_where(&f),
            "wire_api IN ('openai-chat') AND model IN ('gpt-4') AND server_ip IN ('10.0.0.1')"
        );
    }

    #[test]
    fn test_build_dimension_where_for_group_wire_api_no_filter() {
        let f = DimensionFilter::default();
        assert_eq!(
            build_dimension_where_for_group(&f, "wire_api"),
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
            build_dimension_where_for_group(&f, "wire_api"),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
        assert_eq!(
            build_dimension_where_for_group(&f, "model"),
            "wire_api != '*' AND model != '*' AND server_ip IN ('10.0.0.1')"
        );
    }
}
