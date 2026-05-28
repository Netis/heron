//! `/api/sources` — capture data source catalog.
//!
//! Returns every registered local capture source (pcap live / pcap-file /
//! cloud-probe receiver) plus every cloud-probe peer discovered at
//! runtime. The payload includes a server-side `as_of_ms` so the console
//! can compute online/idle/offline status without trusting the browser
//! clock.

use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use ts_common::source_registry::{self, SourceSnapshot};

use crate::response::{ApiError, ApiResponse};
use crate::AppState;

#[derive(Serialize)]
pub struct SourcesResponse {
    pub as_of_ms: i64,
    pub sources: Vec<SourceSnapshot>,
}

pub async fn list(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let sources = state.sources.snapshot();
    let as_of_ms = source_registry::now_ms();
    Ok(ApiResponse::ok(SourcesResponse {
        as_of_ms,
        sources,
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::sync::Arc;
    use tower::ServiceExt;
    use ts_common::source_registry::{SourceKind, SourceRegistry};
    use ts_storage::duckdb::DuckDbBackend;

    use crate::{router, AppState};

    async fn test_state() -> AppState {
        let backend = DuckDbBackend::open(":memory:").unwrap();
        <DuckDbBackend as ts_storage::StorageBackend>::init(&backend)
            .await
            .unwrap();
        AppState {
            storage: Arc::new(backend),
            sources: SourceRegistry::new(),
        }
    }

    #[tokio::test]
    async fn empty_registry_returns_empty_list() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/sources")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], 0);
        assert!(v["data"]["as_of_ms"].as_i64().unwrap() > 1_700_000_000_000);
        assert_eq!(v["data"]["sources"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn populated_registry_returns_entries() {
        let state = test_state().await;
        state.sources.register_static("eth0", SourceKind::Pcap, "eth0", None);
        state.sources.register_static(
            "tcp://0.0.0.0:5555",
            SourceKind::CloudProbeReceiver,
            "tcp://0.0.0.0:5555",
            None,
        );
        state
            .sources
            .ensure_peer("peer-uuid", "tcp://0.0.0.0:5555");
        state.sources.touch("peer-uuid", 1_700_000_001_000, false);

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/sources")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let sources = v["data"]["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 3);

        // Sorted alphabetically by key.
        assert_eq!(sources[0]["key"], "eth0");
        assert_eq!(sources[0]["kind"], "pcap");
        assert_eq!(sources[1]["key"], "peer-uuid");
        assert_eq!(sources[1]["kind"], "cloud_probe_peer");
        assert_eq!(sources[1]["parent_key"], "tcp://0.0.0.0:5555");
        assert_eq!(sources[1]["packets"], 1);
        assert_eq!(sources[2]["key"], "tcp://0.0.0.0:5555");
        assert_eq!(sources[2]["kind"], "cloud_probe_receiver");
    }
}
