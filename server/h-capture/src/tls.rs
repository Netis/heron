//! mTLS setup for the probe↔central transport.
//!
//! The thin probe is the TLS **client** (it dials out — NAT/firewall friendly);
//! the central is the TLS **server** and demands a client certificate (mutual
//! TLS), so only a probe holding a key signed by the configured CA can connect.
//! That mutual-auth handshake — not the best-effort edge redaction — is the real
//! security boundary for shipping post-TLS plaintext over the network.
//!
//! Both sides build their `rustls` config here with an explicit **ring** crypto
//! provider, so config construction never depends on whichever process-default
//! provider some other dependency (e.g. the ClickHouse client) may or may not
//! have installed.

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};

use crate::{CaptureError, Result};

fn tls_err(msg: impl std::fmt::Display) -> CaptureError {
    CaptureError::Tls(msg.to_string())
}

/// Load a PEM certificate chain from `path`.
fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut rd = BufReader::new(File::open(path).map_err(|e| tls_err(format!("{path}: {e}")))?);
    let certs: std::result::Result<Vec<_>, _> = rustls_pemfile::certs(&mut rd).collect();
    let certs = certs.map_err(|e| tls_err(format!("{path}: {e}")))?;
    if certs.is_empty() {
        return Err(tls_err(format!("{path}: no certificates found")));
    }
    Ok(certs)
}

/// Load the first PEM private key (PKCS#8 / PKCS#1 / SEC1) from `path`.
fn load_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let mut rd = BufReader::new(File::open(path).map_err(|e| tls_err(format!("{path}: {e}")))?);
    rustls_pemfile::private_key(&mut rd)
        .map_err(|e| tls_err(format!("{path}: {e}")))?
        .ok_or_else(|| tls_err(format!("{path}: no private key found")))
}

/// Build a [`RootCertStore`] trusting every cert in the PEM at `path`.
fn root_store(path: &str) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for cert in load_certs(path)? {
        roots
            .add(cert)
            .map_err(|e| tls_err(format!("{path}: {e}")))?;
    }
    Ok(roots)
}

/// Build the central's mTLS **server** config: present `cert`/`key`, and require
/// + verify a client certificate chaining to `client_ca`.
pub fn server_config(cert: &str, key: &str, client_ca: &str) -> Result<ServerConfig> {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let roots = Arc::new(root_store(client_ca)?);
    let verifier = WebPkiClientVerifier::builder_with_provider(roots, provider.clone())
        .build()
        .map_err(|e| tls_err(format!("client verifier: {e}")))?;
    ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err(format!("protocol versions: {e}")))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(load_certs(cert)?, load_key(key)?)
        .map_err(|e| tls_err(format!("server cert/key: {e}")))
}

/// Build the probe's mTLS **client** config: present `cert`/`key`, and verify the
/// central's server cert chains to `server_ca`.
pub fn client_config(cert: &str, key: &str, server_ca: &str) -> Result<ClientConfig> {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err(format!("protocol versions: {e}")))?
        .with_root_certificates(root_store(server_ca)?)
        .with_client_auth_cert(load_certs(cert)?, load_key(key)?)
        .map_err(|e| tls_err(format!("client cert/key: {e}")))
}

/// Extract the subject Common Name from a DER-encoded certificate, used as the
/// probe's `source_id` when a batch arrives without an explicit one. Returns
/// `None` if the cert has no parseable CN.
pub fn peer_common_name(cert_der: &[u8]) -> Option<String> {
    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(cert_der).ok()?;
    // Bind the owned String to a local so `cert`'s borrow ends before return
    // (the iterator chain otherwise holds the borrow past the block).
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(|s| s.to_string());
    cn
}
