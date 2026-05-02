//! Smoke test: build a minimal Router with the `/api/pcap/extract` route
//! and a stub `Vec<PipelineRoot>`, hit it with a synthetic GET, assert
//! the response shape.
//!
//! Uses `axum::Router::new().route(...).with_state(...)` directly rather
//! than `ts_api::router(...)` to keep the test focused on the new route.

use std::sync::Arc;

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use ts_pcap_extract::PipelineRoot;
use tower::util::ServiceExt;

#[tokio::test]
async fn returns_header_only_pcap_when_no_files() {
    let roots: Arc<Vec<PipelineRoot>> = Arc::new(vec![PipelineRoot {
        name: "local".into(),
        dump_dir: std::path::PathBuf::from("/nonexistent"),
    }]);
    let app: Router = Router::new()
        .route("/api/pcap/extract", get(ts_api::routes::pcap_extract::handler))
        .with_state(roots);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/pcap/extract?source_id=en0&start=0&end=30000000")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    // Header-only: 24 bytes, magic at start.
    assert_eq!(body.len(), 24);
    assert_eq!(&body[0..4], &0xa1b2_c3d4u32.to_le_bytes());
}

#[tokio::test]
async fn rejects_window_too_wide_with_400() {
    let roots: Arc<Vec<PipelineRoot>> = Arc::new(vec![]);
    let app: Router = Router::new()
        .route("/api/pcap/extract", get(ts_api::routes::pcap_extract::handler))
        .with_state(roots);

    // 1h + 1us
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/pcap/extract?source_id=en0&start=0&end=3600000001")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
