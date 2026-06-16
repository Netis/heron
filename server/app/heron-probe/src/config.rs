//! `heron-probe` configuration (TOML).
//!
//! A probe is deliberately thin: it runs one [`CaptureSourceConfig`] (an `ebpf`
//! source in production; a `pcap-file` source for dev smoke-tests on a host
//! without the BPF toolchain) and ships the resulting packets to a central
//! `heron` over mTLS. All the easy-to-change wire-API decoding stays central, so
//! a probe fleet rarely needs to be upgraded.

use std::path::Path;

use serde::Deserialize;

use h_common::config::{CaptureSourceConfig, TlsClientConfig};

/// Bounded queue between the capture source and the uplink. When the uplink is
/// slow, backpressure flows here and on to the eBPF perf buffer (which drops by
/// absolute seq offset — graceful). This bound is the OOM guard.
const fn default_queue_capacity() -> usize {
    8192
}

fn default_batch_max_packets() -> usize {
    256
}

fn default_flush_ms() -> u64 {
    100
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeConfig {
    /// `host:port` of the central thin-probe listener.
    pub central_endpoint: String,
    /// SNI / certificate name to validate the central's server cert against
    /// (must match a SAN in that cert).
    pub server_name: String,
    /// This probe's identity. When unset, an empty id is sent and the central
    /// falls back to the probe's client-certificate CN — a per-probe identity
    /// with no manual config, and unspoofable across the fleet.
    #[serde(default)]
    pub source_id: Option<String>,
    /// Bounded capture→uplink queue capacity.
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    /// mTLS material (client side).
    pub tls: TlsClientConfig,
    /// The capture source to run and ship. `type = "ebpf"` in production.
    pub source: CaptureSourceConfig,
    /// Uplink batching knobs.
    #[serde(default)]
    pub batching: BatchingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchingConfig {
    /// Flush a frame once this many packets accumulate.
    #[serde(default = "default_batch_max_packets")]
    pub max_packets: usize,
    /// Flush a partial batch after at most this long.
    #[serde(default = "default_flush_ms")]
    pub flush_ms: u64,
}

impl Default for BatchingConfig {
    fn default() -> Self {
        Self {
            max_packets: default_batch_max_packets(),
            flush_ms: default_flush_ms(),
        }
    }
}

impl ProbeConfig {
    /// Load and deserialize the probe config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, config::ConfigError> {
        config::Config::builder()
            .add_source(config::File::from(path))
            .build()?
            .try_deserialize()
    }
}
