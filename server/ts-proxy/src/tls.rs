//! Per-host leaf cert minting, signed by the TokenScope MITM CA.
//!
//! When a client opens a TLS connection through the proxy via CONNECT,
//! the server side needs a cert that matches the SNI hostname the
//! client is dialing. We can't pre-generate those (millions of hosts);
//! instead we mint them on demand and cache by SNI.

use crate::ca::{CaError, CaMaterial};
use lru::LruCache;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, KeyUsagePurpose, SanType};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::sign::CertifiedKey;

const LEAF_CACHE_CAPACITY: usize = 1024;

#[derive(Debug, Error)]
pub enum LeafCertError {
    #[error("rcgen error: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("CA error: {0}")]
    Ca(#[from] CaError),
    #[error("rustls signer error: {0}")]
    Signer(String),
    #[error("invalid SNI hostname: {0}")]
    InvalidSni(String),
}

/// Mints per-SNI leaf certs lazily, sharing the underlying CA. The
/// returned `CertifiedKey` is the type rustls' `ServerConfig::with_cert_resolver`
/// hands back to drive the handshake.
pub struct LeafCertStore {
    ca: CaMaterial,
    cache: Mutex<LruCache<String, Arc<CertifiedKey>>>,
}

impl LeafCertStore {
    pub fn new(ca: CaMaterial) -> Self {
        Self {
            ca,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(LEAF_CACHE_CAPACITY).expect("non-zero"),
            )),
        }
    }

    /// Look up or mint a cert for `sni`. Idempotent — multiple callers
    /// asking for the same hostname share one underlying cert.
    pub fn get(&self, sni: &str) -> Result<Arc<CertifiedKey>, LeafCertError> {
        validate_sni(sni)?;
        if let Some(ck) = self.cache.lock().expect("poisoned").get(sni).cloned() {
            return Ok(ck);
        }
        let minted = self.mint(sni)?;
        let arc = Arc::new(minted);
        self.cache
            .lock()
            .expect("poisoned")
            .put(sni.to_string(), arc.clone());
        Ok(arc)
    }

    /// Expose the CA PEM — used by the print-ca CLI and the
    /// `GET /api/proxy/ca.pem` endpoint.
    pub fn ca_pem(&self) -> &str {
        &self.ca.cert_pem
    }

    fn mint(&self, sni: &str) -> Result<CertifiedKey, LeafCertError> {
        let leaf_key = KeyPair::generate()?;
        let params = build_leaf_params(sni)?;
        let issuer = self.ca.issuer_certificate()?;
        let leaf = params.signed_by(&leaf_key, &issuer, &self.ca.key_pair)?;

        let leaf_der = CertificateDer::from(leaf.der().to_vec());
        let key_der = PrivatePkcs8KeyDer::from(leaf_key.serialize_der());

        let signing_key = tokio_rustls::rustls::crypto::ring::sign::any_supported_type(
            &PrivateKeyDer::Pkcs8(key_der),
        )
        .map_err(|e| LeafCertError::Signer(e.to_string()))?;

        // Single-element chain: leaf only. The client already trusts our
        // CA via its system trust store (or REQUESTS_CA_BUNDLE etc.) —
        // sending a regenerated copy of the root in-chain would risk a
        // byte mismatch against the installed one on strict validators,
        // and is unnecessary either way (RFC 5280 §6 trust comes from
        // the truststore, not the wire).
        Ok(CertifiedKey::new(vec![leaf_der], signing_key))
    }
}

fn build_leaf_params(sni: &str) -> Result<CertificateParams, rcgen::Error> {
    let mut params = CertificateParams::new(vec![sni.to_string()])?;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.subject_alt_names = vec![SanType::DnsName(sni.try_into().map_err(|_| {
        rcgen::Error::CouldNotParseCertificate
    })?)];

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, sni);
    params.distinguished_name = dn;

    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    // 1-year validity — we don't ever serve these externally, and a
    // long-running proxy process can comfortably keep the cache hot.
    params.not_after = now + time::Duration::days(365);

    Ok(params)
}

fn validate_sni(sni: &str) -> Result<(), LeafCertError> {
    // rustls / browsers reject hostnames with embedded NULs or control
    // chars. Cheap check up front so we don't waste cycles minting a
    // bad cert.
    if sni.is_empty()
        || sni.len() > 253
        || sni
            .chars()
            .any(|c| c.is_control() || c == ' ' || c == '/')
    {
        return Err(LeafCertError::InvalidSni(sni.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ca::load_or_generate_ca;
    use tempfile::TempDir;

    fn fresh_store() -> LeafCertStore {
        let dir = TempDir::new().unwrap();
        let ca = load_or_generate_ca(dir.path()).expect("ca");
        // Tempdir drops after this fn returns, but the CA material is
        // already loaded into memory — no file access happens past this
        // point, so it's fine.
        std::mem::forget(dir);
        LeafCertStore::new(ca)
    }

    #[test]
    fn mints_a_leaf_for_a_hostname() {
        let store = fresh_store();
        let ck = store.get("api.openai.com").expect("mint");
        assert!(!ck.cert.is_empty());
        // Single-element chain: leaf only (root comes from truststore).
        assert_eq!(ck.cert.len(), 1);
    }

    #[test]
    fn cache_returns_same_arc_on_second_call() {
        let store = fresh_store();
        let ck1 = store.get("api.openai.com").expect("mint");
        let ck2 = store.get("api.openai.com").expect("hit cache");
        assert!(Arc::ptr_eq(&ck1, &ck2));
    }

    #[test]
    fn distinct_hostnames_get_distinct_certs() {
        let store = fresh_store();
        let a = store.get("api.openai.com").expect("a");
        let b = store.get("api.anthropic.com").expect("b");
        assert!(!Arc::ptr_eq(&a, &b));
        assert_ne!(a.cert[0].as_ref(), b.cert[0].as_ref());
    }

    #[test]
    fn ca_pem_round_trips() {
        let store = fresh_store();
        let pem = store.ca_pem();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn rejects_empty_sni() {
        let store = fresh_store();
        let err = store.get("").unwrap_err();
        assert!(matches!(err, LeafCertError::InvalidSni(_)));
    }

    #[test]
    fn rejects_sni_with_space() {
        let store = fresh_store();
        let err = store.get("api openai com").unwrap_err();
        assert!(matches!(err, LeafCertError::InvalidSni(_)));
    }

    #[test]
    fn rejects_overlong_sni() {
        let store = fresh_store();
        let long = "a".repeat(300);
        let err = store.get(&long).unwrap_err();
        assert!(matches!(err, LeafCertError::InvalidSni(_)));
    }
}
