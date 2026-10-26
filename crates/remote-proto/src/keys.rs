//! Tunnel identities: a persistent P-256 key per side, a self-signed
//! certificate around it, and the `sha256:<hex>` pin of its public key
//! (SubjectPublicKeyInfo) that the other side checks. Renewing the
//! certificate keeps the key, so pins never need to change.
use anyhow::{bail, ensure, Context, Result};
use aws_lc_rs::digest::{digest, SHA256};
use rcgen::PublicKeyData;
use std::io::Write;
use std::path::Path;

pub fn pin(spki_der: &[u8]) -> String {
    let mut s = String::from("sha256:");
    for b in digest(&SHA256, spki_der).as_ref() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Loads the key at `path`, or creates it (mode 0600, never overwriting).
pub fn persistent_key(path: &Path) -> Result<rcgen::KeyPair> {
    match std::fs::read_to_string(path) {
        Ok(pem) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path)?.permissions().mode();
                ensure!(
                    mode & 0o077 == 0,
                    "{} must not be readable by group or others",
                    path.display()
                );
            }
            let key = rcgen::KeyPair::from_pem(&pem)
                .with_context(|| format!("key {}", path.display()))?;
            ensure!(
                key.algorithm() == &rcgen::PKCS_ECDSA_P256_SHA256,
                "{} is not a P-256 key",
                path.display()
            );
            Ok(key)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options
                .open(path)
                .with_context(|| format!("create {}", path.display()))?;
            file.write_all(key.serialize_pem().as_bytes())?;
            file.sync_all()?;
            Ok(key)
        }
        Err(e) => bail!("read {}: {e}", path.display()),
    }
}

pub fn key_pin(key: &rcgen::KeyPair) -> String {
    pin(&key.subject_public_key_info())
}

/// `(certificate DER, PKCS#8 key DER)` of a self-signed certificate for
/// `key`, valid from an hour ago for `days`.
pub fn self_signed(
    key: &rcgen::KeyPair,
    names: &[String],
    days: i64,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut params = rcgen::CertificateParams::new(names.to_vec())?;
    let now = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    params.not_before = now;
    params.not_after = now + time::Duration::days(days);
    let cert = params.self_signed(key)?;
    Ok((cert.der().to_vec(), key.serialize_der()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_persists_with_private_mode_and_stable_pin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel.key");
        let a = persistent_key(&path).unwrap();
        let b = persistent_key(&path).unwrap();
        assert_eq!(key_pin(&a), key_pin(&b));
        assert_eq!(key_pin(&a).len(), 71);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(persistent_key(&path).is_err());
        }
    }

    #[test]
    fn certificate_carries_the_pinned_key() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let (cert, _) = self_signed(&key, &["veynode-tunnel".into()], 3650).unwrap();
        let spki = key.subject_public_key_info();
        assert!(cert.windows(spki.len()).any(|w| w == spki.as_slice()));
    }
}
