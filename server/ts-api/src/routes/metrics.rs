use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use ts_storage::query::{MetricsModelsQuery, MetricsSummaryQuery, MetricsTimeseriesQuery};
use ts_storage::StorageBackend;

use crate::extractors::Query;
use crate::params::*;
use crate::response::{ApiError, ApiResponse};

const VALID_GRANULARITIES: &[&str] = &["10s", "1m", "5m", "1h"];

#[derive(Serialize)]
struct TimeseriesSeries {
    name: String,
    group: Option<String>,
    values: Vec<Option<f64>>,
}

#[derive(Serialize)]
struct TimeseriesData {
    timestamps: Vec<i64>,
    series: Vec<TimeseriesSeries>,
}

pub async fn timeseries(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<TimeseriesParams>,
) -> Result<impl IntoResponse, ApiError> {
    if !VALID_GRANULARITIES.contains(&params.granularity.as_str()) {
        return Err(ApiError::InvalidParam(format!(
            "granularity must be one of: {}",
            VALID_GRANULARITIES.join(", ")
        )));
    }
    let fields = parse_csv(&Some(params.fields.clone()));
    if fields.is_empty() {
        return Err(ApiError::InvalidParam("fields is required".to_string()));
    }
    if let Some(ref gb) = params.group_by {
        if gb != "provider" && gb != "model" {
            return Err(ApiError::InvalidParam(
                "group_by must be 'provider' or 'model'".to_string(),
            ));
        }
    }

    let query = MetricsTimeseriesQuery {
        time_range: to_time_range(params.start, params.end),
        granularity: params.granularity,
        filter: to_dimension_filter(&params.provider, &params.model, &params.server_ip),
        fields: fields.clone(),
        group_by: params.group_by,
    };

    let rows = storage.query_metrics_timeseries(&query).await?;

    // Pivot: rows (each with timestamp + group + values) -> timestamps[] + series[]
    let mut timestamps: Vec<i64> = Vec::new();
    let mut series_map: BTreeMap<(String, Option<String>), Vec<Option<f64>>> = BTreeMap::new();

    for row in &rows {
        let ts_idx = if let Some(pos) = timestamps.iter().position(|&t| t == row.timestamp) {
            pos
        } else {
            timestamps.push(row.timestamp);
            for values in series_map.values_mut() {
                values.push(None);
            }
            timestamps.len() - 1
        };

        for (i, field) in fields.iter().enumerate() {
            let key = (field.clone(), row.group.clone());
            let values = series_map
                .entry(key)
                .or_insert_with(|| vec![None; timestamps.len()]);
            while values.len() < timestamps.len() {
                values.push(None);
            }
            values[ts_idx] = row.values.get(i).copied().flatten();
        }
    }
    for values in series_map.values_mut() {
        while values.len() < timestamps.len() {
            values.push(None);
        }
    }

    let series = series_map
        .into_iter()
        .map(|((name, group), values)| TimeseriesSeries {
            name,
            group,
            values,
        })
        .collect();

    Ok(ApiResponse::ok(TimeseriesData { timestamps, series }))
}

pub async fn summary(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<SummaryParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = MetricsSummaryQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.provider, &params.model, &params.server_ip),
    };
    let row = storage.query_metrics_summary(&query).await?;
    Ok(ApiResponse::ok(row))
}

#[derive(Serialize)]
struct ModelsData {
    models: Vec<ts_storage::query::MetricsModelRow>,
}

pub async fn models(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<ModelsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let query = MetricsModelsQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.provider, &params.model, &params.server_ip),
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        limit: params.limit,
    };
    let rows = storage.query_metrics_models(&query).await?;
    Ok(ApiResponse::ok(ModelsData { models: rows }))
}
