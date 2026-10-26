//! Host tunnel listener: Mac hosts dial in over QUIC with a client
//! certificate whose key the gateway pins (`[tunnel] hosts`). One live
//! connection per host; a reconnect replaces the old one.
use crate::certs::{pin, spki_sha256, CertStore};
use crate::routes::{Downlink, Routes};
use anyhow::{anyhow, Context, Result};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vm_remote_proto::config::HostEntry;
use vm_remote_proto::tunnel::{self, ALPN};

/// Close codes on the tunnel connection.
pub const CLOSE_UNKNOWN_HOST: u32 = 4001;
pub const CLOSE_REPLACED: u32 = 4002;

/// Accepts any client certificate whose handshake signature verifies; the
/// key is then matched against the configured pins after the handshake.
#[derive(Debug)]
struct PinnedClients(Arc<rustls::crypto::CryptoProvider>);

impl ClientCertVerifier for PinnedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not allowed".into()))
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
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

const ROUTE_ID_TIMEOUT: Duration = Duration::from_millis(500);

pub fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    t.max_concurrent_uni_streams(256u32.into());
    t.max_concurrent_bidi_streams(64u32.into());
    t.datagram_receive_buffer_size(Some(1 << 20));
    t
}

pub fn endpoint(addr: SocketAddr, certs: Arc<CertStore>) -> Result<quinn::Endpoint> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(PinnedClients(provider)))
        .with_cert_resolver(certs);
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    server.transport_config(Arc::new(transport_config()));
    Ok(quinn::Endpoint::server(server, addr)?)
}

/// Live host connections by configured name.
#[derive(Default)]
pub struct Hosts {
    live: Mutex<HashMap<String, quinn::Connection>>,
}

impl Hosts {
    pub fn get(&self, name: &str) -> Option<quinn::Connection> {
        self.live.lock().unwrap().get(name).cloned()
    }
    /// The only live host, when exactly one is connected.
    pub fn only(&self) -> Option<(String, quinn::Connection)> {
        let live = self.live.lock().unwrap();
        (live.len() == 1).then(|| {
            let (n, c) = live.iter().next().unwrap();
            (n.clone(), c.clone())
        })
    }
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.live.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
    fn insert(&self, name: &str, conn: quinn::Connection) {
        if let Some(old) = self.live.lock().unwrap().insert(name.into(), conn) {
            old.close(CLOSE_REPLACED.into(), b"replaced");
        }
    }
    fn remove(&self, name: &str, conn: &quinn::Connection) {
        let mut live = self.live.lock().unwrap();
        if live
            .get(name)
            .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            live.remove(name);
        }
    }
}

fn host_name(conn: &quinn::Connection, hosts: &[HostEntry]) -> Result<String> {
    let identity = conn.peer_identity().context("no client certificate")?;
    let chain = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow!("unexpected peer identity"))?;
    let leaf = chain.first().context("empty client certificate chain")?;
    let id = pin(&spki_sha256(leaf.as_ref())?);
    hosts
        .iter()
        .find(|h| h.id == id)
        .map(|h| h.name.clone())
        .with_context(|| format!("unknown host key {id}"))
}

pub async fn serve(
    endpoint: quinn::Endpoint,
    allowed: Vec<HostEntry>,
    hosts: Arc<Hosts>,
    routes: Arc<Routes>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let (allowed, hosts, routes) = (allowed.clone(), hosts.clone(), routes.clone());
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => return eprintln!("vm-gateway: tunnel handshake: {e}"),
            };
            let name = match host_name(&conn, &allowed) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("vm-gateway: tunnel from {}: {e:#}", conn.remote_address());
                    conn.close(CLOSE_UNKNOWN_HOST.into(), b"unknown host");
                    return;
                }
            };
            eprintln!(
                "vm-gateway: host {name} connected from {}",
                conn.remote_address()
            );
            hosts.insert(&name, conn.clone());
            host_loop(&conn, &routes).await;
            hosts.remove(&name, &conn);
            routes.close_host(conn.stable_id());
            eprintln!("vm-gateway: host {name} disconnected");
        });
    }
}

/// Video streams and datagrams from a host, handed to their routes.
async fn host_loop(conn: &quinn::Connection, routes: &Arc<Routes>) {
    loop {
        tokio::select! {
            uni = conn.accept_uni() => {
                let Ok(mut recv) = uni else { return };
                let routes = routes.clone();
                // Read the route id off the loop so one slow stream never
                // holds back the others or the datagrams.
                tokio::spawn(async move {
                    let mut id = [0u8; 8];
                    let read = tokio::time::timeout(ROUTE_ID_TIMEOUT, recv.read_exact(&mut id)).await;
                    if !matches!(read, Ok(Ok(()))) {
                        let _ = recv.stop(1u32.into());
                        return;
                    }
                    let route_id = u64::from_be_bytes(id);
                    if let Err(Downlink::Video(mut recv)) = routes.downlink(route_id, Downlink::Video(recv)) {
                        // Unknown route or the browser is behind: the host sees
                        // the stream stopped and treats the frame as lost.
                        let _ = recv.stop(1u32.into());
                    }
                });
            }
            dgram = conn.read_datagram() => {
                let Ok(bytes) = dgram else { return };
                if let Ok((route_id, message)) = tunnel::unroute(&bytes) {
                    let _ = routes.downlink(route_id, Downlink::Datagram(bytes.slice(bytes.len() - message.len()..)));
                }
            }
        }
    }
}
