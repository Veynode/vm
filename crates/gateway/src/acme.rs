//! Certificates from an ACME CA (Let's Encrypt by default) over HTTP-01.
//!
//! The account credentials, the certificate chain and its key live in
//! `state_dir` (keys mode 0600). A certificate is renewed 30 days before it
//! ends; the new one is swapped into the shared [`CertStore`], so HTTPS and
//! WebTransport pick it up on their next handshake without a restart.
use crate::certs::{self, CertStore};
use anyhow::{anyhow, bail, Context, Result};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 3600);
const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(3600);

pub struct Acme {
    pub domains: Vec<String>,
    pub email: String,
    pub dir: PathBuf,
    pub staging: bool,
    /// HTTP-01 answers by token, served at `/.well-known/acme-challenge/`.
    pub answers: Arc<Mutex<HashMap<String, String>>>,
}

impl Acme {
    fn cert_path(&self) -> PathBuf {
        self.dir.join("cert.pem")
    }
    fn key_path(&self) -> PathBuf {
        self.dir.join("key.pem")
    }

    /// The stored certificate, when there is one that is not yet due.
    pub fn stored(&self) -> Result<Option<rustls::sign::CertifiedKey>> {
        let (cert, key) = (self.cert_path(), self.key_path());
        if !cert.exists() || !key.exists() {
            return Ok(None);
        }
        let loaded = certs::from_files(&cert, &key)?;
        let left = remaining(loaded.cert[0].as_ref())?;
        Ok((left > RENEW_BEFORE).then_some(loaded))
    }

    /// Keeps `store` holding a current certificate, forever.
    pub async fn maintain(self, store: Arc<CertStore>) {
        loop {
            let wait = match self.stored() {
                Ok(Some(_)) => CHECK_EVERY,
                Ok(None) | Err(_) => match self.issue().await {
                    Ok(key) => {
                        eprintln!(
                            "vm-gateway: acme: certificate issued for {:?}",
                            self.domains
                        );
                        store.replace(key);
                        CHECK_EVERY
                    }
                    Err(e) => {
                        eprintln!("vm-gateway: acme: {e:#}");
                        RETRY_AFTER_FAILURE
                    }
                },
            };
            tokio::time::sleep(wait).await;
        }
    }

    async fn account(&self) -> Result<Account> {
        let path = self.dir.join("account.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            let credentials: AccountCredentials =
                serde_json::from_str(&text).context("acme account.json")?;
            return Ok(Account::builder()?.from_credentials(credentials).await?);
        }
        let contact = format!("mailto:{}", self.email);
        let directory = if self.staging {
            LetsEncrypt::Staging.url()
        } else {
            LetsEncrypt::Production.url()
        };
        let (account, credentials) = Account::builder()?
            .create(
                &NewAccount {
                    contact: &[&contact],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                directory.to_owned(),
                None,
            )
            .await?;
        write_private(&path, serde_json::to_string(&credentials)?.as_bytes())?;
        Ok(account)
    }

    async fn issue(&self) -> Result<rustls::sign::CertifiedKey> {
        std::fs::create_dir_all(&self.dir)?;
        let account = self.account().await?;
        let identifiers: Vec<Identifier> = self
            .domains
            .iter()
            .map(|d| Identifier::Dns(d.clone()))
            .collect();
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
        let mut tokens = Vec::new();
        {
            let mut authorizations = order.authorizations();
            while let Some(auth) = authorizations.next().await {
                let mut auth = auth?;
                match auth.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    s => bail!("authorization {s:?}"),
                }
                let mut challenge = auth
                    .challenge(ChallengeType::Http01)
                    .context("CA offered no http-01 challenge")?;
                let token = challenge.token.clone();
                self.answers
                    .lock()
                    .unwrap()
                    .insert(token.clone(), challenge.key_authorization().as_str().into());
                tokens.push(token);
                challenge.set_ready().await?;
            }
        }
        let result = self.finish(&mut order).await;
        let mut answers = self.answers.lock().unwrap();
        for t in tokens {
            answers.remove(&t);
        }
        result
    }

    async fn finish(&self, order: &mut instant_acme::Order) -> Result<rustls::sign::CertifiedKey> {
        let retries = RetryPolicy::new().timeout(Duration::from_secs(120));
        match order.poll_ready(&retries).await? {
            OrderStatus::Ready => {}
            s => bail!("order {s:?}"),
        }
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::new(self.domains.clone())?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let csr = params.serialize_request(&key)?;
        order.finalize_csr(csr.der()).await?;
        let chain = order.poll_certificate(&retries).await?;
        let key_pem = key.serialize_pem();
        let loaded = certs::from_pem(chain.as_bytes(), key_pem.as_bytes())?;
        write_private(&self.key_path(), key_pem.as_bytes())?;
        write_private(&self.cert_path(), chain.as_bytes())?;
        Ok(loaded)
    }
}

/// Time until the certificate ends (zero when it already has).
pub fn remaining(der: &[u8]) -> Result<Duration> {
    let (_, cert) =
        x509_parser::parse_x509_certificate(der).map_err(|e| anyhow!("certificate: {e}"))?;
    let end = cert.validity().not_after.timestamp();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    Ok(Duration::from_secs(end.saturating_sub(now).max(0) as u64))
}

/// Writes through a temporary file so a crash never leaves half a key.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_certificate_due_for_renewal_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let acme = Acme {
            domains: vec!["localhost".into()],
            email: "ops@example.com".into(),
            dir: dir.path().into(),
            staging: true,
            answers: Default::default(),
        };
        assert!(acme.stored().unwrap().is_none(), "nothing stored");
        // A 13 day certificate is inside the 30 day renewal window.
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let (cert, _) = vm_remote_proto::keys::self_signed(&key, &acme.domains, 13).unwrap();
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64_lines(&cert)
        );
        write_private(&acme.cert_path(), pem.as_bytes()).unwrap();
        write_private(&acme.key_path(), key.serialize_pem().as_bytes()).unwrap();
        assert!(acme.stored().unwrap().is_none());
        let (cert, _) = vm_remote_proto::keys::self_signed(&key, &acme.domains, 90).unwrap();
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64_lines(&cert)
        );
        write_private(&acme.cert_path(), pem.as_bytes()).unwrap();
        assert!(acme.stored().unwrap().is_some());
    }

    fn base64_lines(der: &[u8]) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(der);
        b64.as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
