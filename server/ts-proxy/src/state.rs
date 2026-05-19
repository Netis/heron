//! Shared per-server state. Held in an `Arc` and cheaply cloned into
//! every spawned task — both per-connection (server.rs accept loop)
//! and per-CONNECT (tunnel.rs TLS termination + inner serve_connection).
//!
//! Putting this in its own module breaks an otherwise-circular include
//! between server.rs, tunnel.rs, and forward.rs (each wants to read the
//! state of the others' workload).

use crate::tls::LeafCertStore;
use crate::ProxyConfig;
use std::sync::Arc;

/// Snapshot of dependencies the proxy needs at runtime, beyond what
/// `ProxyConfig` carries. Today this is a placeholder for the joiner
/// channel that will be wired in once `forward.rs` starts emitting
/// captured exchanges into the existing pipeline (see task #93).
pub struct ProxyDeps {
    // TODO(#93): joiner_event_tx: mpsc::Sender<HttpJoinerEvent>
    pub _placeholder: (),
}

impl Default for ProxyDeps {
    fn default() -> Self {
        Self { _placeholder: () }
    }
}

pub struct ProxyState {
    pub config: ProxyConfig,
    pub leaf_store: Arc<LeafCertStore>,
    pub deps: ProxyDeps,
}
