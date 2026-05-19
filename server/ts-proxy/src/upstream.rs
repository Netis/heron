//! Hyper-rustls client used to forward inner HTTPS requests upstream.
//!
//! Built once at proxy startup and held inside `ProxyState`, then
//! cloned cheaply (it's an `Arc` underneath) into every forward call.
//! Trust anchors come from `webpki-roots` by default; tests can swap in
//! a custom `RootCertStore` so they don't need a globally-trusted cert.

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::Request;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;

/// Type alias for the concrete hyper-util client we use upstream. Body
/// type is `Full<Bytes>` because the proxy always buffers the inner
/// request body before forwarding (request bodies for LLM calls are
/// small, typically a few KB at most).
pub type HyperClient = Client<
    hyper_rustls::HttpsConnector<HttpConnector>,
    Full<Bytes>,
>;

#[derive(Clone)]
pub struct UpstreamClient {
    inner: Arc<HyperClient>,
}

impl UpstreamClient {
    /// Production default: Mozilla CA bundle via `webpki-roots`.
    pub fn with_webpki_roots() -> Self {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots)
    }

    /// Build with a caller-supplied root store. Used in tests so a
    /// fake upstream signed by a temporary CA can be reached.
    pub fn with_roots(roots: RootCertStore) -> Self {
        // Ensure ring is installed. `install_default` errors if a
        // provider is already installed; ignore — first install wins
        // and we're a library, not the test harness.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(client_config)
            .https_or_http()
            .enable_http1()
            .build();
        let inner = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Forward one request upstream. The response body is streaming
    /// (`Incoming`) so SSE traffic doesn't get buffered end-to-end.
    pub async fn send(
        &self,
        req: Request<Full<Bytes>>,
    ) -> Result<hyper::Response<Incoming>, hyper_util::client::legacy::Error> {
        self.inner.request(req).await
    }
}

impl std::fmt::Debug for UpstreamClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamClient").finish()
    }
}
