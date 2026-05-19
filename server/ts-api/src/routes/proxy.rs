//! `GET /api/proxy/ca.pem` — return the on-disk PEM-encoded root CA so
//! users can install it in their trust store without SSH-ing to the
//! host. Read-only and unauthenticated by design: the CA cert is the
//! *public* half of the pair — what makes it dangerous is the private
//! key, which we never expose.
//!
//! Returns 404 when proxy is disabled or the CA hasn't been generated
//! yet (the CA is created on first proxy startup; the file won't exist
//! before then).

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use std::path::Path;

use crate::ApiRuntimeConfigContext;

pub async fn ca_pem(State(ctx): State<ApiRuntimeConfigContext>) -> impl IntoResponse {
    let proxy = &ctx.config.proxy;
    let pem_path = Path::new(&proxy.ca_dir).join("ca.pem");
    match tokio::fs::read_to_string(&pem_path).await {
        Ok(pem) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-pem-file"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"tokenscope-ca.pem\"",
                ),
                // Trust anchors don't change often; let browsers cache
                // briefly so a Settings page re-render doesn't hammer
                // the disk.
                (header::CACHE_CONTROL, "private, max-age=60"),
            ],
            pem,
        )
            .into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"proxy CA not generated yet. Set proxy.enabled = true in the config and restart; the CA is generated on first startup."}"#.to_string(),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/json")],
            format!(r#"{{"error":"failed to read CA: {e}"}}"#),
        )
            .into_response(),
    }
}
