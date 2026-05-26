use serde::Deserialize;
use ts_storage::query::{DimensionFilter, TimeRange};

#[derive(Debug, Deserialize)]
pub struct TimeseriesParams {
    pub start: i64,
    pub end: i64,
    pub granularity: String,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub tool_surface: Option<String>,
    pub fields: String,
    #[serde(default)]
    pub group_by: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SummaryParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub tool_surface: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelsParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub tool_surface: Option<String>,
    #[serde(default = "default_model_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_model_sort_by() -> String {
    "call_count".to_string()
}

fn default_sort_order() -> String {
    "desc".to_string()
}

fn default_limit() -> u32 {
    20
}

pub fn parse_csv(s: &Option<String>) -> Vec<String> {
    match s {
        Some(s) if !s.is_empty() => s
            .split(',')
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Upper bound for `start` / `end` query params expressed in **seconds since
/// epoch**. 4_102_444_800 = 2100-01-01T00:00:00Z. Anything larger is almost
/// certainly the caller passing milliseconds (or microseconds) by mistake —
/// e.g., a bookmarked URL copied from a tool that uses ms. Multiplying such
/// a value by 1_000_000 below would push the resulting timestamp past the
/// year DuckDB's SQL parser accepts and yield strings like
/// "+58346-09-15 01:12:57" which crash the count/list queries with a
/// Conversion Error.
const MAX_SECONDS_SINCE_EPOCH: i64 = 4_102_444_800;

pub fn to_time_range(start: i64, end: i64) -> Result<TimeRange, crate::response::ApiError> {
    use crate::response::ApiError;
    for (name, v) in [("start", start), ("end", end)] {
        if v < 0 {
            return Err(ApiError::InvalidParam(format!(
                "{name}={v}: negative seconds-since-epoch not allowed"
            )));
        }
        if v > MAX_SECONDS_SINCE_EPOCH {
            return Err(ApiError::InvalidParam(format!(
                "{name}={v}: value exceeds {MAX_SECONDS_SINCE_EPOCH} \
                 (year 2100 in seconds-since-epoch). The API expects seconds, \
                 not milliseconds or microseconds."
            )));
        }
    }
    Ok(TimeRange {
        start_us: start * 1_000_000,
        end_us: end * 1_000_000,
    })
}

#[cfg(test)]
mod to_time_range_tests {
    use super::*;

    #[test]
    fn accepts_sane_seconds() {
        let r = to_time_range(1_700_000_000, 1_700_000_060).unwrap();
        assert_eq!(r.start_us, 1_700_000_000_000_000);
        assert_eq!(r.end_us, 1_700_000_060_000_000);
    }

    #[test]
    fn rejects_milliseconds_passed_as_seconds() {
        // 1_747_000_000_000 = May 2025 in *milliseconds*; far past year 2100
        // in seconds. The previous code would silently multiply by 1e6,
        // overflow into year 58000+, and corrupt the SQL count query.
        let err = to_time_range(1_747_000_000_000, 1_747_000_001_000).unwrap_err();
        assert!(matches!(err, crate::response::ApiError::InvalidParam(_)));
    }

    #[test]
    fn rejects_negative_seconds() {
        let err = to_time_range(-1, 1_700_000_000).unwrap_err();
        assert!(matches!(err, crate::response::ApiError::InvalidParam(_)));
    }
}

pub fn to_dimension_filter(
    wire_api: &Option<String>,
    model: &Option<String>,
    server_ip: &Option<String>,
    tool_surface: &Option<String>,
) -> DimensionFilter {
    DimensionFilter {
        wire_apis: parse_csv(wire_api),
        models: parse_csv(model),
        server_ips: parse_csv(server_ip),
        tool_surfaces: parse_csv(tool_surface),
    }
}
