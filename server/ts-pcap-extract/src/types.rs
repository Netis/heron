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

/// One time-bounded 5-tuple filter. Optional fields are wildcards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractFlow {
    pub start_us: i64,
    pub end_us: i64,
    pub client_ip: Option<IpAddr>,
    pub client_port: Option<u16>,
    pub server_ip: Option<IpAddr>,
    pub server_port: Option<u16>,
}

/// A source-level extraction request that can match several exact flows.
#[derive(Debug, Clone)]
pub struct ExtractRequestSet {
    pub source_id: String,
    pub start_us: i64,
    pub end_us: i64,
    pub flows: Vec<ExtractFlow>,
}

impl From<ExtractRequest> for ExtractRequestSet {
    fn from(req: ExtractRequest) -> Self {
        let flow = ExtractFlow {
            start_us: req.start_us,
            end_us: req.end_us,
            client_ip: req.client_ip,
            client_port: req.client_port,
            server_ip: req.server_ip,
            server_port: req.server_port,
        };
        Self {
            source_id: req.source_id,
            start_us: req.start_us,
            end_us: req.end_us,
            flows: vec![flow],
        }
    }
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

        let set = ExtractRequestSet::from(req);
        assert_eq!(set.source_id, "en0");
        assert_eq!(set.flows.len(), 1);
        assert_eq!(set.flows[0].start_us, 0);
    }
}
