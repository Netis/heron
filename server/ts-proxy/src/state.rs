//! Shared per-server state. Held in an `Arc` and cheaply cloned into
//! every spawned task — both per-connection (server.rs accept loop)
//! and per-CONNECT (tunnel.rs TLS termination + inner serve_connection).
//!
//! Putting this in its own module breaks an otherwise-circular include
//! between server.rs, tunnel.rs, and forward.rs (each wants to read the
//! state of the others' workload).

use crate::tls::LeafCertStore;
use crate::upstream::UpstreamClient;
use crate::ProxyConfig;
use std::net::SocketAddr;
use std::sync::Arc;
use ts_llm::wire_apis::build_default_wire_api_registry;
use ts_llm::WireApiRegistry;
use ts_protocol::joiner::HttpJoinerEvent;

/// Per-tunnel addressing context — what came in on the CONNECT line and
/// who's at the other end. Passed alongside the `ProxyState` into
/// `handle_inner_request` so the forwarder can build the upstream URI
/// and stamp `FlowKey` correctly.
#[derive(Clone)]
pub struct TunnelContext {
    pub host: String,
    pub port: u16,
    pub client_peer: SocketAddr,
}

/// Snapshot of dependencies the proxy needs at runtime, beyond what
/// `ProxyConfig` carries. The joiner sender (`joiner_event_tx`) is the
/// only seam through which captured exchanges land in the storage
/// pipeline — when `None`, captures are dropped (useful for stand-alone
/// integration tests that don't care about persistence). Task #93 will
/// have `main.rs` always pass a real sender.
#[derive(Clone)]
pub struct ProxyDeps {
    pub joiner_event_tx: Option<tokio::sync::mpsc::Sender<HttpJoinerEvent>>,
    pub upstream: UpstreamClient,
}

impl Default for ProxyDeps {
    fn default() -> Self {
        Self {
            joiner_event_tx: None,
            upstream: UpstreamClient::with_webpki_roots(),
        }
    }
}

pub struct ProxyState {
    pub config: ProxyConfig,
    pub leaf_store: Arc<LeafCertStore>,
    pub deps: ProxyDeps,
    /// Shared registry of wire-api detectors. Used at the proxy edge
    /// to decide LLM-vs-not; downstream `LlmProcessor` builds its own
    /// instance and re-detects.
    pub registry: Arc<WireApiRegistry>,
}

impl ProxyState {
    pub fn new(config: ProxyConfig, leaf_store: Arc<LeafCertStore>, deps: ProxyDeps) -> Self {
        Self {
            config,
            leaf_store,
            deps,
            registry: Arc::new(build_default_wire_api_registry()),
        }
    }
}
