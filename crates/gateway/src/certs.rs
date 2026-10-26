//! The gateway's server certificate, shared by the HTTPS listener, the
//! WebTransport endpoint and the host tunnel, and swapped in place on
//! renewal (ACME), reload (files, SIGHUP) or rotation (self-signed).
use anyhow::{anyhow, Context, Result};
use rustls::crypto::aws_lc_rs::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// WebTransport `serverCertificateHashes` accepts at most 14 days.
pub const SELF_SIGNED_LIFETIME: Duration = Duration::from_secs(13 * 24 * 3600);
/// Rotate this long before a self-signed certificate ends.
pub const SELF_SIGNED_ROTATE_BEFORE: Duration = Duration::from_secs(2 * 24 * 3600);

#[derive(Debug)]
pub struct CertStore {
    current: RwLock<Arc<CertifiedKey>>,
    /// SHA-256 of the leaf DER, published for pinning when self-signed.
    leaf_sha256: RwLock<[u8; 32]>,
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().unwrap().clone())
    }
}

fn leaf_hash(key: &CertifiedKey) -> [u8; 32] {
    Sha256::digest(key.cert[0].as_ref()).into()
}

impl CertStore {
    pub fn new(key: CertifiedKey) -> Arc<Self> {
        let hash = leaf_hash(&key);
        Arc::new(Self {
            current: RwLock::new(Arc::new(key)),
            leaf_sha256: RwLock::new(hash),
        })
    }

    pub fn replace(&self, key: CertifiedKey) {
        *self.leaf_sha256.write().unwrap() = leaf_hash(&key);
        *self.current.write().unwrap() = Arc::new(key);
    }

    pub fn leaf_sha256(&self) -> [u8; 32] {
        *self.leaf_sha256.read().unwrap()
    }

    /// SHA-256 of the leaf's SubjectPublicKeyInfo.
    #[cfg(test)]
    pub fn spki_sha256(&self) -> Result<[u8; 32]> {
        spki_sha256(self.current.read().unwrap().cert[0].as_ref())
    }
}

pub fn spki_sha256(der: &[u8]) -> Result<[u8; 32]> {
    let (_, cert) =
        x509_parser::parse_x509_certificate(der).map_err(|e| anyhow!("certificate: {e}"))?;
    Ok(Sha256::digest(cert.tbs_certificate.subject_pki.raw).into())
}

pub fn pin(hash: &[u8; 32]) -> String {
    let mut s = String::from("sha256:");
    for b in hash {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn from_pem(chain_pem: &[u8], key_pem: &[u8]) -> Result<CertifiedKey> {
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &chain_pem[..])
        .collect::<Result<_, _>>()
        .context("certificate PEM")?;
    anyhow::ensure!(!chain.is_empty(), "no certificate in PEM");
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("private key PEM")?
        .context("no private key in PEM")?;
    let signer = any_supported_type(&key).map_err(|e| anyhow!("private key: {e}"))?;
    Ok(CertifiedKey::new(chain, signer))
}

pub fn from_files(cert: &Path, key: &Path) -> Result<CertifiedKey> {
    let meta = std::fs::metadata(key).with_context(|| format!("read {}", key.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "{} must not be readable by group or others",
            key.display()
        );
    }
    let _ = meta;
    from_pem(&std::fs::read(cert)?, &std::fs::read(key)?)
}

/// A fresh P-256 certificate with a new key, valid from an hour ago for
/// [`SELF_SIGNED_LIFETIME`], inside WebTransport's 14 day limit.
pub fn self_signed(names: &[String]) -> Result<CertifiedKey> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    with_key(
        &key,
        names,
        (SELF_SIGNED_LIFETIME.as_secs() / 86_400) as i64,
    )
}

/// A self-signed certificate around a kept key (the tunnel's).
pub fn with_key(key: &rcgen::KeyPair, names: &[String], days: i64) -> Result<CertifiedKey> {
    let (cert, key_der) = vm_remote_proto::keys::self_signed(key, names, days)?;
    let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
    let signer = any_supported_type(&der).map_err(|e| anyhow!("private key: {e}"))?;
    Ok(CertifiedKey::new(vec![CertificateDer::from(cert)], signer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_is_pinnable_and_replaceable() {
        let a = self_signed(&["localhost".into()]).unwrap();
        let store = CertStore::new(a);
        let (leaf, spki) = (store.leaf_sha256(), store.spki_sha256().unwrap());
        assert!(pin(&spki).starts_with("sha256:") && pin(&spki).len() == 71);
        store.replace(self_signed(&["localhost".into()]).unwrap());
        assert_ne!(store.leaf_sha256(), leaf);
        assert_ne!(store.spki_sha256().unwrap(), spki);
    }

    #[test]
    fn self_signed_validity_fits_webtransport_hash_limit() {
        let key = self_signed(&["localhost".into()]).unwrap();
        let (_, cert) = x509_parser::parse_x509_certificate(key.cert[0].as_ref()).unwrap();
        let v = cert.validity();
        let span = v.not_after.timestamp() - v.not_before.timestamp();
        assert!(span <= 14 * 24 * 3600, "{span}");
    }
}
