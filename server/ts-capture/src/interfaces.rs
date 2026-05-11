//! Enumerate local pcap-visible interfaces for the Settings UI.
//!
//! Thin serializable wrapper around `pcap::Device::list()` — same source
//! `PcapLiveSource` consults at startup. Used by `GET /api/capture/interfaces`.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureInterface {
    pub name: String,
    pub description: Option<String>,
    pub addresses: Vec<String>,
    pub is_up: bool,
    pub is_running: bool,
    pub is_loopback: bool,
    pub is_wireless: bool,
}

pub fn list_interfaces() -> Result<Vec<CaptureInterface>, pcap::Error> {
    let devices = pcap::Device::list()?;
    let mut out: Vec<CaptureInterface> = devices
        .into_iter()
        .map(|d| {
            let flags = &d.flags;
            CaptureInterface {
                name: d.name,
                description: d.desc.filter(|s| !s.is_empty()),
                addresses: d
                    .addresses
                    .into_iter()
                    .map(|a| match a.addr {
                        IpAddr::V4(v) => v.to_string(),
                        IpAddr::V6(v) => v.to_string(),
                    })
                    .collect(),
                is_up: flags.is_up(),
                is_running: flags.is_running(),
                is_loopback: flags.is_loopback(),
                is_wireless: flags.is_wireless(),
            }
        })
        .collect();

    // libpcap returns the magic "any" pseudo-device on Linux. It's always
    // safe to capture on (matches the current production config) but is
    // missing from the kernel's netdev list, so it lacks addresses/flags.
    // Make sure it shows up first so users can recognize it as the
    // default-recommended choice.
    if !out.iter().any(|i| i.name == "any") {
        out.insert(
            0,
            CaptureInterface {
                name: "any".to_string(),
                description: Some("pseudo-device — capture on all interfaces".to_string()),
                addresses: Vec::new(),
                is_up: true,
                is_running: true,
                is_loopback: false,
                is_wireless: false,
            },
        );
    }
    Ok(out)
}
