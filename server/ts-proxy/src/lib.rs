//! Built-in MITM forward proxy for HTTPS LLM capture.
//!
//! The proxy terminates TLS using on-the-fly per-host leaf certs signed
//! by a local root CA, classifies the inner HTTP request via the existing
//! `WireApiRegistry`, forwards it to the real upstream, tees the response
//! back to the client while accumulating a capture buffer, and emits one
//! `HttpJoinerEvent::Exchange` per LLM call into the existing pipeline.
//! Downstream (wire-api detect, LlmCall extract, AgentTurn assembly,
//! storage) is unchanged from the sniffed-capture path — proxy-originated
//! rows just carry `source_id = ProxyConfig::source_id` (default
//! `"builtin-proxy"`) so the UI source filter can distinguish them.
//!
//! See `docs/design/builtin-mitm-proxy.md` for the architecture diagram.

pub mod ca;
pub mod capture;
pub mod forward;
pub mod redact;
pub mod server;
pub mod state;
pub mod tls;
pub mod tunnel;
pub mod upstream;

pub use ca::{load_or_generate_ca, CaError, CaMaterial};
pub use redact::redact_headers;
pub use server::{spawn_proxy, ProxyServerError};
pub use state::{ProxyDeps, ProxyState, TunnelContext};
pub use tls::{LeafCertError, LeafCertStore, SniResolver, TunnelSniResolver};
pub use upstream::UpstreamClient;

/// Convenience re-exports so callers don't need to depend on ts-common
/// just to pass a `ProxyConfig` / `RedactPolicy`.
pub use ts_common::config::{ProxyConfig, RedactPolicy};
