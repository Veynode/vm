//! The host end to end over loopback QUIC: the test plays the gateway (a
//! pinned tunnel endpoint) and the browser (handshake, secure records), the
//! encoder is a [`FakeSource`]. Covers the signed handshake and key
//! agreement, video on per-unit streams, input, view-only refusal,
//! transport/origin binding and revocation.
use super::hub::tests::{hevc_key, settle, FakeSource};
use super::hub::{EncodedFrame, SourceCommand, SourceEvent};
use super::*;
use p256::ecdsa::{signature::Signer, SigningKey};
use serde_json::{json, Value};
use vm_remote_proto::record::{self, VideoHeader};
use vm_remote_proto::secure::{Direction, Ephemeral, Opener, Sealer, SessionKeys};
use vm_remote_proto::tunnel::{frame, ALPN};
use vm_session_auth::remote::*;
use vm_session_auth::*;

const ORIGIN: &str = "https://desk.example.com";

struct TestKey(SigningKey);
impl TestKey {
    fn new(n: u8) -> Self {
        let mut b = [0; 32];
        b[31] = n;
        Self(SigningKey::from_bytes((&b).into()).unwrap())
    }
}
impl Identity for TestKey {
    fn public_key(&self) -> Result<String> {
        Ok(hex(self
            .0
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()))
    }
    fn sign(&self, b: &[u8]) -> Result<String> {
        let s: p256::ecdsa::Signature = self.0.sign(b);
        Ok(hex(&s.to_bytes()))
    }
}

struct TestHost(TestKey);
impl HostIdentity for TestHost {
    fn answer(
        &self,
        gate: &SessionGate,
        accepted: AcceptedRemoteOffer,
        params: RemoteAnswerParams,
        now: u64,
    ) -> Result<SignedRemoteAnswer> {
        gate.remote_answer(accepted, params, &self.0, now)
    }
}

fn grant(h: &TestKey, d: &TestKey, scopes: &[&str]) -> SignedGrant {
    SignedGrant::new(
        Grant {
            host_id: identity_id(&h.public_key().unwrap()).unwrap(),
            device_id: identity_id(&d.public_key().unwrap()).unwrap(),
            device_public_key: d.public_key().unwrap(),
            vm_id: "linux".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            version: 1,
        },
        h,
    )
    .unwrap()
}

struct Rig {
    _dir: tempfile::TempDir,
    remote: Arc<Remote>,
    source: FakeSource,
    gateway: quinn::Connection,
    host_public: String,
    host_key: TestKey,
    devices: Vec<TestKey>,
}

/// Devices 2 (input + observe) and 3 (observe only) are paired.
async fn rig() -> Rig {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let h = TestKey::new(1);
    let devices = vec![TestKey::new(2), TestKey::new(3)];
    let host_public = h.public_key().unwrap();
    let grants = vec![
        grant(&h, &devices[0], &["input", "observe"]),
        grant(&h, &devices[1], &["observe"]),
    ];
    let gate = SessionGate::new(&host_public, &grants).unwrap();

    // The "gateway": a QUIC server whose certificate key the host pins.
    let gw_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (cert, der) =
        vm_remote_proto::keys::self_signed(&gw_key, &["veynode-tunnel".into()], 30).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.into()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(der.into()),
        )
        .unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let server = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(crypto)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let addr = server.local_addr().unwrap();

    let config = vm_remote_proto::config::Config::parse(&format!(
        r#"
[host]
connect = "{addr}"
gateway_spki_sha256 = "{}"
key = "{}"
name = "studio"
"#,
        vm_remote_proto::keys::key_pin(&gw_key),
        dir.path().join("tunnel.key").display()
    ))
    .unwrap();
    let source = FakeSource::default();
    let remote = Remote::new(
        gate,
        Arc::new(TestHost(TestKey::new(1))),
        config.clone(),
        Arc::new(source.clone()),
    );
    let host = config.host.unwrap();
    let tunnel_key = vm_remote_proto::keys::persistent_key(&host.key).unwrap();
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(tunnel::client_config(&host, &tunnel_key).unwrap());
    let accepting = tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() });
    let conn = tunnel::connect(&client, &host).await.unwrap();
    let gateway = accepting.await.unwrap();
    tokio::spawn(serve_connection(conn, remote.clone()));
    Rig {
        _dir: dir,
        remote,
        source,
        gateway,
        host_public,
        host_key: h,
        devices,
    }
}

/// The browser's half of one session.
struct Browser {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    sealer: Sealer,
    opener: Opener,
    route_id: u64,
    session_id: String,
}

async fn unit(recv: &mut quinn::RecvStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), session::read_frame(recv))
        .await
        .expect("host reply")
        .unwrap()
        .expect("route open")
}

async fn handshake_message(recv: &mut quinn::RecvStream) -> Value {
    let u = unit(recv).await;
    assert_eq!(u[0], record::AUTH);
    serde_json::from_slice(&u[1..]).unwrap()
}

async fn open_route(
    rig: &Rig,
    route_id: u64,
    transport: &str,
    origin: &str,
) -> (quinn::SendStream, quinn::RecvStream) {
    let (mut send, recv) = rig.gateway.open_bi().await.unwrap();
    let open = json!({"route_id":route_id,"transport":transport,"origin":origin,"remote":"198.51.100.7:5000"});
    send.write_all(&frame(&serde_json::to_vec(&open).unwrap()).unwrap())
        .await
        .unwrap();
    (send, recv)
}

async fn send_auth(send: &mut quinn::SendStream, value: Value) {
    let mut u = vec![record::AUTH];
    u.extend(serde_json::to_vec(&value).unwrap());
    send.write_all(&frame(&u).unwrap()).await.unwrap();
}

/// Runs the handshake as device `d`; the route says `transport`, the
/// browser signs `signed_transport`.
async fn browser(
    rig: &Rig,
    route_id: u64,
    d: &TestKey,
    scopes: &[&str],
    transport: &str,
    signed_transport: &str,
) -> std::result::Result<Browser, Value> {
    let (mut send, mut recv) = open_route(rig, route_id, transport, ORIGIN).await;
    let device_id = identity_id(&d.public_key().unwrap()).unwrap();
    send_auth(
        &mut send,
        json!({"t":"hello","proto":1,"device_id":device_id,"vm_id":"linux","scopes":scopes}),
    )
    .await;
    let m = handshake_message(&mut recv).await;
    if m["t"] == "error" {
        return Err(m);
    }
    let challenge: Challenge = serde_json::from_value(m["challenge"].clone()).unwrap();
    let ephemeral = Ephemeral::generate().unwrap();
    let intent = RemoteOfferIntent {
        challenge,
        request_id: format!("r{route_id}"),
        codecs: vec!["h264".into(), "h265".into()],
        browser_ecdh: hex(&ephemeral.public_key().unwrap()),
        transport: signed_transport.into(),
        origin: ORIGIN.into(),
    };
    let signature = d.sign(&intent.canonical().unwrap()).unwrap();
    send_auth(
        &mut send,
        json!({"t":"offer","offer":SignedRemoteOffer{intent: intent.clone(), signature}}),
    )
    .await;
    let m = handshake_message(&mut recv).await;
    if m["t"] == "error" {
        return Err(m);
    }
    let answer: SignedRemoteAnswer = serde_json::from_value(m["answer"].clone()).unwrap();
    answer.verify(&rig.host_public, &intent).unwrap();
    assert_eq!(answer.transcript.codec, "h265");
    let digest = answer.transcript.digest().unwrap();
    let salt: [u8; 32] = unhex(&digest, 32).unwrap().try_into().unwrap();
    let keys: SessionKeys = ephemeral
        .derive(&unhex(&answer.transcript.host_ecdh, 65).unwrap(), &salt)
        .unwrap();
    let confirmation = Confirmation {
        signature: d
            .sign(&remote_confirmation_bytes(&digest).unwrap())
            .unwrap(),
        transcript_sha256: digest,
    };
    send_auth(
        &mut send,
        json!({"t":"confirm","session_id":answer.transcript.session_id,"confirmation":confirmation}),
    )
    .await;
    Ok(Browser {
        send,
        recv,
        sealer: Sealer::new(&keys, Direction::BrowserToHost).unwrap(),
        opener: Opener::new(&keys, Direction::HostToBrowser).unwrap(),
        route_id,
        session_id: answer.transcript.session_id,
    })
}

impl Browser {
    /// The next host event whose `event` is `name`.
    async fn event(&mut self, name: &str) -> Value {
        loop {
            let sealed = unit(&mut self.recv).await;
            let (kind, _, plain) = self.opener.open(&sealed).unwrap();
            if kind != record::EVENT {
                continue;
            }
            let v: Value = serde_json::from_slice(&plain).unwrap();
            if v["event"] == name {
                return v;
            }
        }
    }

    async fn send(&mut self, kind: u8, value: Value) {
        let sealed = self
            .sealer
            .seal(kind, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        self.send.write_all(&frame(&sealed).unwrap()).await.unwrap();
    }
}

fn inputs(source: &FakeSource) -> Vec<Value> {
    source
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter_map(|c| match c {
            SourceCommand::Input(v) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn session_streams_video_takes_input_and_ends_on_revocation() {
    let rig = rig().await;
    let mut b = browser(&rig, 7, &rig.devices[0], &["input", "observe"], "wt", "wt")
        .await
        .unwrap();
    let s = b.event("session").await;
    assert_eq!(
        (s["vm"].as_str(), s["input"].as_bool()),
        (Some("linux"), Some(true))
    );
    assert_eq!(
        *rig.source.started.lock().unwrap(),
        vec![("linux".into(), "hevc".into())]
    );
    rig.source.ready("apple-virtualization").await;
    assert_eq!(b.event("ready").await["backend"], "apple-virtualization");

    // A keyframe reaches the browser on its own tunnel stream.
    rig.source
        .push(SourceEvent::Frame(EncodedFrame {
            track: 0,
            pts_us: 1_000,
            keyframe: true,
            annex_b: hevc_key(4),
        }))
        .await;
    let mut uni = tokio::time::timeout(Duration::from_secs(5), rig.gateway.accept_uni())
        .await
        .unwrap()
        .unwrap();
    let mut bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut uni, &mut bytes)
        .await
        .unwrap();
    assert_eq!(
        u64::from_be_bytes(bytes[..8].try_into().unwrap()),
        b.route_id
    );
    let (kind, _, plain) = b.opener.open(&bytes[8..]).unwrap();
    assert_eq!(kind, record::VIDEO_V2);
    let (header, au) = VideoHeader::decode(&plain).unwrap();
    assert!(header.keyframe && header.sps_changed);
    assert_eq!(
        (header.view.src_w, header.view.w, header.pts_us),
        (1920, 1920, 1_000)
    );
    assert_eq!(au, &hevc_key(4)[..]);

    // Input goes to the encoder relay; a replayed seq does not.
    let key = json!([{"t":"key","code":"KeyA","down":true}]);
    b.send(record::INPUT, json!({"seq":1,"events":key})).await;
    b.send(record::INPUT, json!({"seq":1,"events":key})).await;
    assert!(b.event("input_error").await["message"]
        .as_str()
        .unwrap()
        .contains("seq"));
    assert_eq!(inputs(&rig.source), vec![key.clone()]);

    // Ping is answered with the host clock.
    b.send(record::PING, json!({"id":3,"t_send_us":10})).await;
    loop {
        let sealed = unit(&mut b.recv).await;
        let (kind, _, plain) = b.opener.open(&sealed).unwrap();
        if kind == record::PING {
            let v: Value = serde_json::from_slice(&plain).unwrap();
            assert_eq!(
                (v["id"].as_u64(), v["t_send_us"].as_u64()),
                (Some(3), Some(10))
            );
            assert!(v["t_peer_us"].as_u64().unwrap() > 0);
            break;
        }
    }

    // Revoking the device's grant ends the session with 4003 and releases
    // the key it still held.
    let revoked = rig
        .remote
        .gate
        .replace_grants(
            &rig.host_public,
            &[grant(&rig.host_key, &rig.devices[1], &["observe"])],
        )
        .unwrap();
    assert_eq!(revoked, vec![b.session_id.clone()]);
    for id in revoked {
        rig.remote.close(&id, record::close::REVOKED);
    }
    assert_eq!(b.event("close").await["code"], record::close::REVOKED);
    settle().await;
    assert_eq!(
        inputs(&rig.source).last().unwrap(),
        &json!([{"t":"key","code":"KeyA","down":false}])
    );
}

#[tokio::test]
async fn view_only_sessions_cannot_send_input() {
    let rig = rig().await;
    let mut b = browser(&rig, 8, &rig.devices[1], &["observe"], "wss", "wss")
        .await
        .unwrap();
    assert_eq!(b.event("session").await["input"], false);
    b.send(
        record::INPUT,
        json!({"seq":1,"events":[{"t":"key","code":"KeyA","down":true}]}),
    )
    .await;
    assert!(b.event("input_error").await["message"]
        .as_str()
        .unwrap()
        .contains("view-only"));
    assert!(inputs(&rig.source).is_empty());
}

#[tokio::test]
async fn transport_the_browser_did_not_sign_is_refused() {
    let rig = rig().await;
    // The gateway says wss; the browser signed wt (a downgrade or splice).
    let refused = browser(&rig, 9, &rig.devices[0], &["input", "observe"], "wss", "wt")
        .await
        .err()
        .expect("refused");
    assert_eq!(refused["code"], record::close::UNAUTHORIZED);
    assert!(
        rig.source.started.lock().unwrap().is_empty(),
        "no encoder for a refused session"
    );
    // Scopes beyond the grant are refused at the challenge.
    let refused = browser(&rig, 10, &rig.devices[1], &["input", "observe"], "wt", "wt")
        .await
        .err()
        .expect("refused");
    assert_eq!(refused["t"], "error");
}
