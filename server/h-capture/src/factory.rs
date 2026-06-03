use h_common::config::CaptureSourceConfig;

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
/// `source_id`.
pub fn build_source(
    config: &CaptureSourceConfig,
    pcap_dump: Option<PacketDumperConfig>,
) -> crate::Result<Box<dyn CaptureSource>> {
    match config {
        CaptureSourceConfig::Pcap {
            interface,
            bpf_filter,
            snaplen,
            source_id,
        } => {
            let sid = source_id.clone().unwrap_or_else(|| interface.clone());
            Ok(Box::new(PcapLiveSource::new(
                interface.clone(),
                bpf_filter.clone(),
                *snaplen,
                sid,
                pcap_dump,
            )))
        }
        CaptureSourceConfig::PcapFile {
            path,
            source_id,
            loop_count,
            loop_secs,
            ..
        } => {
            let sid = source_id.clone().unwrap_or_else(|| {
                std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path)
                    .to_string()
            });
            Ok(Box::new(
                PcapFileSource::new(path.into(), sid, pcap_dump)
                    .with_loop(*loop_count, *loop_secs),
            ))
        }
        CaptureSourceConfig::CloudProbe { endpoint, recv_hwm } => Ok(Box::new(
            CloudProbeSource::new(endpoint.clone(), *recv_hwm, pcap_dump),
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
            source_id: None,
            loop_count: 1,
            loop_secs: 0,
        };
        assert!(build_source(&config, None).is_ok());
    }

    #[test]
    fn test_build_pcap_live_source() {
        let config = CaptureSourceConfig::Pcap {
            interface: "lo0".to_string(),
            bpf_filter: None,
            snaplen: 65535,
            source_id: None,
        };
        assert!(build_source(&config, None).is_ok());
    }

    #[test]
    fn test_build_cloud_probe_source() {
        let config = CaptureSourceConfig::CloudProbe {
            endpoint: "tcp://0.0.0.0:5555".to_string(),
            recv_hwm: 1000,
        };
        assert!(build_source(&config, None).is_ok());
    }
}
