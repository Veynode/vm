//! The host's outbound QUIC tunnel to `vm-gateway`. The host presents a
//! certificate around its tunnel key (the gateway pins that key) and
//! accepts only a gateway whose certificate key matches the configured pin;
//! no CA is involved on either side.
use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::sync::Arc;
use std::time::Duration;
use vm_remote_proto::config::Host;
use vm_remote_proto::tunnel::ALPN;

/// The tunnel certificate is self-signed around a kept key; only the key is
/// pinned, so the certificate itself can live long.
const HOST_CERT_DAYS: i64 = 3650;
pub const DEFAULT_SERVER_NAME: &str = "veynode-tunnel";

#[derive(Debug)]
struct PinnedGateway {
    pin: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedGateway {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let parsed = rustls::server::ParsedCertificate::try_from(end_entity)?;
        let found = vm_remote_proto::keys::pin(parsed.subject_public_key_info().as_ref());
        if found == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "gateway key {found} does not match the configured pin"
            )))
        }
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    t.max_concurrent_uni_streams(256u32.into());
    t.max_concurrent_bidi_streams(64u32.into());
    t.datagram_receive_buffer_size(Some(1 << 20));
    t
}

pub fn client_config(host: &Host, key: &rcgen::KeyPair) -> Result<quinn::ClientConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let (cert, key_der) =
        vm_remote_proto::keys::self_signed(key, std::slice::from_ref(&host.name), HOST_CERT_DAYS)?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedGateway {
            pin: host.gateway_spki_sha256.clone(),
            provider,
        }))
        .with_client_auth_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
        )?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.enable_early_data = false;
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config()));
    Ok(config)
}

pub async fn connect(endpoint: &quinn::Endpoint, host: &Host) -> Result<quinn::Connection> {
    let addr = tokio::net::lookup_host(&host.connect)
        .await
        .with_context(|| format!("resolve {}", host.connect))?
        .next()
        .with_context(|| format!("{} has no address", host.connect))?;
    let name = host.server_name.as_deref().unwrap_or(DEFAULT_SERVER_NAME);
    Ok(endpoint.connect(addr, name)?.await?)
}
