//! Cross-layer local transport security tests: real ICE/DTLS/SCTP peers and
//! production auth/run_session/ConsoleControlDispatch/SessionInput paths.
//! In-memory P-256 identities, controller Unix replies and encoder marker process
//! are fixtures. No VM, actual encoding, Keychain or Internet acceptance claim.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use bytes::BytesMut;
use p256::ecdsa::{signature::Signer, SigningKey};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use std::os::unix::fs::PermissionsExt;
use tokio::io::AsyncBufReadExt;
use tower::ServiceExt;
use webrtc::data_channel::{DataChannelEvent, RTCDataChannelInit};

struct MemoryKey(SigningKey);
impl MemoryKey {
    fn new(value: u8) -> Self {
        let mut bytes = [0; 32];
        bytes[31] = value;
        Self(SigningKey::from_bytes((&bytes).into()).unwrap())
    }
}
impl Identity for MemoryKey {
    fn public_key(&self) -> Result<String> {
        Ok(protocol::hex(
            self.0.verifying_key().to_encoded_point(false).as_bytes(),
        ))
    }
    fn sign(&self, bytes: &[u8]) -> Result<String> {
        let sig: p256::ecdsa::Signature = self.0.sign(bytes);
        Ok(protocol::hex(&sig.to_bytes()))
    }
}
struct Events(watch::Sender<bool>);
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.0.send_replace(true);
        }
    }
}
struct Client {
    peer: Arc<dyn PeerConnection>,
    channel: Arc<dyn DataChannel>,
    sdp: String,
}
impl Client {
    async fn new() -> Self {
        Self::with_ice(None).await
    }
    async fn with_ice(config: Option<&protocol::ice::IceConfig>) -> Self {
        let (gathered, ready) = watch::channel(false);
        let mut engine = MediaEngine::default();
        engine
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: RTCRtpCodec {
                        mime_type: "video/H264".into(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line:
                            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                                .into(),
                        rtcp_feedback: vec![],
                    },
                    payload_type: 102,
                },
                RtpCodecKind::Video,
            )
            .unwrap();
        let peer: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(
                    crate::ice_config::configuration(config, now().unwrap()).unwrap(),
                )
                .with_media_engine(engine)
                .with_setting_engine(crate::control_channel::bounded_settings())
                .with_data_channel_send_buffer_limit(crate::control_channel::SEND_BUFFER_BYTES)
                .with_handler(Arc::new(Events(gathered)))
                .with_runtime(webrtc::runtime::default_runtime().unwrap())
                .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
                .build()
                .await
                .unwrap(),
        );
        peer.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let channel = peer
            .create_data_channel(
                crate::control_channel::LABEL,
                Some(RTCDataChannelInit {
                    protocol: crate::control_channel::PROTOCOL.into(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        peer.set_local_description(peer.create_offer(None).await.unwrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(4), crate::flag(ready))
            .await
            .unwrap();
        let sdp = peer.local_description().await.unwrap().sdp;
        Self { peer, channel, sdp }
    }
    async fn accept(&self, sdp: &str) {
        self.accept_for(sdp, Duration::from_secs(5)).await
    }
    async fn accept_for(&self, sdp: &str, wait: Duration) {
        self.peer
            .set_remote_description(
                serde_json::from_value(json!({"type":"answer","sdp":sdp})).unwrap(),
            )
            .await
            .unwrap();
        tokio::time::timeout(wait, async {
            loop {
                match self.channel.poll().await {
                    Some(DataChannelEvent::OnOpen) => break,
                    Some(_) => {}
                    None => panic!("channel ended before open"),
                }
            }
        })
        .await
        .unwrap();
    }
    async fn send(&self, request: &protocol::control::SignedControl) {
        let body = serde_json::to_vec(request).unwrap();
        assert!(body.len() < 16 * 1024 - 17);
        let mut bytes = BytesMut::new();
        bytes.extend_from_slice(&[1]);
        bytes.extend_from_slice(&request.request.sequence.to_be_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&body);
        self.channel.send(bytes).await.unwrap();
    }
    async fn response(&self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match self.channel.poll().await {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        assert!(!message.is_string);
                        assert!(message.data.len() > 17);
                        assert_eq!(
                            u32::from_be_bytes(message.data[13..17].try_into().unwrap()),
                            0
                        );
                        assert_eq!(
                            u32::from_be_bytes(message.data[9..13].try_into().unwrap()) as usize,
                            message.data.len() - 17
                        );
                        break serde_json::from_slice(&message.data[17..]).unwrap();
                    }
                    Some(
                        DataChannelEvent::OnBufferedAmountLow
                        | DataChannelEvent::OnBufferedAmountHigh,
                    ) => {}
                    _ => panic!("channel ended before response"),
                }
            }
        })
        .await
        .unwrap()
    }
    async fn close(self) {
        tokio::time::timeout(Duration::from_secs(2), self.peer.close())
            .await
            .unwrap()
            .unwrap();
    }
}
struct Harness {
    _temp: tempfile::TempDir,
    _console: tokio::net::UnixListener,
    controller: tokio::task::JoinHandle<()>,
    app: App,
    host: Arc<MemoryKey>,
    device: MemoryKey,
    grant: protocol::SignedGrant,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
    marker: PathBuf,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.controller.abort();
    }
}
impl Harness {
    fn new() -> Self {
        Self::with_ice(None)
    }
    fn with_ice(ice_path: Option<PathBuf>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let console = tokio::net::UnixListener::bind(dir.join("console.sock")).unwrap();
        let listener = tokio::net::UnixListener::bind(dir.join("control.sock")).unwrap();
        for p in [dir.join("console.sock"), dir.join("control.sock")] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let encoder = dir.join("marker-encoder");
        // Alarm bounds the standalone fixture even if a failing test misses cleanup.
        std::fs::write(&encoder,b"#!/usr/bin/env python3\nimport os,signal,sys\nsignal.alarm(20)\nwith open(sys.argv[1]+'.started','w') as f: f.write(str(os.getpid()))\nfor line in sys.stdin:\n if line.strip()=='stop':\n  with open(sys.argv[1]+'.stopped','w') as f: f.write('stop received')\n  break\n").unwrap();
        std::fs::set_permissions(&encoder, std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = Arc::new(MemoryKey::new(1));
        let device = MemoryKey::new(2);
        let public = host.public_key().unwrap();
        let grant = protocol::SignedGrant::new(
            protocol::Grant {
                host_id: protocol::identity_id(&public).unwrap(),
                device_id: protocol::identity_id(&device.public_key().unwrap()).unwrap(),
                device_public_key: device.public_key().unwrap(),
                vm_id: "hostile-peer".into(),
                scopes: vec!["input".into(), "observe".into()],
                version: 1,
            },
            host.as_ref(),
        )
        .unwrap();
        let state = dir.join("registry");
        protocol::store::Store::open(&state)
            .unwrap()
            .save(&protocol::store::Registry {
                host_public_key: public.clone(),
                grants: BTreeMap::from([(grant.grant.key(), grant.clone())]),
                versions: BTreeMap::from([(grant.grant.key(), 1)]),
            })
            .unwrap();
        let auth = Arc::new(Auth {
            ice_config: ice_path
                .map(|p| Arc::new(crate::ice_config::IceSource::open(p, now().unwrap()).unwrap())),
            host_public_key: public.clone(),
            vm_id: grant.grant.vm_id.clone(),
            control_state: Some(dir.clone()),
            files_enabled: false,
            gate: protocol::SessionGate::new(&public, std::slice::from_ref(&grant)).unwrap(),
            state,
            namespace: "no-keychain-test-only".into(),
            close_proofs: std::sync::Mutex::new(BTreeMap::new()),
            registry_read: std::sync::Mutex::new(()),
            test_identity: Some(host.clone()),
        });
        let socket = dir.join("console.sock");
        let calls = Arc::new(std::sync::Mutex::new(vec![]));
        let seen = calls.clone();
        let path = socket.clone();
        let controller = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let method = req["method"].as_str().unwrap();
                seen.lock().unwrap().push(method.into());
                let result = match method {
                    "status" => {
                        json!({"config":{"id":"hostile-peer","backend":"qemu-hvf"},"state":"running","capabilities":{"input":true},"runtime":{"console_socket":path},"pid":123})
                    }
                    "screenshot" => {
                        json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"width":1280,"height":800,"mime_type":"image/png","byte_length":1,"data_base64":"AA=="})
                    }
                    _ => panic!("unexpected controller method"),
                };
                let _ = reader
                    .get_mut()
                    .write_all(format!("{}\n", json!({"result":result})).as_bytes())
                    .await;
            }
        });
        let marker = dir.join("console.sock.started");
        let app = App {
            origin: "http://127.0.0.1:8787".into(),
            host: "127.0.0.1:8787".into(),
            encoder,
            socket,
            web: dir,
            capabilities: json!({"h264":"verified-hardware"}),
            fault_au_egress_hold: false,
            fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false,
            active: Arc::new(Mutex::new(SessionBook::default())),
            auth: Some(auth),
            cleanup_failure_exit: None,
            guest_binding: None,
            encoder_rate: None,
        };
        Self {
            _temp: temp,
            _console: console,
            controller,
            app,
            host,
            device,
            grant,
            calls,
            marker,
        }
    }
    async fn post(&self, path: &str, value: Value) -> (StatusCode, Value) {
        let router = Router::new()
            .route("/auth/challenge", post(super::challenge))
            .route("/auth/offer", post(super::offer))
            .route("/auth/confirm", post(super::confirm))
            .route("/auth/close", post(super::close))
            .with_state(self.app.clone());
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, &self.app.host)
            .header(header::ORIGIN, &self.app.origin)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(10), router.oneshot(req))
            .await
            .unwrap()
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn offer(&self, client: &Client) -> protocol::SignedOffer {
        let(status,value)=self.post("/auth/challenge",json!({"device_id":self.grant.grant.device_id,"vm_id":self.grant.grant.vm_id,"scopes":self.grant.grant.scopes})).await;
        assert_eq!(status, StatusCode::OK);
        let intent = protocol::OfferIntent {
            challenge: serde_json::from_value(value).unwrap(),
            request_id: uuid::Uuid::new_v4().to_string(),
            codec: "h264".into(),
            offer: protocol::SdpBinding::from_sdp(&client.sdp).unwrap(),
        };
        protocol::SignedOffer {
            signature: self.device.sign(&intent.canonical().unwrap()).unwrap(),
            intent,
        }
    }
    async fn negotiate(
        &self,
        client: &Client,
        offer: &protocol::SignedOffer,
    ) -> (protocol::SignedAnswer, String) {
        let (status, response) = self
            .post(
                "/auth/offer",
                json!({"signed_offer":offer,"sdp":client.sdp}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "negotiation must succeed");
        let answer: protocol::SignedAnswer =
            serde_json::from_value(response["signed_answer"].clone()).unwrap();
        let sdp = response["sdp"].as_str().unwrap().into();
        (answer, sdp)
    }
    fn confirmation(&self, answer: &protocol::SignedAnswer) -> protocol::Confirmation {
        let transcript_sha256 = answer.transcript.digest().unwrap();
        protocol::Confirmation {
            signature: self
                .device
                .sign(&protocol::confirmation_bytes(&transcript_sha256).unwrap())
                .unwrap(),
            transcript_sha256,
        }
    }
    fn control(
        &self,
        answer: &protocol::SignedAnswer,
        sequence: u64,
    ) -> protocol::control::SignedControl {
        let request = protocol::control::ControlRequest {
            session_id: answer.transcript.session_id.clone(),
            transcript_sha256: answer.transcript.digest().unwrap(),
            sequence,
            expires_at: now().unwrap() + 5,
            action: protocol::control::ControlAction::ObserveFrame,
        };
        protocol::control::SignedControl {
            signature: self.device.sign(&request.canonical().unwrap()).unwrap(),
            request,
        }
    }
    fn assert_unstarted(&self) {
        assert!(!self.marker.exists(), "encoder must not start");
        assert!(
            !self.calls.lock().unwrap().iter().any(|v| v == "screenshot"),
            "controller must not dispatch"
        );
    }
    async fn close(&self, answer: &protocol::SignedAnswer) {
        let digest = answer.transcript.digest().unwrap();
        let signed = protocol::SignedClose {
            session_id: answer.transcript.session_id.clone(),
            signature: self
                .device
                .sign(&protocol::close_bytes(&answer.transcript.session_id, &digest).unwrap())
                .unwrap(),
            transcript_sha256: digest,
        };
        assert_eq!(
            self.post("/auth/close", serde_json::to_value(signed).unwrap())
                .await
                .0,
            StatusCode::OK
        );
        assert!(self.app.active.lock().await.active.is_none());
    }
}
fn changed_fingerprint(sdp: &str) -> String {
    sdp.lines()
        .map(|line| {
            if line.starts_with("a=fingerprint:") {
                format!("a=fingerprint:sha-256 {}", vec!["AA"; 32].join(":"))
            } else {
                line.into()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

#[tokio::test]
async fn hostile_signal_then_exact_peer_confirmation_controls_once() {
    let h = Harness::new();
    let client = Client::new().await;
    let offer = h.offer(&client).await;
    for sdp in [
        format!("{}a=x-signaling-tamper:1\r\n", client.sdp),
        changed_fingerprint(&client.sdp),
    ] {
        assert_eq!(
            h.post("/auth/offer", json!({"signed_offer":offer,"sdp":sdp}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        h.assert_unstarted();
        assert!(h.app.active.lock().await.active.is_none());
    }
    let mut foreign = offer.clone();
    foreign.signature = MemoryKey::new(3)
        .sign(&foreign.intent.canonical().unwrap())
        .unwrap();
    assert_eq!(
        h.post(
            "/auth/offer",
            json!({"signed_offer":foreign,"sdp":client.sdp})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut wrong_grant = offer.clone();
    wrong_grant.intent.challenge.grant_version += 1;
    wrong_grant.signature = h
        .device
        .sign(&wrong_grant.intent.canonical().unwrap())
        .unwrap();
    assert_eq!(
        h.post(
            "/auth/offer",
            json!({"signed_offer":wrong_grant,"sdp":client.sdp})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    h.assert_unstarted();
    let (answer, sdp) = h.negotiate(&client, &offer).await;
    assert_eq!(
        h.post(
            "/auth/offer",
            json!({"signed_offer":offer,"sdp":client.sdp})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    h.assert_unstarted();
    // These are real generated server SDP and certificate fingerprints. The client
    // verifies the full signed answer BEFORE permitting set_remote_description.
    for hostile in [
        format!("{sdp}a=x-answer-tamper:1\r\n"),
        changed_fingerprint(&sdp),
    ] {
        assert!(answer
            .verify(&h.host.public_key().unwrap(), &offer.intent, &hostile)
            .is_err());
        h.assert_unstarted();
    }
    answer
        .verify(&h.host.public_key().unwrap(), &offer.intent, &sdp)
        .unwrap();
    client.accept(&sdp).await;
    let confirm = h.confirmation(&answer);
    let mut wrong = confirm.clone();
    wrong.signature = MemoryKey::new(3)
        .sign(&protocol::confirmation_bytes(&wrong.transcript_sha256).unwrap())
        .unwrap();
    for body in [
        json!({"session_id":answer.transcript.session_id,"confirmation":wrong}),
        json!({"session_id":uuid::Uuid::new_v4().to_string(),"confirmation":confirm}),
    ] {
        assert_eq!(h.post("/auth/confirm", body).await.0, StatusCode::FORBIDDEN);
        h.assert_unstarted();
        assert!(!*h
            .app
            .active
            .lock()
            .await
            .active
            .as_ref()
            .unwrap()
            .confirmed
            .borrow());
        assert!(h
            .app
            .auth
            .as_ref()
            .unwrap()
            .gate
            .authorize_active(&answer.transcript.session_id, "observe")
            .is_err());
    }
    assert_eq!(
        h.post(
            "/auth/confirm",
            json!({"session_id":answer.transcript.session_id,"confirmation":confirm})
        )
        .await
        .0,
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while !h.marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let control = h.control(&answer, 1);
    client.send(&control).await;
    assert_eq!(client.response().await["result"]["source_generation"], 4);
    client.send(&h.control(&answer, 2)).await;
    assert_eq!(client.response().await["result"]["width"], 1280);
    client.send(&control).await; // Same-channel replay must terminate without a second dispatch.
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.app.active.lock().await.active.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        h.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| *v == "screenshot")
            .count(),
        2
    );
    h.close(&answer).await;
    let stopped = PathBuf::from(format!("{}.stopped", h.app.socket.display()));
    assert_eq!(std::fs::read_to_string(stopped).unwrap(), "stop received");
    let pid: i32 = std::fs::read_to_string(&h.marker).unwrap().parse().unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "test encoder must be reaped"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    client.close().await;
}

#[tokio::test]
async fn current_grant_revocation_blocks_connected_peer_confirmation_before_encoder() {
    let h = Harness::new();
    let client = Client::new().await;
    let offer = h.offer(&client).await;
    let (answer, sdp) = h.negotiate(&client, &offer).await;
    answer
        .verify(&h.host.public_key().unwrap(), &offer.intent, &sdp)
        .unwrap();
    client.accept(&sdp).await;
    let auth = h.app.auth.as_ref().unwrap();
    let store = protocol::store::Store::open(&auth.state).unwrap();
    let mut registry = store.load().unwrap();
    registry
        .revoke(&h.grant.grant.device_id, &h.grant.grant.vm_id)
        .unwrap();
    store.save(&registry).unwrap();
    drop(store);
    assert_eq!(h.post("/auth/confirm",json!({"session_id":answer.transcript.session_id,"confirmation":h.confirmation(&answer)})).await.0,StatusCode::FORBIDDEN);
    h.assert_unstarted();
    h.close(&answer).await;
    client.close().await;
    h.assert_unstarted();
}

#[tokio::test]
async fn signed_control_for_another_session_cannot_select_peer_authority() {
    let h = Harness::new();
    let client = Client::new().await;
    let offer = h.offer(&client).await;
    let (answer, sdp) = h.negotiate(&client, &offer).await;
    answer
        .verify(&h.host.public_key().unwrap(), &offer.intent, &sdp)
        .unwrap();
    client.accept(&sdp).await;
    assert_eq!(h.post("/auth/confirm",json!({"session_id":answer.transcript.session_id,"confirmation":h.confirmation(&answer)})).await.0,StatusCode::OK);
    let mut wrong = h.control(&answer, 1);
    wrong.request.session_id = uuid::Uuid::new_v4().to_string();
    wrong.signature = h.device.sign(&wrong.request.canonical().unwrap()).unwrap();
    client.send(&wrong).await;
    // Rejection sends best-effort JSON, then fences/closes the channel. SCTP
    // enqueue is not a delivery receipt: closure may legitimately win. Never
    // accept a successful result for this cross-session request in either case.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.channel.poll().await {
                Some(DataChannelEvent::OnMessage(message)) => {
                    assert!(!message.is_string && message.data.len() > 17);
                    let response: Value = serde_json::from_slice(&message.data[17..]).unwrap();
                    assert!(response.get("error").is_some());
                    assert!(response.get("result").is_none());
                }
                Some(
                    DataChannelEvent::OnOpen
                    | DataChannelEvent::OnBufferedAmountLow
                    | DataChannelEvent::OnBufferedAmountHigh,
                ) => {}
                _ => break,
            }
        }
    })
    .await
    .expect("cross-session rejection must close channel promptly");
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.app.active.lock().await.active.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!h.calls.lock().unwrap().iter().any(|v| v == "screenshot"));
    h.close(&answer).await;
    client.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly started loopback coturn and fresh private config"]
async fn local_turn_production_signed_session_reaches_encoder_and_control() {
    let path = PathBuf::from(
        std::env::var("VEYNODE_TEST_ICE_CONFIG").expect("explicit private local TURN fixture"),
    );
    let config = crate::ice_config::IceSource::open(path.clone(), now().unwrap())
        .unwrap()
        .read(now().unwrap())
        .unwrap();
    let h = Harness::with_ice(Some(path));
    let client = Client::with_ice(Some(&config)).await;
    let offer = h.offer(&client).await;
    let (answer, sdp) = h.negotiate(&client, &offer).await;
    answer
        .verify(&h.host.public_key().unwrap(), &offer.intent, &sdp)
        .unwrap();
    let connected_at = Instant::now();
    client.accept_for(&sdp, Duration::from_secs(10)).await;
    eprintln!(
        "test-only TURN channel-open elapsed_ms={}",
        connected_at.elapsed().as_millis()
    );
    assert_eq!(h.post("/auth/confirm",json!({"session_id":answer.transcript.session_id,"confirmation":h.confirmation(&answer)})).await.0,StatusCode::OK);
    let started = tokio::time::timeout(Duration::from_secs(3), async {
        while !h.marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if started {
        client.send(&h.control(&answer, 1)).await;
        assert_eq!(client.response().await["result"]["source_generation"], 4);
    }
    h.close(&answer).await;
    client.close().await;
    assert!(
        started,
        "production connected/confirmed readiness did not start synthetic encoder"
    );
    let pid: i32 = std::fs::read_to_string(&h.marker).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
