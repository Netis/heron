//! `GET /api/capture/interfaces` — local pcap-visible network interfaces.
//!
//! Reads the same list `PcapLiveSource` would consult at startup, so the
//! Settings UI can offer a dropdown that exactly matches what capture can
//! actually open. Returns 500 if libpcap itself fails to enumerate (rare —
//! usually means CAP_NET_RAW is missing).

use axum::response::IntoResponse;
use serde::Serialize;
use ts_capture::interfaces::{list_interfaces, CaptureInterface};

use crate::response::{ApiError, ApiResponse};

#[derive(Serialize)]
struct InterfacesResponse {
    interfaces: Vec<CaptureInterface>,
}

pub async fn interfaces() -> Result<impl IntoResponse, ApiError> {
    let interfaces = list_interfaces()
        .map_err(|e| ApiError::Internal(format!("failed to list interfaces: {e}")))?;
    Ok(ApiResponse::ok(InterfacesResponse { interfaces }))
}
