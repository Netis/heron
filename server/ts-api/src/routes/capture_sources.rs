//! `PUT /api/capture/sources` — replace the source list for one
//! `[[pipeline]]` in the on-disk config, then self-restart so the new
//! sources actually take effect.
//!
//! Wire:
//!
//! 1. Validate every Pcap source (interface visible to libpcap + BPF
//!    compiles).
//! 2. Patch the TOML file at `ApiRuntimeConfigContext.config_path` via
//!    `toml_edit` (preserves comments outside the rewritten array, atomic
//!    write via tmp + rename).
//! 3. Respond `{ restart_in_ms }` to the client.
//! 4. Spawn a delayed task that re-execs the current binary with the same
//!    argv. Capture is reopened from scratch, in-memory state (active turns,
//!    queues) is reset — same effect as `kill -TERM` + `nohup … &`.
//!
//! Not authenticated. Same trust model as the rest of the API: assumed to
//! be bound to a trusted network.

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use ts_common::config::CaptureSourceConfig;
use ts_common::config_edit::patch_pipeline_sources;

use crate::response::{ApiError, ApiResponse};
use crate::ApiRuntimeConfigContext;

#[derive(Deserialize)]
pub struct UpdateSourcesRequest {
    pub pipeline_name: String,
    pub sources: Vec<CaptureSourceConfig>,
}

#[derive(Serialize)]
struct UpdateSourcesResponse {
    /// Milliseconds the server will wait between sending this response and
    /// re-execing itself. Lets clients show a "restarting…" overlay and
    /// poll `/api/health` for the new uptime.
    restart_in_ms: u64,
}

const RESTART_DELAY: Duration = Duration::from_millis(500);

pub async fn update(
    State(ctx): State<ApiRuntimeConfigContext>,
    Json(req): Json<UpdateSourcesRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if req.pipeline_name.is_empty() {
        return Err(ApiError::InvalidParam(
            "pipeline_name is required".to_string(),
        ));
    }
    if req.sources.is_empty() {
        return Err(ApiError::InvalidParam(
            "at least one source is required (refusing to disarm capture)".to_string(),
        ));
    }

    // Validate every pcap source. Other source kinds are accepted as-is —
    // pcap-file gets path-checked in run_pipeline, cloud-probe needs nothing
    // beyond a parseable endpoint.
    for s in &req.sources {
        if let CaptureSourceConfig::Pcap {
            interface,
            bpf_filter,
            ..
        } = s
        {
            ts_capture::interfaces::validate_pcap_source(interface, bpf_filter.as_deref())
                .map_err(|e| ApiError::InvalidParam(format!("invalid pcap source: {e}")))?;
        }
    }

    // Write TOML
    let path = PathBuf::from(&ctx.config_path);
    patch_pipeline_sources(&path, &req.pipeline_name, &req.sources).map_err(|e| match e {
        ts_common::config_edit::ConfigEditError::PipelineNotFound(_) => {
            ApiError::NotFound(format!(
                "pipeline '{}' is not declared in the config file (running in CLI mode?)",
                req.pipeline_name
            ))
        }
        other => ApiError::Internal(format!("config write failed: {other}")),
    })?;

    tracing::warn!(
        config_path = %ctx.config_path,
        pipeline = %req.pipeline_name,
        sources = req.sources.len(),
        "Settings: capture sources updated; self-restart scheduled in {:?}",
        RESTART_DELAY,
    );

    // Schedule self-restart after the response is on the wire.
    tokio::spawn(async {
        tokio::time::sleep(RESTART_DELAY).await;
        do_self_restart();
    });

    Ok(ApiResponse::ok(UpdateSourcesResponse {
        restart_in_ms: RESTART_DELAY.as_millis() as u64,
    }))
}

fn do_self_restart() {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "cannot resolve current_exe(); aborting restart");
            return;
        }
    };

    // /proc/self/exe reports `/path/to/binary (deleted)` when the running
    // binary's inode was replaced (cargo build does this on every release
    // rebuild). std::env::current_exe() passes that suffix through verbatim,
    // and execv() then bombs with ENOENT. Strip it — the bare path resolves
    // to whatever's on disk now, which is exactly the new binary we want.
    let exe = strip_deleted_suffix(exe);

    if !exe.exists() {
        tracing::error!(
            exe = %exe.display(),
            "self-restart target does not exist; aborting"
        );
        return;
    }

    tracing::warn!(exe = %exe.display(), "Settings: execv'ing self");
    let err = std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .exec();
    // exec() only returns on failure (otherwise the process image was replaced).
    tracing::error!(error = %err, "execv failed; process continues with previous config");

    fn strip_deleted_suffix(p: PathBuf) -> PathBuf {
        let s = p.to_string_lossy();
        if let Some(base) = s.strip_suffix(" (deleted)") {
            PathBuf::from(base)
        } else {
            p
        }
    }
}
