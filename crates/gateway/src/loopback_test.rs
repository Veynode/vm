//! Gateway on loopback with a fake host (pinned QUIC tunnel) and a fake
//! browser (WebTransport client): routing of reliable messages, video
//! streams and datagrams, rejection of forged notices and unknown origins,
//! and route cleanup.
use crate::certs::{self, CertStore};
use crate::routes::{read_frame, Routes};
use crate::state::State;
use crate::{tunnel, wt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use vm_remote_proto::config::Config;
use vm_remote_proto::tunnel::{frame, routed, RouteOpen, ALPN};

const ORIGIN: &str = "https://desk.example.com";

#[derive(Debug)]
struct AnyServer(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AnyServer {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2".into()))
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

fn client_tls(alpn: &[u8], identity: Option<(&rcgen::KeyPair, &str)>) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServer(provider)));
    let mut tls = match identity {
        Some((key, name)) => {
            let (cert, der) = vm_remote_proto::keys::self_signed(key, &[name.into()], 1).unwrap();
            builder
                .with_client_auth_cert(
                    vec![CertificateDer::from(cert)],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der)),
                )
                .unwrap()
        }
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls
}

struct Rig {
    tunnel: SocketAddr,
    wt: SocketAddr,
    routes: Arc<Routes>,
    host_key: rcgen::KeyPair,
}

async fn rig() -> Rig {
    let host_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = Config::parse(&format!(
        r#"
[listen]
tcp = "127.0.0.1:0"
udp = "127.0.0.1:0"
allowed_origins = ["{ORIGIN}"]
[tls]
mode = "self-signed"
names = ["localhost"]
[tunnel]
listen = "127.0.0.1:0"
key = "{}"
hosts = [{{ id = "{}", name = "studio" }}]
"#,
        dir.path().join("tunnel.key").display(),
        vm_remote_proto::keys::key_pin(&host_key)
    ))
    .unwrap();
    let hosts = Arc::new(tunnel::Hosts::default());
    let routes = Arc::new(Routes::default());
    let t = config.tunnel.clone().unwrap();
    let tunnel_key = vm_remote_proto::keys::persistent_key(&t.key).unwrap();
    let tstore =
        CertStore::new(certs::with_key(&tunnel_key, &["veynode-tunnel".into()], 30).unwrap());
    let tep = tunnel::endpoint(t.listen, tstore).unwrap();
    let tunnel_addr = tep.local_addr().unwrap();
    tokio::spawn(tunnel::serve(tep, t.hosts, hosts.clone(), routes.clone()));
    let public = CertStore::new(certs::self_signed(&["localhost".into()]).unwrap());
    let state = State::new(config.clone(), Some(public.clone()), hosts, routes.clone());
    let wep = wt::endpoint(config.listen.unwrap().udp.unwrap(), public).unwrap();
    let wt_addr = wep.local_addr().unwrap();
    tokio::spawn(wt::serve(wep, state));
    std::mem::forget(dir);
    Rig {
        tunnel: tunnel_addr,
        wt: wt_addr,
        routes,
        host_key,
    }
}

async fn host(rig: &Rig, key: &rcgen::KeyPair) -> quinn::Connection {
    let tls = client_tls(ALPN, Some((key, "studio")));
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(tunnel::transport_config()));
    let mut ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    ep.set_default_client_config(config);
    ep.connect(rig.tunnel, "veynode-tunnel")
        .unwrap()
        .await
        .unwrap()
}

async fn browser(rig: &Rig, origin: &str) -> anyhow::Result<wtransport::Connection> {
    let tls = client_tls(wtransport::tls::WEBTRANSPORT_ALPN, None);
    let config = wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_custom_tls(tls)
        .build();
    let ep = wtransport::Endpoint::client(config)?;
    let request = wtransport::endpoint::ConnectOptions::builder(format!(
        "https://127.0.0.1:{}/v1/desktop/studio",
        rig.wt.port()
    ))
    .add_header("origin", origin)
    .build();
    Ok(ep.connect(request).await?)
}

async fn wait_for<F: Fn() -> bool>(f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn routes_reliable_video_and_datagrams_end_to_end() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let rig = rig().await;
    let host = host(&rig, &rig.host_key).await;

    let b = browser(&rig, ORIGIN).await.unwrap();
    let (mut bsend, mut brecv) = b.open_bi().await.unwrap().await.unwrap();
    bsend
        .write_all(&frame(b"\x07hello").unwrap())
        .await
        .unwrap();

    // Host side: the route opens with its metadata, then the message.
    let (mut hsend, mut hrecv) = host.accept_bi().await.unwrap();
    let open: RouteOpen =
        serde_json::from_slice(&read_frame(&mut hrecv).await.unwrap().unwrap()).unwrap();
    assert_eq!(
        (open.transport.as_str(), open.origin.as_str()),
        ("wt", ORIGIN)
    );
    assert_eq!(read_frame(&mut hrecv).await.unwrap().unwrap(), b"\x07hello");
    assert_eq!(rig.routes.count(), 1);

    // Reliable host message back.
    hsend
        .write_all(&frame(b"\x01ready").unwrap())
        .await
        .unwrap();
    assert_eq!(read_frame(&mut brecv).await.unwrap().unwrap(), b"\x01ready");

    // A forged gateway notice from the browser ends the session.
    // (Checked last; first video and datagrams.)
    let mut uni = host.open_uni().await.unwrap();
    uni.write_all(&open.route_id.to_be_bytes()).await.unwrap();
    uni.write_all(b"\x08frame-bytes").await.unwrap();
    uni.finish().unwrap();
    let mut video = b.accept_uni().await.unwrap();
    let mut got = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut video, &mut got)
        .await
        .unwrap();
    assert_eq!(got, b"\x08frame-bytes");

    host.send_datagram(routed(open.route_id, b"\x09ptr").into())
        .unwrap();
    let d = tokio::time::timeout(Duration::from_secs(2), b.receive_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&d[..], b"\x09ptr");

    b.send_datagram(b"\x09move").unwrap();
    let up = tokio::time::timeout(Duration::from_secs(2), host.read_datagram())
        .await
        .unwrap()
        .unwrap();
    let (rid, msg) = vm_remote_proto::tunnel::unroute(&up).unwrap();
    assert_eq!((rid, msg), (open.route_id, &b"\x09move"[..]));

    bsend
        .write_all(&frame(b"\xffforged").unwrap())
        .await
        .unwrap();
    wait_for(|| rig.routes.count() == 0).await;
}

#[tokio::test]
async fn unknown_origin_and_unknown_host_are_refused() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let rig = rig().await;
    let _host = host(&rig, &rig.host_key).await;
    assert!(browser(&rig, "https://evil.example").await.is_err());

    // A tunnel client whose key is not configured is closed.
    let stranger = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let conn = host(&rig, &stranger).await;
    let reason = tokio::time::timeout(Duration::from_secs(2), conn.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason, quinn::ConnectionError::ApplicationClosed(_)),
        "{reason:?}"
    );
    assert_eq!(rig.routes.count(), 0);
}
