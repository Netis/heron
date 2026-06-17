//! Pipeline stage 1: packet capture.
//!
//! Produces `RawPacket` items from one or more sources and feeds them into
//! the downstream protocol stage. Supported sources:
//!
//! * `PcapLiveSource` — local NIC via libpcap (runs on `spawn_blocking`)
//! * `PcapFileSource` — replay from a pcap file (for dev/test)
//! * `CloudProbeSource` — remote packets via ZMQ from cloud-probe
//!
//! Sources implement the [`CaptureSource`] trait and emit into a
//! [`RoutingSender`] that transparently routes packets to one of D dispatcher
//! channels by `hash(source_id) % D`. When `dispatcher_count = 1` (default)
//! the routing is a no-op. This crate performs no protocol parsing; it is
//! strictly an I/O boundary.

mod cloud_probe;
pub mod ebpf;
mod factory;
pub mod heartbeat;
pub mod interfaces;
mod packet;
mod pcap_dump;
mod pcap_file;
mod pcap_live;
mod pcap_retention;
mod probe_uplink;
mod routing;
mod source;
pub mod synth;
// Throwaway-PKI test helper. Compiled for this crate's own tests, and exposed to
// other crates' integration tests via the opt-in `test-helpers` feature (so the
// distributed-capture verification in h-turn can reuse one canonical PKI). Never
// in a default/release build.
#[cfg(any(test, feature = "test-helpers"))]
pub mod testpki;
mod thin_probe;
pub mod tls;
pub mod wire;

pub use cloud_probe::CloudProbeSource;
pub use ebpf::{BootClock, EbpfPump, SslEvent};
pub use factory::build_source;
pub use h_common::process::ProcessInfo;
pub use packet::{RawPacket, HEARTBEAT_ETHER_TYPE, HEARTBEAT_PACKET_LEN};
pub use pcap_dump::{pcap_dump_dir_for, PacketDumper, PacketDumperConfig};
pub use pcap_file::PcapFileSource;
pub use pcap_live::PcapLiveSource;
pub use pcap_retention::spawn_pcap_retention_task;
pub use routing::RoutingSender;
pub use source::CaptureSource;
pub use probe_uplink::ProbeUplink;
pub use synth::{ConnTuple, FlowSynthesizer, StreamDir, SynthConfig};
pub use thin_probe::ThinProbeSource;
pub use wire::{decode_frame, encode_frame, ProbeBatch, WireError, PROTOCOL_VERSION};

/// Whether this binary was compiled with the on-host eBPF capture loader
/// (`--features ebpf`, Linux only). The Settings UI gates its "enable eBPF
/// capture" toggle on this, and the capture-sources API rejects an `ebpf`
/// source on a build that can't run it (clearer than a deferred factory error).
pub fn ebpf_available() -> bool {
    cfg!(all(target_os = "linux", feature = "ebpf"))
}

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("pcap error: {0}")]
    Pcap(#[from] pcap::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("zmq error: {0}")]
    Zmq(#[from] zeromq::ZmqError),

    #[error("tls error: {0}")]
    Tls(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;
