//! Private Unix protocol fixtures exercise the adapter and real crypto, not Guest input delivery.
use super::*;
use p256::ecdsa::{signature::Signer, SigningKey};
use protocol::{control::ControlRequest, Identity};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Mutex as StdMutex};
use tokio::net::UnixListener;
struct TestKey(SigningKey);
impl TestKey {
    fn new(n: u8) -> Self {
        let mut v = [0; 32];
        v[31] = n;
        Self(SigningKey::from_bytes((&v).into()).unwrap())
    }
}
impl Identity for TestKey {
    fn public_key(&self) -> Result<String> {
        Ok(protocol::hex(
            self.0.verifying_key().to_encoded_point(false).as_bytes(),
        ))
    }
    fn sign(&self, b: &[u8]) -> Result<String> {
        let s: p256::ecdsa::Signature = self.0.sign(b);
        Ok(protocol::hex(&s.to_bytes()))
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    _console: UnixListener,
    task: tokio::task::JoinHandle<()>,
    session: Arc<SessionInput>,
    key: TestKey,
    calls: Arc<StdMutex<Vec<Value>>>,
    reply_mode: Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().canonicalize().unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let console = UnixListener::bind(dir.join("console.sock")).unwrap();
        std::fs::set_permissions(
            dir.join("console.sock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let listener = UnixListener::bind(dir.join("control.sock")).unwrap();
        std::fs::set_permissions(
            dir.join("control.sock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let host = TestKey::new(1);
        let key = TestKey::new(2);
        let public = host.public_key().unwrap();
        let grant = protocol::SignedGrant::new(
            protocol::Grant {
                host_id: protocol::identity_id(&public).unwrap(),
                device_id: protocol::identity_id(&key.public_key().unwrap()).unwrap(),
                device_public_key: key.public_key().unwrap(),
                vm_id: "linux-input-test".into(),
                scopes: vec!["input".into(), "observe".into()],
                version: 1,
            },
            &host,
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
            host_public_key: public.clone(),
            state,
            namespace: "never-keychain".into(),
            vm_id: grant.grant.vm_id.clone(),
            gate: protocol::SessionGate::new(&public, std::slice::from_ref(&grant)).unwrap(),
            control_state: Some(dir.clone()),
            close_proofs: StdMutex::new(BTreeMap::new()),
        });
        let sdp = format!(
            "v=0\r\na=fingerprint:sha-256 {}\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\n",
            vec!["11"; 32].join(":")
        );
        let intent = protocol::OfferIntent {
            challenge: auth
                .gate
                .challenge(
                    &grant.grant.device_id,
                    &grant.grant.vm_id,
                    grant.grant.scopes.clone(),
                    now().unwrap(),
                )
                .unwrap(),
            request_id: uuid::Uuid::new_v4().to_string(),
            codec: "h265".into(),
            offer: protocol::SdpBinding::from_sdp(&sdp).unwrap(),
        };
        let offer = protocol::SignedOffer {
            signature: key.sign(&intent.canonical().unwrap()).unwrap(),
            intent,
        };
        let accepted = auth
            .gate
            .accept_offer(&offer, &sdp, now().unwrap())
            .unwrap();
        let answer = auth
            .gate
            .answer(
                accepted,
                uuid::Uuid::new_v4().to_string(),
                &sdp,
                &host,
                now().unwrap(),
            )
            .unwrap();
        let digest = answer.transcript.digest().unwrap();
        auth.gate
            .confirm(
                &answer.transcript.session_id,
                &protocol::Confirmation {
                    signature: key
                        .sign(&protocol::confirmation_bytes(&digest).unwrap())
                        .unwrap(),
                    transcript_sha256: digest,
                },
                now().unwrap(),
            )
            .unwrap();
        auth.close_proofs.lock().unwrap().insert(
            answer.transcript.session_id.clone(),
            super::super::CloseProof {
                transcript: answer.transcript.clone(),
                device_public_key: key.public_key().unwrap(),
                issued: Instant::now(),
            },
        );
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let seen = calls.clone();
        let path = dir.join("console.sock");
        let reply_mode = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mode = reply_mode.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen.clone();
                let path = path.clone();
                let mode = mode.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(socket);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.is_err() {
                        return;
                    };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let mut response = json!({"result":match req["method"].as_str().unwrap(){
    "capabilities"=>json!({"control_session_fencing":{"retired_owner_ttl_seconds":120,"request_dispatch_deadline_seconds":30}}),
    "status"=>json!({"config":{"id":"linux-input-test","backend":"qemu-hvf"},"state":"running","capabilities":{"input":true},"runtime":{"console_socket":path},"pid":123}),
    "acquire_control"=>json!({"epoch":7,"expires_in_seconds":60}),
    "release_control"=>json!({"released":true}),
    "screenshot"=>json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"width":1280,"height":800,"mime_type":"image/png","byte_length":1,"data_base64":"AA=="}),
    "input_click"=>json!({"dispatched":true,"guest_delivery_confirmed":false}),
    "session_end"=>json!({"closed":true,"retired_owner":true,"retired_owner_ttl_seconds":120,"released_vm_ids":[]}),
    _=>panic!("unexpected fixture method")}});
                    let current = mode.load(Ordering::SeqCst);
                    if current == 1 && req["method"] == "input_click" {
                        response = json!({"error":"lease revoked by human takeover"});
                    }
                    if current == 2 && req["method"] == "session_end" {
                        response = json!({"result":{"closed":true}});
                    }
                    if current == 3 && req["method"] == "acquire_control" {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    let _ = reader
                        .get_mut()
                        .write_all(format!("{}\n", response).as_bytes())
                        .await;
                });
            }
        });
        let session = SessionInput::attach(
            auth,
            &answer.transcript.session_id,
            dir.clone(),
            dir.join("console.sock"),
        )
        .await
        .unwrap();
        Self {
            _dir: directory,
            _console: console,
            task,
            session,
            key,
            calls,
            reply_mode,
        }
    }
    fn signed(&self, sequence: u64, action: ControlAction) -> SignedControl {
        let request = ControlRequest {
            session_id: self.session.session_id.clone(),
            transcript_sha256: self.session.transcript_sha256.clone(),
            sequence,
            expires_at: now().unwrap() + 5,
            action,
        };
        SignedControl {
            signature: self.key.sign(&request.canonical().unwrap()).unwrap(),
            request,
        }
    }
    async fn anchor(&self) -> FrameAnchor {
        self.session
            .handle(self.signed(1, ControlAction::Acquire))
            .await
            .unwrap();
        let shot = self
            .session
            .handle(self.signed(2, ControlAction::ObserveFrame))
            .await
            .unwrap();
        frame_from_screenshot(&shot["result"]).unwrap()
    }
    fn click(frame: FrameAnchor, epoch: u64) -> ControlAction {
        ControlAction::Click {
            epoch,
            frame,
            x: 10,
            y: 20,
            button: "left".into(),
        }
    }
}
#[tokio::test]
async fn signed_control_routes_current_lease_and_rejects_replay_tamper() {
    let f = Fixture::new().await;
    let frame = f.anchor().await;
    let request = f.signed(3, Fixture::click(frame, 7));
    let ok = f.session.handle(request.clone()).await.unwrap();
    assert_eq!(ok["result"]["dispatched"], true);
    assert!(f
        .session
        .handle(request.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("replay"));
    let mut altered = request;
    altered.request.sequence = 4;
    assert!(f.session.handle(altered).await.is_err());
    let call = f
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|v| v["method"] == "input_click")
        .unwrap()
        .clone();
    assert_eq!(call["arguments"]["owner"], f.session.owner);
    assert_eq!(call["arguments"]["epoch"], 7);
    f.session.end().await.unwrap();
    assert!(f
        .session
        .handle(f.signed(5, ControlAction::Acquire))
        .await
        .is_err());
}
#[tokio::test]
async fn frame_epoch_mismatch_expiry_and_takeover_fail_closed() {
    let f = Fixture::new().await;
    let frame = f.anchor().await;
    assert!(f
        .session
        .handle(f.signed(3, Fixture::click(frame.clone(), 8)))
        .await
        .is_err());
    let mut altered = frame.clone();
    altered.width = 1279;
    assert!(f
        .session
        .handle(f.signed(4, Fixture::click(altered, 7)))
        .await
        .is_err());
    f.session.state.lock().await.frames[0].received = Instant::now() - Duration::from_secs(6);
    assert!(f
        .session
        .handle(f.signed(5, Fixture::click(frame.clone(), 7)))
        .await
        .unwrap_err()
        .to_string()
        .contains("expired"));
    f.session.state.lock().await.frames[0].received = Instant::now();
    f.reply_mode.store(1, Ordering::SeqCst);
    assert!(f
        .session
        .handle(f.signed(6, Fixture::click(frame, 7)))
        .await
        .unwrap_err()
        .to_string()
        .contains("takeover"));
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "input_click")
            .count(),
        1
    );
    f.session.end().await.unwrap();
}
#[tokio::test]
async fn close_requires_retirement_ack_and_interrupts_inflight_acquire() {
    let f = Fixture::new().await;
    f.reply_mode.store(3, Ordering::SeqCst);
    let session = f.session.clone();
    let request = f.signed(1, ControlAction::Acquire);
    let running = tokio::spawn(async move { session.handle(request).await });
    loop {
        if f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["method"] == "acquire_control")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(f
        .session
        .handle(f.signed(2, ControlAction::Acquire))
        .await
        .unwrap_err()
        .to_string()
        .contains("busy"));
    f.reply_mode.store(2, Ordering::SeqCst);
    assert!(f
        .session
        .end()
        .await
        .unwrap_err()
        .to_string()
        .contains("acknowledgement"));
    assert!(running.await.unwrap().is_err());
    f.reply_mode.store(0, Ordering::SeqCst);
    assert_eq!(
        f.session.end().await.unwrap()["lease_release_confirmed"],
        true
    );
}
#[tokio::test]
async fn revoked_registry_and_replaced_console_reject_before_input() {
    let f = Fixture::new().await;
    let frame = f.anchor().await;
    let store = protocol::store::Store::open(&f.session.auth.state).unwrap();
    let mut registry = store.load().unwrap();
    let device = protocol::identity_id(&f.key.public_key().unwrap()).unwrap();
    registry.revoke(&device, &f.session.vm_id).unwrap();
    store.save(&registry).unwrap();
    assert!(f
        .session
        .handle(f.signed(3, Fixture::click(frame, 7)))
        .await
        .is_err());
    assert!(!f
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["method"] == "input_click"));
    f.session.end().await.unwrap();
    let other = Fixture::new().await;
    let frame = other.anchor().await;
    std::fs::remove_file(&other.session.console.path).unwrap();
    let _replacement = UnixListener::bind(&other.session.console.path).unwrap();
    std::fs::set_permissions(
        &other.session.console.path,
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(other
        .session
        .handle(other.signed(3, Fixture::click(frame, 7)))
        .await
        .unwrap_err()
        .to_string()
        .contains("incarnation"));
    other.session.end().await.unwrap();
}
