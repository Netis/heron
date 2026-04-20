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
    #[serde(default = "default_model_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_model_sort_by() -> String {
    "request_count".to_string()
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

pub fn to_time_range(start: i64, end: i64) -> TimeRange {
    TimeRange {
        start_us: start * 1_000_000,
        end_us: end * 1_000_000,
    }
}

pub fn to_dimension_filter(
    wire_api: &Option<String>,
    model: &Option<String>,
    server_ip: &Option<String>,
) -> DimensionFilter {
    DimensionFilter {
        wire_apis: parse_csv(wire_api),
        models: parse_csv(model),
        server_ips: parse_csv(server_ip),
    }
}
