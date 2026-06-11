//! Process attribution for a captured connection.
//!
//! Packet-tap sources (pcap live / file, cloud-probe) observe traffic on the
//! wire and cannot know which local process owns a connection, so their packets
//! carry no [`ProcessInfo`]. The eBPF SSL-uprobe source, by contrast, runs in
//! the kernel context of the calling process and learns its pid / `comm` (and,
//! best-effort in userspace, its executable path) for free — that on-host
//! attribution is the eBPF path's differentiating value over a passive tap.
//!
//! The type lives here (not in `h-capture` where `RawPacket` is defined) so
//! every pipeline crate — capture, protocol, llm, storage — can name it through
//! its existing `h-common` dependency without adding a cross-crate edge. It sits
//! alongside the [`agent`](crate::agent) taxonomies for the same reason.

use serde::{Deserialize, Serialize};

/// Identifies the local process that owns a captured connection.
///
/// Only ever `Some(_)` on packets produced by an attribution-capable source
/// (today: eBPF). Threaded end-to-end — `RawPacket` → `ParsedPacket` →
/// `TcpFlow` → `Http{Request,Response}Data` → `LlmCall` → storage → API — so a
/// call surfaced in the console can be traced back to the agent process that
/// made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Kernel process id of the connection owner.
    pub pid: u32,
    /// The process `comm` (kernel `TASK_COMM_LEN`-bounded name, ≤15 chars), e.g.
    /// `python3`, `node`, `claude`. Cheap to obtain in the BPF program.
    pub comm: String,
    /// Absolute executable path (`/proc/<pid>/exe`), resolved best-effort in
    /// userspace. `None` when the link could not be read (process already exited,
    /// permission denied) or on a source that does not resolve it.
    pub exe: Option<String>,
}

impl ProcessInfo {
    /// Construct from the fields an eBPF event always carries (pid + comm),
    /// leaving the userspace-resolved `exe` empty.
    pub fn new(pid: u32, comm: impl Into<String>) -> Self {
        Self {
            pid,
            comm: comm.into(),
            exe: None,
        }
    }

    /// Builder-style setter for the best-effort executable path.
    pub fn with_exe(mut self, exe: Option<String>) -> Self {
        self.exe = exe;
        self
    }
}
