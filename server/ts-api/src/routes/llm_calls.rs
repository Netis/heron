use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use ts_storage::query::CallsQuery;
use ts_storage::StorageBackend;

use crate::extractors::{Path, Query};
use crate::params::*;
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Deserialize)]
pub struct CallsParams {
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub server_ip: Option<String>,
    #[serde(default)]
    pub status_code: Option<String>,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub client_ip: Option<String>,
    /// Substring match against `request_path` (case-sensitive, `LIKE '%…%'`).
    #[serde(default)]
    pub request_path: Option<String>,
    #[serde(default = "default_calls_sort_by")]
    pub sort_by: String,
    #[serde(default = "default_calls_sort_order")]
    pub sort_order: String,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_calls_sort_by() -> String {
    "request_time".to_string()
}
fn default_calls_sort_order() -> String {
    "desc".to_string()
}
fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    50
}

pub async fn list(
    State(storage): State<Arc<dyn StorageBackend>>,
    Query(params): Query<CallsParams>,
) -> Result<impl IntoResponse, ApiError> {
    let page_size = params.page_size.min(200);
    let status_codes: Vec<u16> = parse_csv(&params.status_code)
        .iter()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| ApiError::InvalidParam(format!("invalid status_code: {s}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let query = CallsQuery {
        time_range: to_time_range(params.start, params.end),
        filter: to_dimension_filter(&params.wire_api, &params.model, &params.server_ip),
        status_codes,
        finish_reasons: parse_csv(&params.finish_reason),
        client_ips: parse_csv(&params.client_ip),
        request_path_contains: params.request_path.filter(|s| !s.is_empty()),
        sort_by: params.sort_by,
        sort_order: params.sort_order,
        page: params.page,
        page_size,
    };

    let page = storage.query_calls(&query).await?;
    Ok(ApiResponse::ok(page))
}

pub async fn detail(
    State(storage): State<Arc<dyn StorageBackend>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match storage.query_call_by_id(&id).await? {
        Some(detail) => Ok(ApiResponse::ok(detail)),
        None => Err(ApiError::NotFound(format!("call not found: {id}"))),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;
    use ts_storage::duckdb::DuckDbBackend;

    use crate::{router, ApiHealthContext, ApiMetricsContext, ApiRuntimeConfigContext};

    fn test_metrics_context() -> ApiMetricsContext {
        let sys = ts_common::internal_metrics::MetricsSystem::new();
        ApiMetricsContext {
            pipelines: vec![],
            global: sys.start(),
        }
    }

    fn test_runtime_config_context() -> ApiRuntimeConfigContext {
        ApiRuntimeConfigContext {
            config: std::sync::Arc::new(ts_common::config::AppConfig {
                pipelines: vec![],
                storage: ts_common::config::StorageConfig::default(),
                internal_metrics: ts_common::config::InternalMetricsConfig::default(),
                api: ts_common::config::ApiConfig::default(),
            }),
            config_path: "test".to_string(),
            loaded_at_ms: 0,
            version: "test",
        }
    }

    fn test_health_context() -> ApiHealthContext {
        ApiHealthContext {
            started_at_ms: 0,
            version: "test",
            pipelines: vec![],
        }
    }

    #[tokio::test]
    async fn invalid_status_code_returns_json_envelope() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
            .await
            .unwrap();
        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(
            storage,
            test_metrics_context(),
            test_runtime_config_context(),
            test_health_context(),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/llm-calls?start=0&end=1&status_code=200,abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 1001);
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains("invalid status_code: abc"),
            "message: {}",
            v["message"]
        );
    }

    #[tokio::test]
    async fn contains_params_parse() {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
            .await
            .unwrap();
        let storage: std::sync::Arc<dyn ts_storage::StorageBackend> = std::sync::Arc::new(backend);
        let app = router(
            storage,
            test_metrics_context(),
            test_runtime_config_context(),
            test_health_context(),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/llm-calls?start=0&end=1&client_ip=10.0.0.1&request_path=/v1/chat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }
}
