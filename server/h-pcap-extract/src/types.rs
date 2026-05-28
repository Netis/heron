use std::net::IpAddr;
use std::path::PathBuf;

/// A pcap-extract request. Optional 5-tuple fields are wildcards (any value matches).
#[derive(Debug, Clone)]
pub struct ExtractRequest {
    pub source_id: String,
    pub start_us: i64,
    pub end_us: i64,
    pub client_ip: Option<IpAddr>,
    pub client_port: Option<u16>,
    pub server_ip: Option<IpAddr>,
    pub server_port: Option<u16>,
}

/// One pipeline's root info, supplied by the runtime.
#[derive(Debug, Clone)]
pub struct PipelineRoot {
    /// Raw (un-sanitized) pipeline name; the crate sanitizes internally.
    pub name: String,
    /// `pipeline.pcap_dump.dir` — base directory; the pipeline subdir is
    /// appended by this crate.
    pub dump_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("link_type mismatch across candidate files (got {got}, expected {expected})")]
    LinkTypeMismatch { expected: u32, got: u32 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_request_constructs() {
        let req = ExtractRequest {
            source_id: "en0".to_string(),
            start_us: 0,
            end_us: 1_000_000,
            client_ip: None,
            client_port: None,
            server_ip: None,
            server_port: None,
        };
        assert_eq!(req.source_id, "en0");
    }
}
