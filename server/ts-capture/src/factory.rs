use ts_common::config::CaptureSourceConfig;

use crate::cloud_probe::CloudProbeSource;
use crate::pcap_file::PcapFileSource;
use crate::pcap_live::PcapLiveSource;
use crate::source::CaptureSource;

/// Build a [`CaptureSource`] from configuration.
pub fn build_source(config: &CaptureSourceConfig) -> crate::Result<Box<dyn CaptureSource>> {
    match config {
        CaptureSourceConfig::Pcap {
            interface,
            bpf_filter,
            snaplen,
            heartbeat_interval_ms,
            stream_id,
        } => {
            let sid = stream_id.clone().unwrap_or_else(|| interface.clone());
            Ok(Box::new(PcapLiveSource::new(
                interface.clone(),
                bpf_filter.clone(),
                *snaplen,
                *heartbeat_interval_ms,
                sid,
            )))
        }
        CaptureSourceConfig::PcapFile { path, stream_id, .. } => {
            let sid = stream_id.clone().unwrap_or_else(|| {
                std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path)
                    .to_string()
            });
            Ok(Box::new(PcapFileSource::new(path.into(), sid)))
        }
        CaptureSourceConfig::CloudProbe { endpoint, recv_hwm } => Ok(Box::new(
            CloudProbeSource::new(endpoint.clone(), *recv_hwm),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_pcap_file_source() {
        let config = CaptureSourceConfig::PcapFile {
            path: "/tmp/test.pcap".to_string(),
            realtime: false,
            stream_id: None,
        };
        assert!(build_source(&config).is_ok());
    }

    #[test]
    fn test_build_pcap_live_source() {
        let config = CaptureSourceConfig::Pcap {
            interface: "lo0".to_string(),
            bpf_filter: None,
            snaplen: 65535,
            heartbeat_interval_ms: 1000,
            stream_id: None,
        };
        assert!(build_source(&config).is_ok());
    }

    #[test]
    fn test_build_cloud_probe_source() {
        let config = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
            recv_hwm: 1000,
        };
        assert!(build_source(&config).is_ok());
    }
}
