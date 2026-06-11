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
            rate_pps,
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
                    .with_loop(*loop_count, *loop_secs)
                    .with_rate_pps(*rate_pps),
            ))
        }
        CaptureSourceConfig::CloudProbe { endpoint, recv_hwm } => Ok(Box::new(
            CloudProbeSource::new(endpoint.clone(), *recv_hwm, pcap_dump),
        )),
        CaptureSourceConfig::Ebpf { .. } => build_ebpf_source(config, pcap_dump),
    }
}

/// Construct the eBPF capture source. Only available on Linux builds compiled
/// with the `ebpf` feature; every other build returns a clear error so a config
/// referencing an `ebpf` source fails loudly instead of silently doing nothing.
#[cfg(all(target_os = "linux", feature = "ebpf"))]
fn build_ebpf_source(
    config: &CaptureSourceConfig,
    pcap_dump: Option<PacketDumperConfig>,
) -> crate::Result<Box<dyn CaptureSource>> {
    Ok(Box::new(crate::ebpf::EbpfSource::from_config(
        config, pcap_dump,
    )?))
}

#[cfg(not(all(target_os = "linux", feature = "ebpf")))]
fn build_ebpf_source(
    _config: &CaptureSourceConfig,
    _pcap_dump: Option<PacketDumperConfig>,
) -> crate::Result<Box<dyn CaptureSource>> {
    Err(crate::CaptureError::Other(
        "ebpf capture is only available on Linux builds compiled with \
         `--features ebpf` (requires CAP_BPF + BTF on the host)"
            .to_string(),
    ))
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
            rate_pps: 0,
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

    /// On default builds (no `ebpf` feature) the factory must reject an `ebpf`
    /// source with a clear error rather than silently producing nothing.
    #[cfg(not(all(target_os = "linux", feature = "ebpf")))]
    #[test]
    fn test_build_ebpf_source_unavailable_without_feature() {
        let config = CaptureSourceConfig::Ebpf {
            source_id: None,
            ssl_libs: vec![],
            targets: vec![],
            pid_allowlist: vec![],
            segment_size: 16 * 1024,
        };
        // `Box<dyn CaptureSource>` is not Debug, so avoid `unwrap_err`.
        let err = match build_source(&config, None) {
            Ok(_) => panic!("ebpf source should be unavailable without the feature"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("ebpf"),
            "error should mention ebpf, got: {err}"
        );
    }
}
