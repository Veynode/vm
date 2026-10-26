//! Private Unix protocol fixtures exercise the adapter and real crypto, not Guest input delivery.
use super::*;
use p256::ecdsa::{signature::Signer, SigningKey};
use protocol::{control::ControlRequest, Identity};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Mutex as StdMutex};
use tokio::net::UnixListener;
use vm_control::screenshot_scaled::Scaled;
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
    /// Reply mode 9: how many more requests of the scaled screenshot form are refused as `encoded_size_limit`.
    refusals: Arc<std::sync::atomic::AtomicUsize>,
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
            ice_config: None,
            test_identity: None,
            registry_read: std::sync::Mutex::new(()),
            host_public_key: public.clone(),
            state,
            namespace: "never-keychain".into(),
            vm_id: grant.grant.vm_id.clone(),
            gate: protocol::SessionGate::new(&public, std::slice::from_ref(&grant)).unwrap(),
            control_state: Some(dir.clone()),
            files_enabled: false,
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
        let refusals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let refuse = refusals.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen.clone();
                let path = path.clone();
                let mode = mode.clone();
                let refuse = refuse.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(socket);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.is_err() {
                        return;
                    };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let mut response = json!({"result":match req["method"].as_str().unwrap(){
    "status"=>json!({"config":{"id":"linux-input-test","backend":"qemu-hvf"},"state":"running","capabilities":{"input":true},"runtime":{"console_socket":path},"pid":123}),
    "screenshot"=>json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"width":1280,"height":800,"mime_type":"image/png","byte_length":1,"data_base64":"AA=="}),
    "input_click"=>json!({"dispatched":true,"guest_delivery_confirmed":false}),
    _=>panic!("unexpected fixture method")}});
                    let current = mode.load(Ordering::SeqCst);
                    if current == 4 && req["method"] == "screenshot" {
                        // A full-resolution controller reply above the browser transport bound.
                        let png = control_frame::tests::stored_png(
                            2560,
                            1440,
                            1,
                            &control_frame::tests::noise(2560 * 1440, 9),
                        );
                        response =
                            json!({"result":control_frame::tests::screenshot(2560, 1440, &png)});
                    }
                    if current == 5 && req["method"] == "screenshot" {
                        // What a console helper with a smaller PNG cap answers for a large noisy screen.
                        response = json!({"error":{"code":"screenshot_unavailable_or_too_large","message":"GUEST_MARKER_TEXT"}});
                    }
                    if current == 6 && req["method"] == "screenshot" {
                        // Above the 700 KiB frame bound and not a PNG at all: a reply that contradicts itself.
                        let junk = vec![0u8; 800 * 1024];
                        response =
                            json!({"result":control_frame::tests::screenshot(2560, 1440, &junk)});
                    }
                    if current == 7 && req["method"] == "screenshot" {
                        // A valid 16-bit PNG header: well-formed, but a form this service does not decode.
                        let png = control_frame::tests::unsupported_png(800 * 1024);
                        response =
                            json!({"result":control_frame::tests::screenshot(2560, 1440, &png)});
                    }
                    if (8..=15).contains(&current) && req["method"] == "screenshot" {
                        response = scaled_controller_reply(current, &req["arguments"], &refuse);
                    }
                    if current == 1 && req["method"] == "input_click" {
                        response = json!({"error":"input rejected by the guest"});
                    }
                    if current == 3 && req["method"] == "screenshot" {
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
            refusals,
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
        let shot = self
            .session
            .handle(self.signed(2, ControlAction::ObserveFrame))
            .await
            .unwrap();
        frame_from_screenshot(&shot["result"]).unwrap()
    }
    fn click(frame: FrameAnchor) -> ControlAction {
        ControlAction::Click {
            frame,
            x: 10,
            y: 20,
            button: "left".into(),
        }
    }
}
#[tokio::test]
async fn signed_control_routes_input_and_rejects_replay_tamper() {
    let f = Fixture::new().await;
    let frame = f.anchor().await;
    let request = f.signed(3, Fixture::click(frame));
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
    assert!(call["arguments"].get("owner").is_none());
    f.session.end();
    assert!(f
        .session
        .handle(f.signed(5, ControlAction::ObserveFrame))
        .await
        .is_err());
}
#[tokio::test]
async fn frame_mismatch_expiry_and_rejection_fail_closed() {
    let f = Fixture::new().await;
    let frame = f.anchor().await;
    let mut altered = frame.clone();
    altered.width = 1279;
    assert!(f
        .session
        .handle(f.signed(4, Fixture::click(altered)))
        .await
        .is_err());
    f.session.state.lock().await.frames[0].received = Instant::now() - Duration::from_secs(6);
    assert!(f
        .session
        .handle(f.signed(5, Fixture::click(frame.clone())))
        .await
        .unwrap_err()
        .to_string()
        .contains("expired"));
    f.session.state.lock().await.frames[0].received = Instant::now();
    f.reply_mode.store(1, Ordering::SeqCst);
    assert!(f
        .session
        .handle(f.signed(6, Fixture::click(frame)))
        .await
        .unwrap_err()
        .to_string()
        .contains("rejected"));
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "input_click")
            .count(),
        1
    );
    f.session.end();
}
#[tokio::test]
async fn end_interrupts_inflight_rpc_and_rejects_later_actions() {
    let f = Fixture::new().await;
    f.reply_mode.store(3, Ordering::SeqCst);
    let session = f.session.clone();
    let request = f.signed(1, ControlAction::ObserveFrame);
    let running = tokio::spawn(async move { session.handle(request).await });
    loop {
        if f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["method"] == "screenshot")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(f
        .session
        .handle(f.signed(2, ControlAction::ObserveFrame))
        .await
        .unwrap_err()
        .to_string()
        .contains("busy"));
    f.session.end();
    assert!(running.await.unwrap().is_err());
    assert!(f
        .session
        .handle(f.signed(3, ControlAction::ObserveFrame))
        .await
        .is_err());
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
        .handle(f.signed(3, Fixture::click(frame)))
        .await
        .is_err());
    assert!(!f
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["method"] == "input_click"));
    f.session.end();
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
        .handle(other.signed(3, Fixture::click(frame)))
        .await
        .unwrap_err()
        .to_string()
        .contains("incarnation"));
    other.session.end();
}
#[tokio::test]
async fn oversize_1440p_screenshot_becomes_a_bound_scaled_frame_and_input_stays_in_source_pixels() {
    let f = Fixture::new().await;
    f.reply_mode.store(4, Ordering::SeqCst);
    let shot = f
        .session
        .handle(f.signed(2, ControlAction::ObserveFrame))
        .await
        .unwrap();
    let result = &shot["result"];
    // What reaches the browser: a JPEG within the unchanged 700 KiB bound, declaring its exact scale.
    assert_eq!(result["mime_type"], "image/jpeg");
    assert!(result["byte_length"].as_u64().unwrap() <= 700 * 1024);
    assert!(result["data_base64"].as_str().unwrap().len() <= 956_000);
    assert!(
        serde_json::to_vec(&shot).unwrap().len() <= crate::control_channel::JSON_BYTES,
        "the whole control reply still fits the data-channel message bound"
    );
    let divisor = result["scale_divisor"].as_u64().unwrap();
    assert!(divisor > 1);
    assert_eq!(result["frame_width"], 2560u64.div_ceil(divisor));
    assert_eq!(result["frame_height"], 1440u64.div_ceil(divisor));
    // The anchor is the SOURCE geometry and the controller's own frame identity.
    let frame = frame_from_screenshot(result).unwrap();
    assert_eq!(
        frame,
        FrameAnchor {
            frame_id: "01234567-89ab-4def-8123-456789abcdef".into(),
            source_generation: 4,
            width: 2560,
            height: 1440,
        }
    );
    f.reply_mode.store(0, Ordering::SeqCst);
    let click = |frame: &FrameAnchor, x: u64, y: u64| ControlAction::Click {
        frame: frame.clone(),
        x,
        y,
        button: "left".into(),
    };
    // An anchor in frame (scaled) pixels is a substituted geometry: refused before any input RPC.
    let mut scaled = frame.clone();
    scaled.width = result["frame_width"].as_u64().unwrap();
    scaled.height = result["frame_height"].as_u64().unwrap();
    assert!(f
        .session
        .handle(f.signed(3, click(&scaled, 10, 20)))
        .await
        .unwrap_err()
        .to_string()
        .contains("substituted"));
    // One past the source edge cannot even be signed/canonicalised.
    for (x, y) in [(2560, 0), (0, 1440)] {
        assert!(ControlRequest {
            session_id: f.session.session_id.clone(),
            transcript_sha256: f.session.transcript_sha256.clone(),
            sequence: 4,
            expires_at: now().unwrap() + 5,
            action: click(&frame, x, y),
        }
        .canonical()
        .is_err());
    }
    assert!(!f
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["method"] == "input_click"));
    // The far source corner, beyond the scaled image's own extent, is dispatched unchanged in source pixels.
    let ok = f
        .session
        .handle(f.signed(5, click(&frame, 2559, 1439)))
        .await
        .unwrap();
    assert_eq!(ok["result"]["dispatched"], true);
    let call = f
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|v| v["method"] == "input_click")
        .unwrap()
        .clone();
    assert_eq!(call["arguments"]["x"], 2559);
    assert_eq!(call["arguments"]["y"], 1439);
    assert_eq!(call["arguments"]["width"], 2560);
    assert_eq!(call["arguments"]["height"], 1440);
    assert_eq!(
        call["arguments"]["frame_id"],
        "01234567-89ab-4def-8123-456789abcdef"
    );
    assert!(call["arguments"].get("scale_divisor").is_none());
    f.session.end();
}
#[tokio::test]
async fn small_screenshot_reply_is_forwarded_unchanged_and_tampered_scaled_frames_are_refused() {
    let f = Fixture::new().await;
    let shot = f
        .session
        .handle(f.signed(1, ControlAction::ObserveFrame))
        .await
        .unwrap();
    assert_eq!(
        shot["result"],
        json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"width":1280,"height":800,"mime_type":"image/png","byte_length":1,"data_base64":"AA=="})
    );
    // The pre-existing bound and MIME refusals are intact.
    let mut over = shot["result"].clone();
    over["byte_length"] = json!(700 * 1024 + 1);
    assert!(frame_from_screenshot(&over).is_err());
    let mut other = shot["result"].clone();
    other["mime_type"] = json!("image/gif");
    assert!(frame_from_screenshot(&other).is_err());
    // A controller JPEG without a verified scale never becomes an anchor.
    let mut jpeg = shot["result"].clone();
    jpeg["mime_type"] = json!("image/jpeg");
    assert!(frame_from_screenshot(&jpeg).is_err());
    let mut smuggled = shot["result"].clone();
    smuggled["scale_divisor"] = json!(2);
    assert!(frame_from_screenshot(&smuggled).is_err());
    f.session.end();
}

fn code(error: &anyhow::Error) -> &'static str {
    FailureCode::of(error).map_or("untagged", FailureCode::name)
}
#[tokio::test]
async fn refused_control_picture_is_a_typed_reply_that_keeps_the_session() {
    let f = Fixture::new().await;
    for (sequence, mode, reason) in [
        (2u64, 5usize, "controller_refused"),
        (3, 7, "png_decode_unsupported"),
    ] {
        f.reply_mode.store(mode, Ordering::SeqCst);
        let reply = f
            .session
            .handle(f.signed(sequence, ControlAction::ObserveFrame))
            .await
            .unwrap();
        // Exactly three fields; nothing from the controller's own error text reaches the browser.
        assert_eq!(
            reply,
            json!({"sequence":sequence,"error":FRAME_UNAVAILABLE,"reason":reason})
        );
        assert!(!reply.to_string().contains("GUEST_MARKER_TEXT"));
        let state = f.session.state.lock().await;
        assert!(state.frames.is_empty(), "no anchor recorded");
        assert_eq!(state.sequence, sequence, "the sequence is consumed");
    }
    // The consumed sequence cannot be replayed, and that is still a fencing error.
    f.reply_mode.store(0, Ordering::SeqCst);
    let replay = f
        .session
        .handle(f.signed(3, ControlAction::ObserveFrame))
        .await
        .unwrap_err();
    assert_eq!(code(&replay), "request_replay");
    // The same session then gets a picture and sends input in the session it never lost.
    let shot = f
        .session
        .handle(f.signed(4, ControlAction::ObserveFrame))
        .await
        .unwrap();
    let frame = frame_from_screenshot(&shot["result"]).unwrap();
    let click = f
        .session
        .handle(f.signed(5, Fixture::click(frame.clone())))
        .await
        .unwrap();
    assert_eq!(click["result"]["dispatched"], true);
    // A controller refusal of INPUT is not softened: it stays an error with its own category.
    f.reply_mode.store(1, Ordering::SeqCst);
    let refused = f
        .session
        .handle(f.signed(6, Fixture::click(frame)))
        .await
        .unwrap_err();
    assert_eq!(code(&refused), "controller_refused");
    assert!(refused.to_string().contains("input rejected by the guest"));
}
#[tokio::test]
async fn self_contradicting_screenshot_reply_and_denied_authority_still_fail_closed() {
    let f = Fixture::new().await;
    f.reply_mode.store(6, Ordering::SeqCst);
    let corrupt = f
        .session
        .handle(f.signed(2, ControlAction::ObserveFrame))
        .await
        .unwrap_err();
    assert_eq!(code(&corrupt), "png_decode_corrupt");
    assert!(frame_unavailable(&corrupt).is_none());
    // Anchor violations name their category.
    f.reply_mode.store(0, Ordering::SeqCst);
    let shot = f
        .session
        .handle(f.signed(3, ControlAction::ObserveFrame))
        .await
        .unwrap();
    let mut unknown = frame_from_screenshot(&shot["result"]).unwrap();
    unknown.frame_id = "11111111-89ab-4def-8123-456789abcdef".into();
    let unknown_anchor = f
        .session
        .handle(f.signed(4, Fixture::click(unknown)))
        .await
        .unwrap_err();
    assert_eq!(code(&unknown_anchor), "frame_anchor");
    // A refused screenshot does not hide an ended session: the closing check runs before any typed reply.
    f.reply_mode.store(5, Ordering::SeqCst);
    f.session.closing.store(true, Ordering::Release);
    let ended = f
        .session
        .handle(f.signed(5, ControlAction::ObserveFrame))
        .await
        .unwrap_err();
    assert_eq!(code(&ended), "session_ended");
    // Only the four picture reasons are ever soft.
    for hard in [
        FailureCode::ScreenshotReplyTooLarge,
        FailureCode::ScreenshotReplyInvalid,
        FailureCode::PngDecodeCorrupt,
        FailureCode::FrameVerification,
        FailureCode::FrameWorker,
        FailureCode::ControllerUnavailable,
        FailureCode::ControllerReplyInvalid,
        FailureCode::InputAuthorization,
        FailureCode::VmStatus,
        FailureCode::FrameAnchor,
        FailureCode::Signature,
        FailureCode::Replay,
        FailureCode::RequestExpired,
        FailureCode::SessionEnded,
        FailureCode::DispatchRejected,
    ] {
        assert!(
            frame_unavailable(&hard.tag(anyhow::anyhow!("x"))).is_none(),
            "{hard}"
        );
    }
    assert!(frame_unavailable(&anyhow::anyhow!("untagged")).is_none());
}
#[tokio::test]
async fn screenshot_reply_above_the_response_bound_is_named_and_never_buffered_past_the_bound() {
    // A controller that answers `screenshot` with more than MAX_SCREENSHOT_RESPONSE bytes on one line.
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().canonicalize().unwrap().join("c.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let chunk = vec![b'a'; 1 << 20];
            let mut left = SCREENSHOT_RPC_LIMIT as usize + 2;
            while left > 0 {
                let take = left.min(chunk.len());
                if reader.get_mut().write_all(&chunk[..take]).await.is_err() {
                    break;
                }
                left -= take;
            }
        }
    });
    let socket = SocketIdentity::read(&path).unwrap();
    let shot = rpc(&socket, "screenshot", json!({}), Duration::from_secs(10))
        .await
        .unwrap_err();
    assert_eq!(code(&shot), "screenshot_reply_too_large");
    assert!(frame_unavailable(&shot).is_none());
    let other = rpc(&socket, "status", json!({}), Duration::from_secs(10))
        .await
        .unwrap_err();
    assert_eq!(code(&other), "controller_reply_invalid");
    server.abort();
    // The figures this path is bounded by.
    assert_eq!(SCREENSHOT_RPC_LIMIT, 12 * 1024 * 1024);
    assert_eq!(RPC_LIMIT, 1024 * 1024);
    assert_eq!(vm_control::MAX_SCREENSHOT_BYTES, 8 * 1024 * 1024);
}
/// Every token of a timing line after the fixed prefix is `stage_name=decimal`, the name a short fixed word.
fn assert_numbers_only(line: &str) {
    let rest = line
        .strip_prefix("control frame timing ")
        .unwrap_or_else(|| panic!("prefix: {line}"));
    let mut names = Vec::new();
    for token in rest.split(' ') {
        let (name, value) = token
            .split_once('=')
            .unwrap_or_else(|| panic!("token: {token}"));
        assert!(
            name.starts_with(|c: char| c.is_ascii_lowercase())
                && name.len() <= 20
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{token}"
        );
        assert!(
            !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit()),
            "{token}"
        );
        names.push(name);
    }
    assert_eq!(names.len(), 33, "{line}");
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "{line}");
}
#[tokio::test]
async fn observe_frame_timing_line_is_stage_names_and_numbers_only() {
    let f = Fixture::new().await;
    f.reply_mode.store(4, Ordering::SeqCst);
    // Only `observe_frame` records a timing.
    assert!(f.session.unsent.lock().unwrap().is_none());
    let shot = f
        .session
        .handle(f.signed(2, ControlAction::ObserveFrame))
        .await
        .unwrap();
    let (sequence, timing) = f.session.unsent.lock().unwrap().expect("timing recorded");
    assert_eq!(sequence, 2);
    let frame = &timing.frame;
    assert!(!frame.passthrough && frame.attempts >= 1);
    assert_eq!((frame.width, frame.height), (2560, 1440));
    assert_eq!(
        Some(frame.frame_bytes as u64),
        shot["result"]["byte_length"].as_u64()
    );
    assert_eq!(
        Some(frame.divisor as u64),
        shot["result"]["scale_divisor"].as_u64()
    );
    assert!(frame.source_bytes > 700 * 1024 && timing.rpc.bytes > frame.source_bytes);
    // The stages lie inside the total, in order of containment.
    assert!(timing.total_us >= timing.admit_us + timing.rpc.wait_us + timing.rpc.read_us);
    assert!(timing.worker_us >= frame.inflate_us + frame.jpeg_us);
    assert!(timing.total_us >= timing.worker_us + timing.verify_us);
    let reply = crate::control_channel::ReplyTiming {
        serialise_us: 1234,
        send_us: 56_789,
        bytes: 766_998,
        fragments: 47,
    };
    let line = timing_line(&timing, Some(&reply));
    eprintln!("{line}");
    assert_numbers_only(&line);
    assert!(
        line.contains(" send_ms=57 ")
            && line.contains(" fragments=47 ")
            && line.ends_with(" sent=1")
    );
    assert!(line.contains(" width=2560 height=1440 "));
    // Nothing of the picture or its identity is in the line.
    for secret in [
        shot["result"]["sha256"].as_str().unwrap(),
        shot["result"]["frame_id"].as_str().unwrap(),
    ] {
        assert!(!line.contains(secret) && !line.contains(&secret[..12]));
    }
    assert_numbers_only(&timing_line(&timing, None));
    // The largest values still print as plain numbers.
    let mut extreme = timing;
    extreme.total_us = u64::MAX - 1000;
    extreme.frame.raw_bytes = usize::MAX;
    assert_numbers_only(&timing_line(&extreme, Some(&reply)));
    // The line belongs to exactly one reply: another sequence leaves it, its own takes it.
    f.session.replied(3, reply);
    assert!(f.session.unsent.lock().unwrap().is_some());
    f.session.replied(2, reply);
    assert!(f.session.unsent.lock().unwrap().is_none());
    // A small PNG is passed through and says so.
    f.reply_mode.store(0, Ordering::SeqCst);
    f.session
        .handle(f.signed(3, ControlAction::ObserveFrame))
        .await
        .unwrap();
    let (sequence, timing) = f.session.unsent.lock().unwrap().expect("timing recorded");
    assert_eq!(sequence, 3);
    assert!(timing.frame.passthrough && timing.frame.attempts == 0);
    let line = timing_line(&timing, None);
    assert_numbers_only(&line);
    assert!(line.contains(" passthrough=1 ") && line.ends_with(" sent=0"));
}

// ---------------------------------------------------------------------------------------------------------------
// The control picture as the console helper's scaled JPEG (decision 0025 item 2)
// ---------------------------------------------------------------------------------------------------------------
const VM: &str = "linux-input-test";
/// The 2560x1440 screen behind reply modes 8 to 15.
fn scripted_screen() -> control_frame::Rgb {
    control_frame::helper_tests::smooth(2560, 1440)
}
/// The full-size PNG of that controller: above the control-frame bound, so it is scaled and re-encoded here.
fn scripted_png() -> Value {
    static PNG: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PNG.get_or_init(|| {
        let png = control_frame::tests::stored_png(
            2560,
            1440,
            1,
            &control_frame::tests::noise(2560 * 1440, 9),
        );
        control_frame::tests::screenshot(2560, 1440, &png)
    })
    .clone()
}
/// The helper's JPEG for one request; with `big`, filled up to more than half the bound.
fn scripted_jpeg(asked: &Scaled, big: bool) -> Value {
    static MADE: StdMutex<BTreeMap<(u64, u64, u64, bool), Value>> = StdMutex::new(BTreeMap::new());
    let key = (asked.max_width, asked.max_height, asked.quality, big);
    if let Some(made) = MADE.lock().unwrap().get(&key) {
        return made.clone();
    }
    let mut result = control_frame::helper_tests::scripted_reply(&scripted_screen(), asked);
    let engine = &base64::engine::general_purpose::STANDARD;
    let mut jpeg = base64::Engine::decode(engine, result["data_base64"].as_str().unwrap()).unwrap();
    assert!(
        jpeg.len() * 2 < control_frame::CONTROL_FRAME_MAX_BYTES,
        "the plain picture is a small one"
    );
    if big {
        // Comment segments right behind the start marker: still a JPEG with the same header and image.
        let mut filled = jpeg[..2].to_vec();
        while filled.len() + jpeg.len() <= control_frame::CONTROL_FRAME_MAX_BYTES / 2 {
            filled.extend_from_slice(&[0xff, 0xfe, 0xff, 0xff]);
            filled.extend_from_slice(&[b' '; 0xfffd]);
        }
        filled.extend_from_slice(&jpeg[2..]);
        jpeg = filled;
        result["byte_length"] = json!(jpeg.len());
        result["data_base64"] = json!(base64::Engine::encode(engine, &jpeg));
    }
    MADE.lock().unwrap().insert(key, result.clone());
    result
}
/// Position of a request in `control_frame::helper_steps` for the scripted screen.
fn step_index(asked: &Scaled) -> usize {
    control_frame::helper_steps(Some((2560, 1440)))
        .iter()
        .position(|step| step.scaled == *asked)
        .expect("a request of the policy")
}
/// A controller that has the scaled screenshot form. Without the four fields it answers with the full-size PNG
/// above. With them: mode 8 the helper's JPEG; 9 `encoded_size_limit` while `refusals` lasts, then the JPEG;
/// 10 `format_unavailable`; 11 `invalid_request`; 12 a JPEG whose header is not the declared frame; 13 an
/// ordinary refusal without a word; 14 `scale_unavailable`; 15 a screen whose picture fits from the pair with
/// index `refusals % 100` on (`encoded_size_limit` for every pair before it) and is a big one when
/// `refusals >= 100`.
fn scaled_controller_reply(
    mode: usize,
    arguments: &Value,
    refusals: &std::sync::atomic::AtomicUsize,
) -> Value {
    let Some(asked) = Scaled::parse(arguments).unwrap() else {
        return json!({"result":scripted_png()});
    };
    let refusal =
        |word: &str| json!({"error":"backend: \"GUEST_MARKER_TEXT\"","screenshot_refusal":word});
    let screen = refusals.load(Ordering::SeqCst);
    match mode {
        9 if take_refusal(refusals) => refusal("encoded_size_limit"),
        15 if step_index(&asked) < screen % 100 => refusal("encoded_size_limit"),
        15 => json!({"result":scripted_jpeg(&asked, screen >= 100)}),
        8 | 9 => json!({"result":scripted_jpeg(&asked, false)}),
        10 => refusal("format_unavailable"),
        11 => refusal("invalid_request"),
        12 => {
            let mut result = scripted_jpeg(&asked, false);
            result["frame_height"] = json!(result["frame_height"].as_u64().unwrap() + 2);
            result["height"] = json!(result["height"].as_u64().unwrap() + 2);
            json!({"result":result})
        }
        13 => json!({"error":"backend: \"GUEST_MARKER_TEXT\""}),
        _ => refusal("scale_unavailable"),
    }
}
/// One of the remaining refusals, when there is one. Requests of one session come one at a time.
fn take_refusal(refusals: &std::sync::atomic::AtomicUsize) -> bool {
    let left = refusals.load(Ordering::SeqCst);
    if left > 0 {
        refusals.store(left - 1, Ordering::SeqCst);
    }
    left > 0
}
impl Fixture {
    /// The `arguments` of every `screenshot` request made since the last call.
    fn screenshot_requests(&self) -> Vec<Value> {
        let mut calls = self.calls.lock().unwrap();
        let requests = calls
            .iter()
            .filter(|call| call["method"] == "screenshot")
            .map(|call| call["arguments"].clone())
            .collect();
        calls.clear();
        requests
    }
    async fn observe(&self, sequence: u64) -> Result<Value> {
        self.session
            .handle(self.signed(sequence, ControlAction::ObserveFrame))
            .await
    }
    /// The timing of the last `observe_frame` that produced a picture.
    fn last_timing(&self) -> ObserveTiming {
        self.session
            .unsent
            .lock()
            .unwrap()
            .expect("timing recorded")
            .1
    }
    /// A session on a controller of `mode` whose first picture has been taken: the plain PNG request, as every
    /// session starts, here answered with a PNG above the bound, so that the next pictures are the helper's.
    /// Sequences 1 and 2 are used.
    async fn after_large_png(mode: usize) -> Self {
        let f = Self::new().await;
        f.reply_mode.store(mode, Ordering::SeqCst);
        f.screenshot_requests();
        let shot = f.observe(2).await.unwrap();
        assert_eq!(f.screenshot_requests(), [plain_request()]);
        let result = &shot["result"];
        assert_eq!(
            (&result["mime_type"], &result["source_mime_type"]),
            (&json!("image/jpeg"), &json!("image/png"))
        );
        assert!(result["scale_divisor"].as_u64().unwrap() > 1);
        let timing = f.last_timing();
        assert!(!timing.frame.helper_jpeg && !timing.frame.passthrough);
        assert_eq!((timing.frame.requests, timing.frame.helper_quality), (1, 0));
        f
    }
}
/// The screenshot request as it was before the scaled form existed.
fn plain_request() -> Value {
    json!({"id":VM})
}
fn scaled_request(max_width: u64, max_height: u64, quality: u64) -> Value {
    json!({"id":VM,"format":"jpeg","max_width":max_width,"max_height":max_height,"quality":quality})
}
/// The six requests of the policy for the scripted screen, in order.
fn walk() -> [Value; 6] {
    [
        scaled_request(3840, 2160, 50),
        scaled_request(3840, 2160, 35),
        scaled_request(3840, 2160, 25),
        scaled_request(1280, 720, 50),
        scaled_request(1280, 720, 35),
        scaled_request(1280, 720, 25),
    ]
}
#[tokio::test]
async fn control_picture_is_the_helpers_scaled_jpeg_with_the_fields_the_transform_writes() {
    let f = Fixture::after_large_png(8).await;
    let shot = f.observe(3).await.unwrap();
    // One request, for the size and quality the transform starts at: unscaled, our quality 80 as the helper's 50.
    assert_eq!(f.screenshot_requests(), [scaled_request(3840, 2160, 50)]);
    let result = &shot["result"];
    assert_eq!(result["mime_type"], "image/jpeg");
    assert_eq!(
        (&result["width"], &result["height"]),
        (&json!(2560), &json!(1440))
    );
    assert_eq!(
        (&result["frame_width"], &result["frame_height"]),
        (&json!(2560), &json!(1440))
    );
    assert_eq!(
        (&result["scale_divisor"], &result["frame_quality"]),
        (&json!(1), &json!(80))
    );
    let jpeg = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        result["data_base64"].as_str().unwrap(),
    )
    .unwrap();
    assert!(jpeg.len() <= control_frame::CONTROL_FRAME_MAX_BYTES);
    assert_eq!(result["byte_length"], jpeg.len());
    assert_eq!(result["sha256"], protocol::hash(&jpeg));
    assert_eq!(control_frame::jpeg_dimensions(&jpeg).unwrap(), (2560, 1440));
    assert!(
        serde_json::to_vec(&shot).unwrap().len() <= crate::control_channel::JSON_BYTES,
        "the whole control reply still fits the data-channel message bound"
    );
    // The same fields as a transformed PNG of the same controller, except the two that describe a PNG source.
    let transformed = control_frame::control_frame_from_screenshot(
        scaled_controller_reply(8, &json!({"id":VM}), &f.refusals)["result"].take(),
    )
    .unwrap();
    let keys = |value: &Value| {
        value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    let mut expected = keys(&transformed);
    expected.retain(|key| {
        key != "source_mime_type" && key != "source_byte_length" && key != "capture_us"
    });
    let mut actual = keys(result);
    // Fields the daemon adds to every screenshot; the PNG fixture carries its own stand-in (`capture_us`).
    actual.retain(|key| {
        ![
            "source_sequence",
            "coordinate_space",
            "display_id",
            "guest_ui_scale",
            "captured_at_us",
            "captured_monotonic_us",
        ]
        .contains(&key.as_str())
    });
    assert_eq!(actual, expected);
    // The anchor is the source geometry and the controller's frame identity, and input binds to it as before.
    let frame = frame_from_screenshot(result).unwrap();
    assert_eq!(
        frame,
        FrameAnchor {
            frame_id: "01234567-89ab-4def-8123-456789abcdef".into(),
            source_generation: 4,
            width: 2560,
            height: 1440,
        }
    );
    let click = f
        .session
        .handle(f.signed(4, Fixture::click(frame)))
        .await
        .unwrap();
    assert_eq!(click["result"]["dispatched"], true);
    // The log line says which path made the picture and what it cost.
    let timing = f.last_timing();
    assert!(timing.frame.helper_jpeg && !timing.frame.passthrough);
    assert_eq!((timing.frame.requests, timing.frame.attempts), (1, 0));
    assert_eq!(
        (
            timing.frame.quality,
            timing.frame.helper_quality,
            timing.frame.divisor
        ),
        (80, 50, 1)
    );
    assert_eq!(
        (
            timing.frame.width,
            timing.frame.height,
            timing.frame.frame_bytes
        ),
        (2560, 1440, jpeg.len())
    );
    assert_eq!(
        (
            timing.frame.inflate_us,
            timing.frame.scale_us,
            timing.frame.jpeg_us
        ),
        (0, 0, 0)
    );
    let line = timing_line(&timing, None);
    assert_numbers_only(&line);
    assert!(
        line.contains(
            " divisor=1 quality=80 passthrough=0 helper_jpeg=1 helper_quality=50 requests=1 sent=0"
        ),
        "{line}"
    );
    // The next picture is asked for in the same way.
    f.screenshot_requests();
    f.observe(5).await.unwrap();
    assert_eq!(f.screenshot_requests(), [scaled_request(3840, 2160, 50)]);
}
#[tokio::test]
async fn size_limit_refusals_walk_the_transforms_quality_and_divisor_order_within_a_bound() {
    // A helper that never fits, asked from the first pair: every pair of the bounded walk in the transform's
    // order (three qualities unscaled, then exactly half the known geometry), then the PNG request.
    let f = Fixture::after_large_png(9).await;
    f.refusals.store(1000, Ordering::SeqCst);
    let shot = f.observe(3).await.unwrap();
    let requests = f.screenshot_requests();
    assert_eq!(requests.len(), control_frame::HELPER_ATTEMPTS + 1);
    assert_eq!(requests[..control_frame::HELPER_ATTEMPTS], walk());
    assert_eq!(requests[control_frame::HELPER_ATTEMPTS], plain_request());
    assert_eq!(shot["result"]["source_mime_type"], "image/png");
    let timing = f.last_timing();
    assert!(!timing.frame.helper_jpeg && timing.frame.attempts >= 1);
    assert_eq!(
        timing.frame.requests as usize,
        control_frame::HELPER_ATTEMPTS + 1
    );
    assert_eq!(timing.frame.helper_quality, 0);
    let line = timing_line(&timing, None);
    assert_numbers_only(&line);
    assert!(
        line.contains(" passthrough=0 helper_jpeg=0 helper_quality=0 requests=7 sent=0"),
        "{line}"
    );
    // The times and bytes of all seven exchanges are in the line's controller stages.
    assert!(timing.rpc.bytes > timing.frame.source_bytes);
    // A size limit is about that picture, not about the controller: the helper is asked again for the next one
    // (from the last pair now, see the hint tests below), and its picture is used.
    f.refusals.store(0, Ordering::SeqCst);
    let shot = f.observe(4).await.unwrap();
    assert_eq!(f.screenshot_requests(), [scaled_request(1280, 720, 25)]);
    assert!(shot["result"].get("source_mime_type").is_none());
    assert_eq!(
        (
            &shot["result"]["scale_divisor"],
            &shot["result"]["frame_quality"]
        ),
        (&json!(2), &json!(40))
    );

    // One refusal: the next quality step at the same size is the picture.
    let f = Fixture::after_large_png(9).await;
    f.refusals.store(1, Ordering::SeqCst);
    let shot = f.observe(3).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[..2]);
    assert_eq!(
        (
            &shot["result"]["scale_divisor"],
            &shot["result"]["frame_quality"]
        ),
        (&json!(1), &json!(60))
    );
    let timing = f.last_timing();
    assert_eq!(
        (
            timing.frame.requests,
            timing.frame.quality,
            timing.frame.helper_quality
        ),
        (2, 60, 35)
    );
    // Two more: past the last quality the request is for exactly half the known geometry, at the first quality.
    f.refusals.store(2, Ordering::SeqCst);
    let shot = f.observe(4).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[1..4]);
    let result = &shot["result"];
    assert_eq!(
        (&result["scale_divisor"], &result["frame_quality"]),
        (&json!(2), &json!(80))
    );
    assert_eq!(
        (&result["frame_width"], &result["frame_height"]),
        (&json!(1280), &json!(720))
    );
    assert_eq!(
        (&result["width"], &result["height"]),
        (&json!(2560), &json!(1440))
    );
    frame_from_screenshot(result).unwrap();
    assert_eq!(f.last_timing().frame.requests, 3);
}
#[tokio::test]
async fn controller_without_the_scaled_form_gets_the_png_request_and_is_not_asked_twice() {
    // 10: refused as unavailable. 11: refused as invalid. 4: an older controller that ignores the fields and
    // answers with its PNG (used for that picture, no second request).
    for (mode, first) in [(10usize, 2usize), (11, 2), (4, 1)] {
        let f = Fixture::after_large_png(mode).await;
        let shot = f.observe(3).await.unwrap();
        let requests = f.screenshot_requests();
        assert_eq!(requests.len(), first, "mode {mode}");
        assert_eq!(requests[0], scaled_request(3840, 2160, 50));
        if first == 2 {
            assert_eq!(
                requests[1],
                plain_request(),
                "the PNG request is the one that was always made"
            );
        }
        // The picture is the transformed PNG, exactly as before.
        let result = &shot["result"];
        assert_eq!(
            (&result["mime_type"], &result["source_mime_type"]),
            (&json!("image/jpeg"), &json!("image/png"))
        );
        assert!(result["scale_divisor"].as_u64().unwrap() > 1);
        frame_from_screenshot(result).unwrap();
        let timing = f.last_timing();
        assert!(!timing.frame.helper_jpeg);
        assert_eq!(timing.frame.requests as usize, first);
        // Remembered for the session: from now on one request per picture, without the fields.
        for sequence in 4..6 {
            f.observe(sequence).await.unwrap();
            assert_eq!(f.screenshot_requests(), [plain_request()], "mode {mode}");
            assert_eq!(f.last_timing().frame.requests, 1);
        }
        // Even when the controller would serve the form later.
        f.reply_mode.store(8, Ordering::SeqCst);
        f.observe(6).await.unwrap();
        assert_eq!(f.screenshot_requests(), [plain_request()]);
    }
}
#[tokio::test]
async fn session_whose_png_fits_the_bound_never_asks_for_the_scaled_form() {
    // The small PNG of the default fixture: passed through byte for byte, picture after picture, and the
    // controller sees only the request it always saw.
    let f = Fixture::new().await;
    f.screenshot_requests();
    for sequence in 2..8 {
        let shot = f.observe(sequence).await.unwrap();
        assert_eq!(
            shot["result"],
            json!({"frame_id":"01234567-89ab-4def-8123-456789abcdef","source_generation":4,"width":1280,"height":800,"mime_type":"image/png","byte_length":1,"data_base64":"AA=="})
        );
        assert_eq!(f.screenshot_requests(), [plain_request()]);
        let timing = f.last_timing();
        assert!(timing.frame.passthrough && !timing.frame.helper_jpeg);
        assert_eq!((timing.frame.requests, timing.frame.helper_quality), (1, 0));
        let line = timing_line(&timing, None);
        assert!(
            line.contains(" passthrough=1 helper_jpeg=0 helper_quality=0 requests=1 sent=0"),
            "{line}"
        );
    }
    let raw = f.calls.lock().unwrap().clone();
    assert!(raw.is_empty());
}
#[tokio::test]
async fn helper_pictures_start_at_the_pair_that_fitted_last_time() {
    // A noisy screen: nothing fits before the fourth pair (half size, first quality), and that picture is above
    // half the bound.
    let f = Fixture::after_large_png(15).await;
    f.refusals.store(103, Ordering::SeqCst);
    // The first helper picture pays for the three pairs above it, once.
    let shot = f.observe(3).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[..4]);
    assert_eq!(
        (
            &shot["result"]["scale_divisor"],
            &shot["result"]["frame_quality"]
        ),
        (&json!(2), &json!(80))
    );
    assert_eq!(f.last_timing().frame.requests, 4);
    // Every following one is a single request for that pair, however many there are.
    for sequence in 4..24 {
        let shot = f.observe(sequence).await.unwrap();
        assert_eq!(f.screenshot_requests(), walk()[3..4], "picture {sequence}");
        let result = &shot["result"];
        assert_eq!(
            (&result["scale_divisor"], &result["frame_quality"]),
            (&json!(2), &json!(80))
        );
        assert_eq!(
            (&result["frame_width"], &result["frame_height"]),
            (&json!(1280), &json!(720))
        );
        frame_from_screenshot(result).unwrap();
        let timing = f.last_timing();
        assert!(timing.frame.helper_jpeg);
        assert_eq!(
            (
                timing.frame.requests,
                timing.frame.divisor,
                timing.frame.helper_quality
            ),
            (1, 2, 50)
        );
        let line = timing_line(&timing, None);
        assert!(
            line.contains(" divisor=2 quality=80 passthrough=0 helper_jpeg=1 helper_quality=50 requests=1 sent=0"),
            "{line}"
        );
    }
    // The screen gets busier: the walk goes on from the hint, not from the top.
    f.refusals.store(104, Ordering::SeqCst);
    f.observe(24).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[3..5]);
    f.observe(25).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[4..5]);
}
#[tokio::test]
async fn hint_moves_one_pair_better_after_a_small_picture_and_back_after_a_refusal() {
    let f = Fixture::after_large_png(15).await;
    // Fits from the fourth pair on, and the picture is under half the bound.
    f.refusals.store(3, Ordering::SeqCst);
    f.observe(3).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[..4]);
    // The pair above was refused a moment ago: it is not tried again for the next `HINT_HOLD` helper pictures,
    // of which the one just taken is the first.
    let mut sequence = 4;
    for _ in 1..HINT_HOLD {
        f.observe(sequence).await.unwrap();
        assert_eq!(f.screenshot_requests(), walk()[3..4], "picture {sequence}");
        sequence += 1;
    }
    // The screen has become simpler meanwhile (fits from the second pair). The hold is over: this small picture
    // moves the hint, the next request is one pair better, and so is the one after it.
    f.refusals.store(1, Ordering::SeqCst);
    f.observe(sequence).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[3..4]);
    let shot = f.observe(sequence + 1).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[2..3]);
    assert_eq!(
        (
            &shot["result"]["scale_divisor"],
            &shot["result"]["frame_quality"]
        ),
        (&json!(1), &json!(40))
    );
    assert_eq!(f.last_timing().frame.requests, 1);
    f.observe(sequence + 2).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[1..2]);
    // One pair too far: refused, the pair below is the picture (two requests, once), and the hint is back there.
    let shot = f.observe(sequence + 3).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[..2]);
    assert_eq!(
        (
            &shot["result"]["scale_divisor"],
            &shot["result"]["frame_quality"]
        ),
        (&json!(1), &json!(60))
    );
    assert_eq!(f.last_timing().frame.requests, 2);
    sequence += 4;
    for _ in 1..HINT_HOLD {
        f.observe(sequence).await.unwrap();
        assert_eq!(f.screenshot_requests(), walk()[1..2], "picture {sequence}");
        assert_eq!(f.last_timing().frame.requests, 1);
        sequence += 1;
    }
    // A picture above half the bound never moves the hint, hold or not.
    f.refusals.store(101, Ordering::SeqCst);
    for _ in 0..HINT_HOLD + 2 {
        f.observe(sequence).await.unwrap();
        assert_eq!(f.screenshot_requests(), walk()[1..2], "picture {sequence}");
        sequence += 1;
    }
    // At the first pair there is nothing better to move to.
    f.refusals.store(0, Ordering::SeqCst);
    f.observe(sequence).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[1..2]);
    for sequence in sequence + 1..sequence + 4 {
        f.observe(sequence).await.unwrap();
        assert_eq!(f.screenshot_requests(), walk()[..1]);
    }
}
#[tokio::test]
async fn every_64_helper_pictures_one_png_decides_again_and_a_fitting_one_returns_to_lossless() {
    let f = Fixture::after_large_png(8).await;
    let mut sequence = 3;
    // Two rounds on a screen that stays large: 64 helper pictures, then one PNG, which is above the bound again.
    for round in 0..2 {
        for picture in 0..PNG_PROBE_EVERY {
            let shot = f.observe(sequence).await.unwrap();
            assert_eq!(
                f.screenshot_requests(),
                walk()[..1],
                "round {round} picture {picture}"
            );
            assert!(shot["result"].get("source_mime_type").is_none());
            sequence += 1;
        }
        if round == 0 {
            let shot = f.observe(sequence).await.unwrap();
            assert_eq!(f.screenshot_requests(), [plain_request()]);
            assert_eq!(shot["result"]["source_mime_type"], "image/png");
            let timing = f.last_timing();
            assert!(!timing.frame.helper_jpeg && !timing.frame.passthrough);
            assert_eq!(timing.frame.requests, 1);
            sequence += 1;
        }
    }
    // The screen has become one whose PNG fits (the default fixture's): the PNG due now is passed through, and
    // the session is back on the lossless picture, with the request it always made.
    f.reply_mode.store(0, Ordering::SeqCst);
    for _ in 0..4 {
        let shot = f.observe(sequence).await.unwrap();
        assert_eq!(f.screenshot_requests(), [plain_request()]);
        assert_eq!(shot["result"]["mime_type"], "image/png");
        assert_eq!(shot["result"]["data_base64"], "AA==");
        assert!(f.last_timing().frame.passthrough);
        sequence += 1;
    }
    // Nothing about the controller was concluded from that: a large PNG turns the helper on again.
    f.reply_mode.store(8, Ordering::SeqCst);
    let shot = f.observe(sequence).await.unwrap();
    assert_eq!(f.screenshot_requests(), [plain_request()]);
    assert_eq!(shot["result"]["source_mime_type"], "image/png");
    let shot = f.observe(sequence + 1).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[..1]);
    assert_eq!(shot["result"]["frame_quality"], 80);
    assert!(f.last_timing().frame.helper_jpeg);
}
#[tokio::test]
async fn png_taken_inside_helper_mode_decides_like_any_other_png() {
    // The helper has no frame of the size asked for: the PNG request follows for that picture. It is above the
    // bound, so the helper is asked again next time, and the count towards the next PNG starts over.
    let f = Fixture::after_large_png(8).await;
    let mut sequence = 3;
    for _ in 0..PNG_PROBE_EVERY - 1 {
        f.observe(sequence).await.unwrap();
        sequence += 1;
    }
    f.screenshot_requests();
    f.reply_mode.store(14, Ordering::SeqCst);
    let shot = f.observe(sequence).await.unwrap();
    assert_eq!(
        f.screenshot_requests(),
        [walk()[0].clone(), plain_request()]
    );
    assert_eq!(shot["result"]["source_mime_type"], "image/png");
    assert_eq!(f.last_timing().frame.requests, 2);
    f.reply_mode.store(8, Ordering::SeqCst);
    for picture in 0..PNG_PROBE_EVERY {
        f.observe(sequence + 1 + picture as u64).await.unwrap();
        assert_eq!(f.screenshot_requests(), walk()[..1], "picture {picture}");
    }
    sequence += 1 + PNG_PROBE_EVERY as u64;
    f.observe(sequence).await.unwrap();
    assert_eq!(f.screenshot_requests(), [plain_request()]);
    f.observe(sequence + 1).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[..1]);
    // A failed picture changes nothing: an ordinary refusal of the PNG request, then the helper as before.
    f.reply_mode.store(5, Ordering::SeqCst);
    f.session.state.lock().await.helper_pictures = PNG_PROBE_EVERY;
    let reply = f.observe(sequence + 2).await.unwrap();
    assert_eq!(reply["error"], FRAME_UNAVAILABLE);
    assert_eq!(
        f.screenshot_requests(),
        [plain_request(), walk()[0].clone()]
    );
    f.reply_mode.store(8, Ordering::SeqCst);
    f.observe(sequence + 3).await.unwrap();
    assert_eq!(
        f.screenshot_requests(),
        [plain_request()],
        "the PNG that was due is still due"
    );
    f.observe(sequence + 4).await.unwrap();
    assert_eq!(f.screenshot_requests(), walk()[..1]);
}
#[tokio::test]
async fn scaled_refusals_and_bad_replies_keep_the_typed_reply_and_the_fence_where_they_were() {
    let f = Fixture::after_large_png(8).await;
    // An ordinary refusal of the scaled request (frame not ready, capture busy): the typed reply, after that one
    // request; nothing is remembered about the form.
    f.reply_mode.store(13, Ordering::SeqCst);
    let reply = f.observe(3).await.unwrap();
    assert_eq!(
        reply,
        json!({"sequence":3,"error":FRAME_UNAVAILABLE,"reason":"controller_refused"})
    );
    assert_eq!(f.screenshot_requests(), [scaled_request(3840, 2160, 50)]);
    // No frame fits the size asked for: the PNG path for this picture only.
    f.reply_mode.store(14, Ordering::SeqCst);
    let shot = f.observe(4).await.unwrap();
    assert_eq!(
        f.screenshot_requests(),
        [scaled_request(3840, 2160, 50), plain_request()]
    );
    assert_eq!(shot["result"]["source_mime_type"], "image/png");
    f.reply_mode.store(8, Ordering::SeqCst);
    let shot = f.observe(5).await.unwrap();
    assert_eq!(f.screenshot_requests(), [scaled_request(3840, 2160, 50)]);
    let frame = frame_from_screenshot(&shot["result"]).unwrap();
    // A scaled reply that contradicts itself is a control violation, as a contradicting PNG reply is.
    f.reply_mode.store(12, Ordering::SeqCst);
    let invalid = f.observe(6).await.unwrap_err();
    assert_eq!(code(&invalid), "screenshot_reply_invalid");
    assert!(frame_unavailable(&invalid).is_none());
    assert!(!invalid.to_string().contains("GUEST_MARKER"));
    // The refusal word changes nothing about the error itself: code, text, and what may be logged.
    f.reply_mode.store(10, Ordering::SeqCst);
    let socket = SocketIdentity::read(&f.session.control.path).unwrap();
    let refused = rpc(
        &socket,
        "screenshot",
        scaled_request(1280, 720, 50),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&refused), "controller_refused");
    assert_eq!(
        refused.to_string(),
        "controller rejected operation: \"backend: \\\"GUEST_MARKER_TEXT\\\"\""
    );
    assert_eq!(
        refused
            .downcast_ref::<ScaledRefused>()
            .map(|refused| refused.refusal),
        Some(Refusal::FormatUnavailable)
    );
    assert_eq!(
        frame_unavailable(&refused).map(FailureCode::name),
        Some("controller_refused")
    );
    // The earlier picture's anchor still binds input: none of the above touched the session.
    f.reply_mode.store(0, Ordering::SeqCst);
    let click = f
        .session
        .handle(f.signed(7, Fixture::click(frame)))
        .await
        .unwrap();
    assert_eq!(click["result"]["dispatched"], true);
}

/// Capture-policy tests use the same capture/transform/anchor path without requiring a socket bind.
fn policy_state(helper_mode: bool) -> InputState {
    InputState {
        sequence: 9,
        frames: VecDeque::new(),
        timing: None,
        png_only: false,
        source: helper_mode.then_some((2560, 1440)),
        helper_mode,
        helper_pictures: 0,
        helper_hint: 0,
        helper_hold: 0,
    }
}
fn policy_snapshot(state: &InputState) -> Value {
    json!({"png_only":state.png_only,"source":state.source,"helper_mode":state.helper_mode,
        "helper_pictures":state.helper_pictures,"helper_hint":state.helper_hint,"helper_hold":state.helper_hold,
        "sequence":state.sequence,"frames":state.frames.len()})
}
fn policy_refused(refusal: Refusal) -> anyhow::Error {
    anyhow::Error::new(FailureCode::ControllerRefused).context(ScaledRefused {
        refusal,
        message: "scripted refusal".into(),
    })
}
fn policy_unavailable() -> anyhow::Error {
    FailureCode::ControllerRefused.tag(anyhow::anyhow!("screenshot_unavailable_or_too_large"))
}
async fn policy_capture(
    state: &mut InputState,
    replies: Vec<(Duration, Result<Value>)>,
) -> (Result<(Value, FrameAnchor)>, Vec<Value>, ObserveTiming) {
    let mut replies = VecDeque::from(replies);
    let mut calls = Vec::new();
    let mut timing = ObserveTiming::default();
    let result = capture_picture(VM, state, &mut timing, |arguments, _deadline| {
        calls.push(arguments);
        let (delay, reply) = replies.pop_front().expect("no extra capture attempts");
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            (reply, RpcTiming::default())
        }
    })
    .await;
    assert!(replies.is_empty(), "all scripted requests were made");
    (result, calls, timing)
}
fn policy_jpeg(index: usize) -> Value {
    scripted_jpeg(&Scaled::parse(&walk()[index]).unwrap().unwrap(), true)
}

#[tokio::test(start_paused = true)]
async fn policy_failed_png_probes_fall_back_and_reset_only_on_accepted_picture() {
    for failure in [0, 1, 2] {
        let mut state = policy_state(true);
        state.helper_pictures = PNG_PROBE_EVERY;
        state.helper_hint = 3;
        state.helper_hold = 4;
        let refused = match failure {
            0 => policy_unavailable(),
            1 => FailureCode::ControllerRefused.tag(anyhow::anyhow!("frame_unavailable")),
            _ => policy_refused(Refusal::EncodedSizeLimit),
        };
        let began = tokio::time::Instant::now();
        let (result, calls, timing) = policy_capture(
            &mut state,
            vec![
                (Duration::ZERO, Err(refused)),
                (Duration::ZERO, Ok(policy_jpeg(3))),
            ],
        )
        .await;
        result.unwrap();
        assert_eq!(calls, [plain_request(), walk()[3].clone()]);
        assert_eq!(timing.frame.requests, 2);
        assert!(tokio::time::Instant::now() - began < Duration::from_secs(4));
        assert_eq!(
            (
                state.helper_mode,
                state.helper_pictures,
                state.helper_hint,
                state.helper_hold
            ),
            (true, 0, 3, 3)
        );
        // Exactly another 64 accepted helper pictures before a PNG probe is due.
        for picture in 1..=PNG_PROBE_EVERY {
            let (result, calls, _) =
                policy_capture(&mut state, vec![(Duration::ZERO, Ok(policy_jpeg(3)))]).await;
            result.unwrap();
            assert_eq!(calls, walk()[3..4], "picture {picture}, failure {failure}");
            assert_eq!(state.helper_pictures, picture);
        }
        let (result, calls, _) = policy_capture(
            &mut state,
            vec![
                (Duration::ZERO, Err(policy_unavailable())),
                (Duration::ZERO, Ok(policy_jpeg(3))),
            ],
        )
        .await;
        result.unwrap();
        assert_eq!(calls, [plain_request(), walk()[3].clone()]);
        assert_eq!(state.helper_pictures, 0);
    }
}

#[tokio::test(start_paused = true)]
async fn policy_slow_png_within_deadline_is_accepted_without_helper_request() {
    for probe in [false, true] {
        let mut state = policy_state(probe);
        state.helper_pictures = if probe { PNG_PROBE_EVERY } else { 0 };
        let png = control_frame::tests::stored_png(1, 1, 3, &[0, 0, 0]);
        let reply = control_frame::tests::screenshot(1, 1, &png);
        let began = tokio::time::Instant::now();
        let (result, calls, timing) = policy_capture(
            &mut state,
            vec![(Duration::from_millis(2100), Ok(reply.clone()))],
        )
        .await;
        assert_eq!(result.unwrap().0, reply);
        assert_eq!(calls, [plain_request()]);
        assert_eq!(timing.frame.requests, 1);
        assert!(timing.frame.passthrough);
        assert!(tokio::time::Instant::now() - began >= Duration::from_millis(2100));
        assert!(tokio::time::Instant::now() - began < Duration::from_secs(16));
        assert!(!state.helper_mode);
        assert_eq!(state.helper_pictures, 0);
    }
}

#[tokio::test(start_paused = true)]
async fn policy_png_deadline_never_starts_helper_or_commits_state() {
    for replied in [false, true] {
        let mut state = policy_state(true);
        state.helper_pictures = PNG_PROBE_EVERY;
        state.helper_hint = 3;
        state.helper_hold = 4;
        let before = policy_snapshot(&state);
        let (result, calls, _) = policy_capture(
            &mut state,
            vec![(
                if replied {
                    Duration::ZERO
                } else {
                    Duration::from_secs(17)
                },
                Err(FailureCode::ControllerDeadline.tag(anyhow::anyhow!("capture timed out"))),
            )],
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(code(&error), "controller_deadline");
        assert!(matches!(
            frame_unavailable(&error),
            Some(FailureCode::ControllerDeadline)
        ));
        assert_eq!(calls, [plain_request()]);
        assert_eq!(policy_snapshot(&state), before);
    }
}

#[tokio::test]
async fn policy_blocked_transform_expires_without_committing_and_finishes_in_background() {
    for fallback in [false, true] {
        let mut state = policy_state(true);
        state.helper_pictures = if fallback { PNG_PROBE_EVERY } else { 17 };
        state.helper_hint = 3;
        state.helper_hold = 4;
        let before = policy_snapshot(&state);
        let mut replies = VecDeque::new();
        if fallback {
            replies.push_back(Err(policy_refused(Refusal::EncodedSizeLimit)));
        }
        replies.push_back(Ok(policy_jpeg(3)));
        let mut calls = Vec::new();
        let (release, blocked) = std::sync::mpsc::channel();
        let (finished, completion) = tokio::sync::oneshot::channel();
        let done = Arc::new(AtomicBool::new(false));
        let worker_done = done.clone();
        let began = tokio::time::Instant::now();
        // Inject a short remaining freshness window on fallback, or a short normal observation deadline.
        let mut timing = ObserveTiming {
            entered: Some((began - Duration::from_millis(3980)).into_std()),
            ..ObserveTiming::default()
        };
        let deadline = began
            + if fallback {
                Duration::from_secs(16)
            } else {
                Duration::from_millis(20)
            };
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            capture_picture_with_transform(
                VM,
                &mut state,
                &mut timing,
                |arguments, _deadline| {
                    calls.push(arguments);
                    let reply = replies.pop_front().expect("no extra capture attempts");
                    async move { (reply, RpcTiming::default()) }
                },
                deadline,
                move |shot| {
                    // The test releases the worker only after the capture future has returned.
                    let released = blocked.recv_timeout(Duration::from_millis(100)).is_ok();
                    let result = transform_picture(shot);
                    worker_done.store(true, Ordering::SeqCst);
                    let _ = finished.send(released);
                    result
                },
            ),
        )
        .await;
        let finished_at_return = done.load(Ordering::SeqCst);
        let at_return = policy_snapshot(&state);
        let _ = release.send(());
        let released = completion.await.unwrap();
        let error = result
            .expect("capture must return at its own deadline")
            .unwrap_err();
        assert_eq!(code(&error), "controller_deadline");
        assert!(matches!(
            frame_unavailable(&error),
            Some(FailureCode::ControllerDeadline)
        ));
        assert!(
            !finished_at_return,
            "capture did not wait for the blocking transform"
        );
        assert!(
            released,
            "the detached worker finished after capture returned"
        );
        assert_eq!(at_return, before);
        assert_eq!(policy_snapshot(&state), before);
        assert!(replies.is_empty());
        assert_eq!(
            calls,
            if fallback {
                vec![plain_request(), walk()[3].clone()]
            } else {
                walk()[3..4].to_vec()
            }
        );
    }
}

#[tokio::test]
async fn policy_start_on_over_bound_png_can_accept_a_smaller_helper_picture() {
    let mut state = policy_state(false);
    let mut replies = vec![(Duration::ZERO, Err(policy_unavailable()))];
    replies.extend((0..3).map(|_| {
        (
            Duration::ZERO,
            Err(policy_refused(Refusal::EncodedSizeLimit)),
        )
    }));
    let bootstrap = scaled_request(1920, 1080, 50);
    let jpeg = scripted_jpeg(&Scaled::parse(&bootstrap).unwrap().unwrap(), true);
    replies.push((Duration::ZERO, Ok(jpeg)));
    let (result, calls, timing) = policy_capture(&mut state, replies).await;
    let (result, _) = result.unwrap();
    assert_eq!(
        calls,
        [
            plain_request(),
            walk()[0].clone(),
            walk()[1].clone(),
            walk()[2].clone(),
            bootstrap
        ]
    );
    assert_eq!(timing.frame.requests, 5);
    assert_eq!(result["scale_divisor"], 2);
    assert_eq!(
        (
            state.helper_mode,
            state.helper_pictures,
            state.helper_hint,
            state.helper_hold
        ),
        (true, 0, 3, HINT_HOLD - 1)
    );
    // observe_frame records the accepted source geometry before the next picture.
    state.source = result["width"].as_u64().zip(result["height"].as_u64());
    let (result, calls, _) =
        policy_capture(&mut state, vec![(Duration::ZERO, Ok(policy_jpeg(3)))]).await;
    result.unwrap();
    assert_eq!(calls, walk()[3..4]);
}

#[tokio::test(start_paused = true)]
async fn policy_six_helper_size_refusals_then_failed_png_leave_every_field_unchanged() {
    for timeout in [false, true] {
        let mut state = policy_state(true);
        state.helper_pictures = 17;
        state.helper_hold = 2;
        let before = policy_snapshot(&state);
        let mut replies: Vec<_> = (0..control_frame::HELPER_ATTEMPTS)
            .map(|_| {
                (
                    Duration::ZERO,
                    Err(policy_refused(Refusal::EncodedSizeLimit)),
                )
            })
            .collect();
        replies.push((
            if timeout {
                Duration::from_secs(17)
            } else {
                Duration::ZERO
            },
            Err(policy_unavailable()),
        ));
        let (result, calls, _) = policy_capture(&mut state, replies).await;
        assert_eq!(
            code(&result.unwrap_err()),
            if timeout {
                "controller_deadline"
            } else {
                "controller_refused"
            }
        );
        assert_eq!(
            calls,
            walk()
                .into_iter()
                .chain([plain_request()])
                .collect::<Vec<_>>()
        );
        assert_eq!(policy_snapshot(&state), before);
    }
}

#[tokio::test]
async fn policy_anchor_failure_after_helper_refusal_or_png_transform_changes_nothing() {
    for helper in [false, true] {
        let mut state = policy_state(helper);
        state.helper_pictures = 7;
        state.helper_hold = 2;
        let before = policy_snapshot(&state);
        let mut replies = Vec::new();
        let mut reply = if helper {
            replies.push((
                Duration::ZERO,
                Err(policy_refused(Refusal::EncodedSizeLimit)),
            ));
            policy_jpeg(1)
        } else {
            scripted_png()
        };
        reply["frame_id"] = json!("invalid-anchor");
        replies.push((Duration::ZERO, Ok(reply)));
        let (result, calls, timing) = policy_capture(&mut state, replies).await;
        assert_eq!(code(&result.unwrap_err()), "frame_verification");
        assert!(
            timing.frame.frame_bytes > 0,
            "transform succeeded before the anchor check"
        );
        assert_eq!(
            calls,
            if helper {
                walk()[..2].to_vec()
            } else {
                vec![plain_request()]
            }
        );
        assert_eq!(policy_snapshot(&state), before);
    }
}

#[tokio::test(start_paused = true)]
async fn policy_failed_probe_and_failed_helper_retry_without_policy_changes_or_extra_attempts() {
    for exhausted in [false, true] {
        let mut state = policy_state(true);
        state.helper_pictures = PNG_PROBE_EVERY;
        let before = policy_snapshot(&state);
        for _ in 0..2 {
            let mut replies = vec![(Duration::ZERO, Err(policy_unavailable()))];
            if exhausted {
                replies.extend((0..control_frame::HELPER_ATTEMPTS).map(|_| {
                    (
                        Duration::ZERO,
                        Err(policy_refused(Refusal::EncodedSizeLimit)),
                    )
                }));
            } else {
                replies.push((Duration::from_secs(5), Ok(policy_jpeg(0))));
            }
            let began = tokio::time::Instant::now();
            let (result, calls, _) = policy_capture(&mut state, replies).await;
            assert_eq!(
                code(&result.unwrap_err()),
                if exhausted {
                    "controller_refused"
                } else {
                    "controller_deadline"
                }
            );
            let expected = std::iter::once(plain_request())
                .chain(walk().into_iter().take(if exhausted {
                    control_frame::HELPER_ATTEMPTS
                } else {
                    1
                }))
                .collect::<Vec<_>>();
            assert_eq!(calls, expected);
            assert!(tokio::time::Instant::now() - began <= Duration::from_secs(4));
            assert_eq!(policy_snapshot(&state), before);
        }
        let (result, calls, _) = policy_capture(
            &mut state,
            vec![
                (Duration::ZERO, Err(policy_unavailable())),
                (Duration::ZERO, Ok(policy_jpeg(0))),
            ],
        )
        .await;
        result.unwrap();
        assert_eq!(calls, [plain_request(), walk()[0].clone()]);
        assert_eq!(state.helper_pictures, 0);
    }
}

#[tokio::test]
async fn policy_no_helper_capability_or_invalid_png_reply_does_not_trigger_fallback() {
    for invalid in [false, true] {
        let mut state = policy_state(true);
        state.helper_pictures = PNG_PROBE_EVERY;
        state.png_only = !invalid;
        let before = policy_snapshot(&state);
        let failure = if invalid {
            FailureCode::ScreenshotReplyTooLarge
                .tag(anyhow::anyhow!("reply exceeds the wire bound"))
        } else {
            policy_unavailable()
        };
        let (result, calls, _) =
            policy_capture(&mut state, vec![(Duration::ZERO, Err(failure))]).await;
        assert_eq!(
            code(&result.unwrap_err()),
            if invalid {
                "screenshot_reply_too_large"
            } else {
                "controller_refused"
            }
        );
        assert_eq!(calls, [plain_request()]);
        assert_eq!(policy_snapshot(&state), before);
    }
}
