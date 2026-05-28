use std::sync::Arc;

use ts_common::config::CaptureSourceConfig;
use ts_common::source_registry::{SourceKind, SourceRegistry};

use crate::cloud_probe::CloudProbeSource;
use crate::pcap_dump::PacketDumperConfig;
use crate::pcap_file::PcapFileSource;
use crate::pcap_live::PcapLiveSource;
use crate::source::CaptureSource;

/// Build a [`CaptureSource`] from configuration.
///
/// `pcap_dump` is forwarded to every source built from this config: when
/// `Some(...)`, the source will open a [`PacketDumper`](crate::PacketDumper)
/// inside `run()` and mirror every non-heartbeat packet to disk, grouped by
/// `stream_id`.
///
/// `registry` is populated in place with a static entry for this source so
/// it shows up in `/api/sources` even before the first packet arrives.
/// Cloud-probe peers are inserted later at runtime by `CloudProbeSource`.
pub fn build_source(
    config: &CaptureSourceConfig,
    pcap_dump: Option<PacketDumperConfig>,
    registry: Arc<SourceRegistry>,
) -> crate::Result<Box<dyn CaptureSource>> {
    match config {
        CaptureSourceConfig::Pcap {
            interface,
            bpf_filter,
            snaplen,
            stream_id,
        } => {
            let sid = stream_id.clone().unwrap_or_else(|| interface.clone());
            registry.register_static(&sid, SourceKind::Pcap, interface, None);
            Ok(Box::new(PcapLiveSource::new(
                interface.clone(),
                bpf_filter.clone(),
                *snaplen,
                sid,
                pcap_dump,
                registry,
            )))
        }
        CaptureSourceConfig::PcapFile {
            path, stream_id, ..
        } => {
            let sid = stream_id.clone().unwrap_or_else(|| {
                std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path)
                    .to_string()
            });
            registry.register_static(&sid, SourceKind::PcapFile, path, None);
            Ok(Box::new(PcapFileSource::new(
                path.into(),
                sid,
                pcap_dump,
                registry,
            )))
        }
        CaptureSourceConfig::CloudProbe { endpoint, recv_hwm } => {
            registry.register_static(endpoint, SourceKind::CloudProbeReceiver, endpoint, None);
            Ok(Box::new(CloudProbeSource::new(
                endpoint.clone(),
                *recv_hwm,
                pcap_dump,
                registry,
            )))
        }
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
        assert!(build_source(&config, None, SourceRegistry::new()).is_ok());
    }

    #[test]
    fn test_build_pcap_live_source() {
        let config = CaptureSourceConfig::Pcap {
            interface: "lo0".to_string(),
            bpf_filter: None,
            snaplen: 65535,
            stream_id: None,
        };
        assert!(build_source(&config, None, SourceRegistry::new()).is_ok());
    }

    #[test]
    fn test_build_cloud_probe_source() {
        let config = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
            recv_hwm: 1000,
        };
        assert!(build_source(&config, None, SourceRegistry::new()).is_ok());
    }

    #[test]
    fn test_factory_populates_registry_static_entries() {
        let registry = SourceRegistry::new();
        let pcap_cfg = CaptureSourceConfig::Pcap {
            interface: "lo0".to_string(),
            bpf_filter: None,
            snaplen: 65535,
            stream_id: None,
        };
        let probe_cfg = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
            recv_hwm: 1000,
        };
        let _ = build_source(&pcap_cfg, None, registry.clone()).unwrap();
        let _ = build_source(&probe_cfg, None, registry.clone()).unwrap();

        let snap = registry.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().any(|s| s.key == "lo0" && s.kind == SourceKind::Pcap));
        assert!(snap
            .iter()
            .any(|s| s.key == "tcp://0.0.0.0:5555" && s.kind == SourceKind::CloudProbeReceiver));
    }
}
