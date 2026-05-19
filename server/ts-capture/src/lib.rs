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
mod factory;
pub mod heartbeat;
pub mod interfaces;
mod packet;
mod pcap_dump;
mod pcap_file;
mod pcap_live;
mod pcap_retention;
mod routing;
mod source;

pub use cloud_probe::CloudProbeSource;
pub use factory::build_source;
pub use packet::{RawPacket, HEARTBEAT_ETHER_TYPE, HEARTBEAT_PACKET_LEN};
pub use pcap_dump::{pcap_dump_dir_for, PacketDumper, PacketDumperConfig};
pub use pcap_file::PcapFileSource;
pub use pcap_live::PcapLiveSource;
pub use pcap_retention::spawn_pcap_retention_task;
pub use routing::RoutingSender;
pub use source::CaptureSource;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("pcap error: {0}")]
    Pcap(#[from] pcap::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("zmq error: {0}")]
    Zmq(#[from] zeromq::ZmqError),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;
