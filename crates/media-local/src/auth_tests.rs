//! Route/state-machine unit tests: actual P-256 signatures and private registry files,
//! in-memory test identities only. No Keychain, VM, peer, encoder or public listener.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use p256::ecdsa::{signature::Signer, SigningKey};
use tower::ServiceExt;

struct TestIdentity(SigningKey);
impl TestIdentity {
    fn new(scalar: u8) -> Self {
        let mut bytes = [0u8; 32];
        bytes[31] = scalar;
        Self(SigningKey::from_bytes((&bytes).into()).unwrap())
    }
}
impl protocol::Identity for TestIdentity {
    fn public_key(&self) -> Result<String> {
        Ok(protocol::hex(
            self.0.verifying_key().to_encoded_point(false).as_bytes(),
        ))
    }
    fn sign(&self, message: &[u8]) -> Result<String> {
        let signature: p256::ecdsa::Signature = self.0.sign(message);
        Ok(protocol::hex(&signature.to_bytes()))
    }
}
fn sdp(octet: &str) -> String {
    format!(
        "v=0\r\na=fingerprint:sha-256 {}\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=recvonly\r\n",
        vec![octet; 32].join(":")
    )
}
struct Fixture {
    _directory: tempfile::TempDir,
    app: App,
    host: TestIdentity,
    device: TestIdentity,
    grant: protocol::SignedGrant,
}
impl Fixture {
    fn new() -> Self {
        let host = TestIdentity::new(1);
        let device = TestIdentity::new(2);
        let host_public_key = host.public_key().unwrap();
        let grant = protocol::SignedGrant::new(
            protocol::Grant {
                host_id: protocol::identity_id(&host_public_key).unwrap(),
                device_id: protocol::identity_id(&device.public_key().unwrap()).unwrap(),
                device_public_key: device.public_key().unwrap(),
                vm_id: "linux-auth-fixture".into(),
                scopes: vec!["observe".into()],
                version: 1,
            },
            &host,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("registry");
        let registry = protocol::store::Registry {
            host_public_key: host_public_key.clone(),
            grants: BTreeMap::from([(grant.grant.key(), grant.clone())]),
            versions: BTreeMap::from([(grant.grant.key(), 1)]),
        };
        protocol::store::Store::open(&state)
            .unwrap()
            .save(&registry)
            .unwrap();
        let auth = Arc::new(Auth {
            ice_config: None,
            test_identity: None,
            registry_read: std::sync::Mutex::new(()),
            host_public_key: host_public_key.clone(),
            vm_id: grant.grant.vm_id.clone(),
            gate: protocol::SessionGate::new(&host_public_key, std::slice::from_ref(&grant))
                .unwrap(),
            state,
            namespace: "not-used-no-keychain".into(),
            control_state: None,
            files_enabled: false,
            close_proofs: std::sync::Mutex::new(BTreeMap::new()),
        });
        let app = App {
            host: "127.0.0.1:8787".into(),
            origin: "http://127.0.0.1:8787".into(),
            encoder: PathBuf::from("must-not-start-encoder"),
            socket: PathBuf::new(),
            web: PathBuf::new(),
            capabilities: json!({"h265":"verified-hardware","h264":"verified-hardware"}),
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
            _directory: directory,
            app,
            host,
            device,
            grant,
        }
    }
    fn auth(&self) -> Arc<Auth> {
        self.app.auth.clone().unwrap()
    }
    fn signed_offer(&self) -> protocol::SignedOffer {
        let intent = protocol::OfferIntent {
            challenge: self
                .auth()
                .gate
                .challenge(
                    &self.grant.grant.device_id,
                    &self.grant.grant.vm_id,
                    vec!["observe".into()],
                    now().unwrap(),
                )
                .unwrap(),
            request_id: uuid::Uuid::new_v4().to_string(),
            codec: "h265".into(),
            offer: protocol::SdpBinding::from_sdp(&sdp("11")).unwrap(),
        };
        protocol::SignedOffer {
            signature: self.device.sign(&intent.canonical().unwrap()).unwrap(),
            intent,
        }
    }
    fn pending(&self) -> protocol::SignedAnswer {
        self.pending_with_request(None)
    }
    fn pending_with_request(&self, request_id: Option<&str>) -> protocol::SignedAnswer {
        let mut offer = self.signed_offer();
        if let Some(request_id) = request_id {
            offer.intent.request_id = request_id.to_owned();
            offer.signature = self
                .device
                .sign(&offer.intent.canonical().unwrap())
                .unwrap();
        }
        let auth = self.auth();
        let accepted = auth
            .gate
            .accept_offer(&offer, &sdp("11"), now().unwrap())
            .unwrap();
        let answer = auth
            .gate
            .answer(
                accepted,
                uuid::Uuid::new_v4().to_string(),
                &sdp("22"),
                &self.host,
                now().unwrap(),
            )
            .unwrap();
        // Mirrors the retained public proof after production sign_answer; private-key
        // adapter access is covered by the separate actual Keychain integration test.
        auth.close_proofs.lock().unwrap().insert(
            answer.transcript.session_id.clone(),
            CloseProof {
                transcript: answer.transcript.clone(),
                device_public_key: self.device.public_key().unwrap(),
                issued: Instant::now(),
            },
        );
        answer
    }
    fn confirmation(&self, answer: &protocol::SignedAnswer) -> protocol::Confirmation {
        let digest = answer.transcript.digest().unwrap();
        protocol::Confirmation {
            signature: self
                .device
                .sign(&protocol::confirmation_bytes(&digest).unwrap())
                .unwrap(),
            transcript_sha256: digest,
        }
    }
    fn signed_close(&self, answer: &protocol::SignedAnswer) -> protocol::SignedClose {
        let digest = answer.transcript.digest().unwrap();
        protocol::SignedClose {
            session_id: answer.transcript.session_id.clone(),
            signature: self
                .device
                .sign(&protocol::close_bytes(&answer.transcript.session_id, &digest).unwrap())
                .unwrap(),
            transcript_sha256: digest,
        }
    }
    fn revoke_registry(&self) {
        let auth = self.auth();
        let store = protocol::store::Store::open(&auth.state).unwrap();
        let mut registry = store.load().unwrap();
        registry
            .revoke(&self.grant.grant.device_id, &self.grant.grant.vm_id)
            .unwrap();
        store.save(&registry).unwrap();
    }
    async fn install_session(
        &self,
        answer: &protocol::SignedAnswer,
    ) -> (
        watch::Receiver<bool>,
        watch::Receiver<bool>,
        watch::Sender<Option<bool>>,
    ) {
        let (cancel, cancelled) = watch::channel(false);
        let (confirmed, confirmation) = watch::channel(false);
        let (done, finished) = watch::channel(None);
        self.app.active.lock().await.active = Some(Session {
            request: answer.transcript.intent.request_id.clone(),
            id: answer.transcript.session_id.clone(),
            cancel,
            finished,
            confirmed,
            input: None,
            files: None,
        });
        (cancelled, confirmation, done)
    }
    async fn complete(&self, answer: &protocol::SignedAnswer, clean: bool) {
        self.auth().gate.close(&answer.transcript.session_id);
        let mut book = self.app.active.lock().await;
        book.active = None;
        book.completed.insert(
            answer.transcript.intent.request_id.clone(),
            (Instant::now(), clean, answer.transcript.session_id.clone()),
        );
    }
    async fn post(&self, path: &str, body: Value, origin: Option<&str>) -> (StatusCode, Vec<u8>) {
        let router = Router::new()
            .route("/offer", post(crate::offer))
            .route("/close", post(crate::close))
            .route("/auth/challenge", post(super::challenge))
            .route("/auth/offer", post(super::offer))
            .route("/auth/confirm", post(super::confirm))
            .route("/auth/close", post(super::close))
            .with_state(self.app.clone());
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, &self.app.host)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin)
        }
        let response = router
            .oneshot(
                request
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, body.to_vec())
    }
}

#[tokio::test]
async fn authenticated_router_rejects_unsigned_and_foreign_origin_without_reservation() {
    let f = Fixture::new();
    let origin = f.app.origin.clone();
    let offer = json!({"request_id":uuid::Uuid::new_v4().to_string(),"sdp":sdp("11"),"type":"offer","codec":"h265"});
    let close = json!({"request_id":uuid::Uuid::new_v4().to_string(),"session_id":"untrusted"});
    assert_eq!(
        f.post("/offer", offer.clone(), Some(&origin)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.post("/close", close, Some(&origin)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.post("/auth/offer", offer, Some(&origin)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let challenge = json!({"device_id":f.grant.grant.device_id,"vm_id":f.grant.grant.vm_id,"scopes":["observe"]});
    for invalid_origin in [None, Some("https://attacker.invalid")] {
        assert_eq!(
            f.post("/auth/challenge", challenge.clone(), invalid_origin)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        f.post("/auth/challenge", challenge, Some(&origin)).await.0,
        StatusCode::OK
    );
    assert!(f.app.active.lock().await.active.is_none());
    assert!(f.app.active.lock().await.cancelled.is_empty());
}

#[tokio::test]
async fn challenge_scope_vm_and_invalid_offer_signature_are_denied_before_peer() {
    let f = Fixture::new();
    let origin = f.app.origin.clone();
    for (vm, scopes) in [
        ("other-vm", vec!["observe"]),
        ("linux-auth-fixture", vec!["input"]),
        ("linux-auth-fixture", vec!["input", "observe"]),
    ] {
        assert_eq!(
            f.post(
                "/auth/challenge",
                json!({"device_id":f.grant.grant.device_id,"vm_id":vm,"scopes":scopes}),
                Some(&origin)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let mut offer = f.signed_offer();
    offer.signature = TestIdentity::new(3)
        .sign(&offer.intent.canonical().unwrap())
        .unwrap();
    assert_eq!(
        f.post(
            "/auth/offer",
            json!({"signed_offer":offer,"sdp":sdp("11")}),
            Some(&origin)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(f.app.active.lock().await.active.is_none());
}

#[tokio::test]
async fn signed_close_proof_survives_completed_peer_and_revoked_grant() {
    let f = Fixture::new();
    let answer = f.pending();
    let signed = f.signed_close(&answer);
    f.complete(&answer, true).await;
    assert!(f
        .auth()
        .gate
        .authorize_session(&answer.transcript.session_id, "observe")
        .is_err());
    f.revoke_registry();
    f.auth().reload().unwrap();
    let (status, body) = f
        .post(
            "/auth/close",
            serde_json::to_value(&signed).unwrap(),
            Some(&f.app.origin),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"closed":true})
    );
    // Retry is a receipt for the same completed session, not newly granted access.
    assert_eq!(
        f.post(
            "/auth/close",
            serde_json::to_value(&signed).unwrap(),
            Some(&f.app.origin)
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn tampered_foreign_and_expired_close_proofs_never_acknowledge() {
    let f = Fixture::new();
    let answer = f.pending();
    let valid = f.signed_close(&answer);
    f.complete(&answer, true).await;
    let mut variants = Vec::new();
    let mut bad = valid.clone();
    bad.session_id = uuid::Uuid::new_v4().to_string();
    variants.push(bad);
    let mut bad = valid.clone();
    bad.transcript_sha256 = "00".repeat(32);
    variants.push(bad);
    let mut bad = valid.clone();
    bad.signature = TestIdentity::new(3)
        .sign(&protocol::close_bytes(&bad.session_id, &bad.transcript_sha256).unwrap())
        .unwrap();
    variants.push(bad);
    for invalid in variants {
        assert_eq!(
            f.post(
                "/auth/close",
                serde_json::to_value(invalid).unwrap(),
                Some(&f.app.origin)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        f.post(
            "/auth/close",
            serde_json::to_value(&valid).unwrap(),
            Some("https://attacker.invalid")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    f.auth()
        .close_proofs
        .lock()
        .unwrap()
        .get_mut(&valid.session_id)
        .unwrap()
        .issued = Instant::now() - Duration::from_secs(241);
    assert_eq!(
        f.post(
            "/auth/close",
            serde_json::to_value(valid).unwrap(),
            Some(&f.app.origin)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn signed_close_never_turns_missing_cleanup_attestation_into_success() {
    let f = Fixture::new();
    let answer = f.pending();
    let signed = f.signed_close(&answer);
    let (cancelled, _, done) = f.install_session(&answer).await;
    drop(done);
    let (status, body) = f
        .post(
            "/auth/close",
            serde_json::to_value(&signed).unwrap(),
            Some(&f.app.origin),
        )
        .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(*cancelled.borrow());
    assert!(!String::from_utf8_lossy(&body).contains("\"closed\":true"));
    f.complete(&answer, false).await;
    assert_eq!(
        f.post(
            "/auth/close",
            serde_json::to_value(&signed).unwrap(),
            Some(&f.app.origin)
        )
        .await
        .0,
        StatusCode::GATEWAY_TIMEOUT
    );
    // Losing the bounded completion record must fail closed, not infer cleanup.
    f.app.active.lock().await.completed.clear();
    assert_eq!(
        f.post(
            "/auth/close",
            serde_json::to_value(&signed).unwrap(),
            Some(&f.app.origin)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn expired_close_proof_can_retry_only_exact_unresolved_cleanup_after_revocation() {
    let f = Fixture::new();
    let answer = f.pending();
    let signed = f.signed_close(&answer);
    f.complete(&answer, false).await;
    f.auth()
        .close_proofs
        .lock()
        .unwrap()
        .get_mut(&signed.session_id)
        .unwrap()
        .issued = Instant::now() - Duration::from_secs(3600);
    f.revoke_registry();
    f.auth().reload().unwrap();
    for _ in 0..2 {
        let (status, body) = f
            .post(
                "/auth/close",
                serde_json::to_value(&signed).unwrap(),
                Some(&f.app.origin),
            )
            .await;
        // Authorization reaches cleanup rather than failing at proof expiry,
        // but no Guest proof exists in this fixture, so it cannot claim success.
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(!String::from_utf8_lossy(&body).contains("\"closed\":true"));
        assert!(f
            .app
            .active
            .lock()
            .await
            .cancelled
            .contains_key(&answer.transcript.intent.request_id));
        assert!(f
            .auth()
            .gate
            .authorize_session(&signed.session_id, "observe")
            .is_err());
        assert!(f
            .auth()
            .gate
            .authorize_session(&signed.session_id, "input")
            .is_err());
    }
}

#[tokio::test]
async fn unresolved_cleanup_does_not_waive_signature_or_exact_expired_proof_binding() {
    for variant in [
        "request",
        "session",
        "missing",
        "clean",
        "digest",
        "signature",
    ] {
        let f = Fixture::new();
        let answer = f.pending();
        let mut signed = f.signed_close(&answer);
        f.complete(&answer, false).await;
        f.auth()
            .close_proofs
            .lock()
            .unwrap()
            .get_mut(&signed.session_id)
            .unwrap()
            .issued = Instant::now() - Duration::from_secs(3600);
        {
            let mut book = f.app.active.lock().await;
            let request = &answer.transcript.intent.request_id;
            match variant {
                "request" => {
                    let entry = book.completed.remove(request).unwrap();
                    book.completed
                        .insert(uuid::Uuid::new_v4().to_string(), entry);
                }
                "session" => {
                    book.completed.get_mut(request).unwrap().2 = uuid::Uuid::new_v4().to_string()
                }
                "missing" => book.completed.clear(),
                "clean" => book.completed.get_mut(request).unwrap().1 = true,
                "digest" => signed.transcript_sha256 = "00".repeat(32),
                "signature" => {
                    signed.signature = TestIdentity::new(3)
                        .sign(
                            &protocol::close_bytes(&signed.session_id, &signed.transcript_sha256)
                                .unwrap(),
                        )
                        .unwrap()
                }
                _ => unreachable!(),
            }
        }
        let (status, body) = f
            .post(
                "/auth/close",
                serde_json::to_value(&signed).unwrap(),
                Some(&f.app.origin),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "variant {variant}");
        assert!(!String::from_utf8_lossy(&body).contains("\"closed\":true"));
        assert!(
            f.app.active.lock().await.cancelled.is_empty(),
            "invalid proof reached cleanup: {variant}"
        );
        assert!(
            f.auth()
                .close_proofs
                .lock()
                .unwrap()
                .get(&answer.transcript.session_id)
                .unwrap()
                .issued
                .elapsed()
                >= Duration::from_secs(3600),
            "invalid proof renewed receipt: {variant}"
        );
    }
}

#[tokio::test]
async fn recovered_close_receipt_is_retryable_when_recovery_acknowledgement_is_lost() {
    let f = Fixture::new();
    let answer = f.pending();
    let signed = f.signed_close(&answer);
    f.complete(&answer, false).await;
    f.auth()
        .close_proofs
        .lock()
        .unwrap()
        .get_mut(&signed.session_id)
        .unwrap()
        .issued = Instant::now() - Duration::from_secs(3600);
    f.revoke_registry();
    f.auth().reload().unwrap();
    let payload = serde_json::to_value(&signed).unwrap();
    assert_eq!(
        f.post("/auth/close", payload.clone(), Some(&f.app.origin))
            .await
            .0,
        StatusCode::GATEWAY_TIMEOUT
    );
    // This test supplies only the state-machine's positive cleanup result;
    // it does not simulate or claim a Guest recovery proof. The caller loses
    // the first successful acknowledgement and retries the exact same signature.
    f.complete(&answer, true).await;
    let discarded_reply = f
        .post("/auth/close", payload.clone(), Some(&f.app.origin))
        .await;
    assert_eq!(discarded_reply.0, StatusCode::OK);
    let (status, body) = f.post("/auth/close", payload, Some(&f.app.origin)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"closed":true})
    );
    assert!(f
        .auth()
        .gate
        .authorize_session(&signed.session_id, "observe")
        .is_err());
    assert!(f
        .auth()
        .gate
        .authorize_session(&signed.session_id, "input")
        .is_err());
}

#[tokio::test]
async fn wrong_confirmation_cannot_start_encoder_watch_and_valid_retry_is_idempotent() {
    let f = Fixture::new();
    let answer = f.pending();
    let (_, confirmed, _done) = f.install_session(&answer).await;
    let valid = f.confirmation(&answer);
    let mut invalid = valid.clone();
    invalid.signature = TestIdentity::new(3)
        .sign(&protocol::confirmation_bytes(&invalid.transcript_sha256).unwrap())
        .unwrap();
    let body = |confirmation: protocol::Confirmation| json!({"session_id":answer.transcript.session_id,"confirmation":confirmation});
    assert_eq!(
        f.post("/auth/confirm", body(invalid), Some(&f.app.origin))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(!*confirmed.borrow());
    assert!(f
        .auth()
        .gate
        .authorize_active(&answer.transcript.session_id, "observe")
        .is_err());
    assert_eq!(
        f.post(
            "/auth/confirm",
            body(valid.clone()),
            Some("https://attacker.invalid")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(!*confirmed.borrow());
    assert_eq!(
        f.post("/auth/confirm", body(valid.clone()), Some(&f.app.origin))
            .await
            .0,
        StatusCode::OK
    );
    assert!(*confirmed.borrow());
    assert_eq!(
        f.post("/auth/confirm", body(valid), Some(&f.app.origin))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn pending_registry_revocation_ends_authorization_watch_and_blocks_confirmation() {
    let f = Fixture::new();
    let answer = f.pending();
    let (_, confirmed, _done) = f.install_session(&answer).await;
    let auth = f.auth();
    auth.gate
        .authorize_session(&answer.transcript.session_id, "observe")
        .unwrap();
    let watch = watch_authorization(Some(auth.clone()), &answer.transcript.session_id, false);
    tokio::pin!(watch);
    assert!(tokio::time::timeout(Duration::from_millis(30), &mut watch)
        .await
        .is_err());
    f.revoke_registry();
    assert!(tokio::time::timeout(Duration::from_secs(1), &mut watch)
        .await
        .expect("revocation watch must terminate")
        .is_err());
    assert!(auth
        .gate
        .authorize_session(&answer.transcript.session_id, "observe")
        .is_err());
    assert_eq!(f.post("/auth/confirm",json!({"session_id":answer.transcript.session_id,"confirmation":f.confirmation(&answer)}),Some(&f.app.origin)).await.0,StatusCode::FORBIDDEN);
    assert!(!*confirmed.borrow());
}

#[test]
fn duplicate_and_partial_auth_cli_config_fail_before_keychain_access() {
    let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(from_args(&[]).unwrap().is_none());
    assert!(from_args(&strings(&[
        "--fault-drop-first-keyframe",
        "--fault-suppress-hevc-frames"
    ]))
    .unwrap()
    .is_none());
    for args in [
        vec!["--auth-state"],
        vec!["--auth-state", "/missing"],
        vec!["--auth-namespace", "unused"],
        vec!["--vm", "linux"],
        vec!["--auth-state", "/missing", "--auth-namespace", "unused"],
        vec!["--auth-state", "one", "--auth-state", "two"],
        vec!["--auth-namespace", "one", "--auth-namespace", "two"],
        vec!["--vm", "one", "--vm", "two"],
        vec!["--ice-config", "/missing"],
        vec!["--ice-config", "one", "--ice-config", "two"],
        vec!["--unknown"],
    ] {
        assert!(
            from_args(&strings(&args)).is_err(),
            "accepted incomplete/duplicate options: {args:?}"
        );
    }
}

#[tokio::test]
async fn signed_active_close_waits_for_positive_cleanup_attestation() {
    let f = Fixture::new();
    let answer = f.pending();
    let signed = f.signed_close(&answer);
    let (mut cancelled, _, done) = f.install_session(&answer).await;
    let response = f.post(
        "/auth/close",
        serde_json::to_value(&signed).unwrap(),
        Some(&f.app.origin),
    );
    tokio::pin!(response);
    tokio::select! {
        reply = &mut response => panic!("close replied before cleanup: {}", reply.0),
        result = cancelled.changed() => result.unwrap(),
        _ = tokio::time::sleep(Duration::from_secs(1)) => panic!("close failed to request cancellation"),
    }
    assert!(*cancelled.borrow());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut response)
            .await
            .is_err()
    );
    done.send(Some(true)).unwrap();
    let (status, body) = tokio::time::timeout(Duration::from_secs(1), response)
        .await
        .expect("confirmed cleanup should release close response");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"closed":true})
    );
}

#[tokio::test]
async fn old_signed_close_cannot_borrow_new_sessions_cleanup_after_request_id_reuse() {
    let f = Fixture::new();
    let old = f.pending();
    let old_close = f.signed_close(&old);
    let request_id = old.transcript.intent.request_id.clone();
    f.complete(&old, false).await;
    assert_eq!(
        f.post(
            "/auth/close",
            serde_json::to_value(&old_close).unwrap(),
            Some(&f.app.origin)
        )
        .await
        .0,
        StatusCode::GATEWAY_TIMEOUT
    );

    // Unresolved cleanup must survive the old 60s pruning boundary. Only a
    // later positive cleanup result makes this request eligible for expiry.
    {
        let mut book = f.app.active.lock().await;
        book.completed.get_mut(&request_id).unwrap().0 = Instant::now() - Duration::from_secs(61);
        book.prune();
        assert!(!book.completed.get(&request_id).unwrap().1);
    }
    f.complete(&old, true).await;
    // Clean receipts expire at 60s while signed close proofs live for 240s.
    // Simulate both elapsed receipt/tombstone clocks, preserving the valid proof.
    {
        let mut book = f.app.active.lock().await;
        book.completed.get_mut(&request_id).unwrap().0 = Instant::now() - Duration::from_secs(61);
        *book.cancelled.get_mut(&request_id).unwrap() = Instant::now() - Duration::from_secs(61);
        book.prune();
        assert!(!book.completed.contains_key(&request_id));
        assert!(!book.cancelled.contains_key(&request_id));
    }
    assert!(f
        .auth()
        .close_proofs
        .lock()
        .unwrap()
        .contains_key(&old_close.session_id));

    // A fresh challenge/signature legitimately reuses the now-available request ID.
    // Its different session ID and nonce must keep its cleanup receipt separate.
    let new = f.pending_with_request(Some(&request_id));
    let new_close = f.signed_close(&new);
    assert_eq!(
        old.transcript.intent.request_id,
        new.transcript.intent.request_id
    );
    assert_ne!(old.transcript.session_id, new.transcript.session_id);
    assert_ne!(
        old.transcript.intent.challenge.nonce,
        new.transcript.intent.challenge.nonce
    );
    f.complete(&new, true).await;

    let (status, body) = f
        .post(
            "/auth/close",
            serde_json::to_value(&old_close).unwrap(),
            Some(&f.app.origin),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!String::from_utf8_lossy(&body).contains("\"closed\":true"));
    let (status, body) = f
        .post(
            "/auth/close",
            serde_json::to_value(&new_close).unwrap(),
            Some(&f.app.origin),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"closed":true})
    );
}

#[tokio::test]
async fn shutdown_fences_fresh_signed_offer_after_previous_session_releases_slot() {
    let f = Fixture::new();
    let old = f.pending();
    let fresh = f.signed_offer();
    let (cancelled, _, done) = f.install_session(&old).await;
    let finished = crate::begin_shutdown(&f.app).await;
    assert!(*cancelled.borrow());
    assert!(f.app.active.lock().await.stopping);
    // This is the previously vulnerable interval: old session has released the
    // slot, while the graceful-shutdown waiter has not yet resumed.
    f.complete(&old, true).await;
    done.send(Some(true)).unwrap();
    let (receipt, wait) = tokio::join!(
        f.post(
            "/auth/offer",
            json!({"signed_offer":fresh,"sdp":sdp("11")}),
            Some(&f.app.origin)
        ),
        crate::await_shutdown_cleanup(finished, Duration::from_millis(50))
    );
    wait.unwrap();
    assert_eq!(receipt.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(String::from_utf8_lossy(&receipt.1).contains("service stopping"));
    assert!(f.app.active.lock().await.active.is_none());
    // Authentication succeeded/consumed the one-use challenge before admission
    // refused it; this does not create a replayable token or pending session.
    assert!(f
        .auth()
        .gate
        .accept_offer(&fresh, &sdp("11"), now().unwrap())
        .is_err());
}

#[test]
fn concurrent_registry_readers_serialize_but_external_writer_still_fails_closed() {
    let f = Fixture::new();
    let auth = f.auth();
    // Force overlap: one legitimate in-process reader owns the mutex and Store
    // flock while other OS threads enter reload. They must wait, not report busy.
    let reader = auth.registry_read.lock().unwrap();
    let store = protocol::store::Store::open(&auth.state).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let (tx, rx) = std::sync::mpsc::channel();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let auth = auth.clone();
            let barrier = barrier.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..32 {
                    if let Err(error) = auth.reload() {
                        tx.send(Err(error.to_string())).unwrap();
                        return;
                    }
                }
                tx.send(Ok(())).unwrap();
            })
        })
        .collect();
    barrier.wait();
    let before_release = rx.recv_timeout(Duration::from_millis(100));
    drop(store);
    drop(reader);
    let results: Vec<_> = (0..8)
        .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(matches!(
        before_release,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    // An independent administrator/process holds flock without this Auth mutex.
    // Do not retry, bypass, or broaden the lock: the authorization check must fail.
    let external_writer = protocol::store::Store::open(&auth.state).unwrap();
    assert!(auth
        .reload()
        .unwrap_err()
        .to_string()
        .contains("registry busy"));
    drop(external_writer);
    auth.reload().unwrap();
}

#[tokio::test]
async fn confirmed_retry_preserves_adapter_identity_and_revalidates_signature_and_grant() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut f = Fixture::new();
    let dir = f._directory.path().canonicalize().unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let control = tokio::net::UnixListener::bind(dir.join("control.sock")).unwrap();
    let console = tokio::net::UnixListener::bind(dir.join("console.sock")).unwrap();
    for path in [dir.join("control.sock"), dir.join("console.sock")] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    Arc::get_mut(f.app.auth.as_mut().unwrap())
        .unwrap()
        .control_state = Some(dir.clone());
    f.app.socket = dir.join("console.sock");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests = calls.clone();
    let console_path = f.app.socket.clone();
    let vm = f.auth().vm_id.clone();
    let worker = tokio::spawn(async move {
        loop {
            let (socket, _) = control.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let reply = match request["method"].as_str().unwrap() {
                "status" => {
                    requests.fetch_add(1, Ordering::SeqCst);
                    json!({"config":{"id":vm,"backend":"qemu-hvf"},"state":"running","capabilities":{"input":true},"runtime":{"console_socket":console_path},"pid":123})
                }
                _ => panic!("unexpected controller fixture request"),
            };
            reader
                .get_mut()
                .write_all(format!("{}\n", json!({"result":reply})).as_bytes())
                .await
                .unwrap();
        }
    });
    let answer = f.pending();
    let (_, confirmed, _done) = f.install_session(&answer).await;
    let confirmation = f.confirmation(&answer);
    let body = |confirmation: protocol::Confirmation| json!({"session_id":answer.transcript.session_id,"confirmation":confirmation});
    assert_eq!(
        f.post(
            "/auth/confirm",
            body(confirmation.clone()),
            Some(&f.app.origin)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(*confirmed.borrow());
    let original = f
        .app
        .active
        .lock()
        .await
        .active
        .as_ref()
        .unwrap()
        .input
        .clone()
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            f.post(
                "/auth/confirm",
                body(confirmation.clone()),
                Some(&f.app.origin)
            )
            .await
            .0,
            StatusCode::OK
        );
        let retained = f
            .app
            .active
            .lock()
            .await
            .active
            .as_ref()
            .unwrap()
            .input
            .clone()
            .unwrap();
        assert!(
            Arc::ptr_eq(&original, &retained),
            "confirmation retry replaced the input owner"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "retry must not attach a new controller session"
    );
    let mut invalid = confirmation.clone();
    invalid.signature = TestIdentity::new(3)
        .sign(&protocol::confirmation_bytes(&invalid.transcript_sha256).unwrap())
        .unwrap();
    assert_eq!(
        f.post("/auth/confirm", body(invalid), Some(&f.app.origin))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    f.revoke_registry();
    assert_eq!(
        f.post("/auth/confirm", body(confirmation), Some(&f.app.origin))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(f
        .auth()
        .gate
        .authorize_active(&answer.transcript.session_id, "observe")
        .is_err());
    original.end();
    worker.abort();
    drop(console);
}

#[tokio::test]
async fn failed_adapter_attach_revokes_and_cancels_session_without_confirming() {
    let mut f = Fixture::new();
    Arc::get_mut(f.app.auth.as_mut().unwrap())
        .unwrap()
        .control_state = Some(f._directory.path().join("unavailable-controller"));
    let answer = f.pending();
    let (cancelled, confirmed, _done) = f.install_session(&answer).await;
    let body =
        json!({"session_id":answer.transcript.session_id,"confirmation":f.confirmation(&answer)});
    assert_eq!(
        f.post("/auth/confirm", body.clone(), Some(&f.app.origin))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert!(*cancelled.borrow());
    assert!(!*confirmed.borrow());
    assert!(f
        .app
        .active
        .lock()
        .await
        .active
        .as_ref()
        .unwrap()
        .input
        .is_none());
    assert!(f
        .auth()
        .gate
        .authorize_session(&answer.transcript.session_id, "observe")
        .is_err());
    assert_eq!(
        f.post("/auth/confirm", body, Some(&f.app.origin)).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn plaintext_control_route_is_absent_even_for_authenticated_origin() {
    let f = Fixture::new();
    assert_eq!(
        f.post("/auth/control", json!({}), Some(&f.app.origin))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn closing_dispatch_during_confirm_attach_revokes_before_book_unlock() {
    use crate::control_channel::ControlDispatch;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    // Real route + real SessionInput attach + real ConsoleControlDispatch::end.
    // Only the private controller's capabilities/status replies are fixtures;
    // no Guest, Keychain, peer or encoder is claimed by this test.
    let mut f = Fixture::new();
    let dir = f._directory.path().canonicalize().unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let controller = tokio::net::UnixListener::bind(dir.join("control.sock")).unwrap();
    let _console = tokio::net::UnixListener::bind(dir.join("console.sock")).unwrap();
    for path in [dir.join("control.sock"), dir.join("console.sock")] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    Arc::get_mut(f.app.auth.as_mut().unwrap())
        .unwrap()
        .control_state = Some(dir.clone());
    f.app.socket = dir.join("console.sock");
    let vm = f.auth().vm_id.clone();
    let console = f.app.socket.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let (socket, _) = controller.accept().await.unwrap();
        let mut reader = tokio::io::BufReader::new(socket);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "status");
        entered_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        reader.get_mut().write_all(format!("{}\n",json!({"result":{"config":{"id":vm,"backend":"qemu-hvf"},"state":"running","capabilities":{"input":true},"runtime":{"console_socket":console},"pid":123}})).as_bytes()).await.unwrap();
    });
    let answer = f.pending();
    let id = answer.transcript.session_id.clone();
    let (cancelled, confirmed, _done) = f.install_session(&answer).await;
    let confirm = f.post(
        "/auth/confirm",
        json!({"session_id":id,"confirmation":f.confirmation(&answer)}),
        Some(&f.app.origin),
    );
    let race = async {
        tokio::time::timeout(Duration::from_secs(2), entered_rx)
            .await
            .unwrap()
            .unwrap();
        f.auth().gate.authorize_active(&id, "observe").unwrap();
        assert!(
            f.app.active.try_lock().is_err(),
            "confirm must hold session book while attach awaits status"
        );
        let dispatch = crate::ConsoleControlDispatch {
            app: f.app.clone(),
            id: id.clone(),
        };
        let ending = tokio::spawn(async move { dispatch.end().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while f.auth().gate.authorize_active(&id, "observe").is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("authority must be revoked without waiting for the session book");
        assert!(
            !ending.is_finished(),
            "end must still wait for attach to relinquish the session book"
        );
        assert!(!*confirmed.borrow());
        resume_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), ending)
            .await
            .expect("cleanup must finish after attach resolves")
            .unwrap()
            .unwrap();
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(confirm, race)
    })
    .await
    .unwrap();
    assert_eq!(response.0, StatusCode::CONFLICT);
    assert!(*cancelled.borrow());
    assert!(!*confirmed.borrow());
    assert!(f.auth().gate.authorize_session(&id, "observe").is_err());
    assert!(f
        .app
        .active
        .lock()
        .await
        .active
        .as_ref()
        .unwrap()
        .input
        .is_none());
    worker.await.unwrap();
}

fn enable_ice(f: &mut Fixture) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let now = now().unwrap();
    let path = f._directory.path().join("ice.json");
    let config = protocol::ice::IceConfig {
        version: 1,
        transport_policy: "relay".into(),
        issued_at: now,
        expires_at: now + 600,
        ice_servers: vec![protocol::ice::IceServer {
            urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
            username: "fixture-user".into(),
            credential: "not-a-real-credential-secret".into(),
        }],
    };
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap()
        .write_all(&serde_json::to_vec(&config).unwrap())
        .unwrap();
    let auth = Arc::get_mut(f.app.auth.as_mut().unwrap()).unwrap();
    auth.ice_config = Some(Arc::new(
        crate::ice_config::IceSource::open(path.clone(), now).unwrap(),
    ));
    auth.test_identity = Some(Arc::new(TestIdentity::new(1)));
    path
}
/// Synthetic clock only: the credential is issued at 100 and expires at 700, and the
/// admission and deadline checks run on that same clock.
fn synthetic_ice(f: &mut Fixture) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = f._directory.path().join("ice-synthetic.json");
    let config = protocol::ice::IceConfig {
        version: 1,
        transport_policy: "relay".into(),
        issued_at: 100,
        expires_at: 700,
        ice_servers: vec![protocol::ice::IceServer {
            urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
            username: "fixture-user".into(),
            credential: "not-a-real-credential-secret".into(),
        }],
    };
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap()
        .write_all(&serde_json::to_vec(&config).unwrap())
        .unwrap();
    let auth = Arc::get_mut(f.app.auth.as_mut().unwrap()).unwrap();
    auth.ice_config = Some(Arc::new(
        crate::ice_config::IceSource::open(path, 100).unwrap(),
    ));
}
fn ice_request(f: &Fixture) -> protocol::ice::SignedIceRequest {
    let request = protocol::ice::IceRequest {
        challenge: f
            .auth()
            .gate
            .challenge(
                &f.grant.grant.device_id,
                &f.grant.grant.vm_id,
                vec!["observe".into()],
                now().unwrap(),
            )
            .unwrap(),
        request_nonce: protocol::random32(),
    };
    protocol::ice::SignedIceRequest {
        signature: f.device.sign(&request.canonical().unwrap()).unwrap(),
        request,
    }
}
async fn call_ice(f: &Fixture, signed: &protocol::ice::SignedIceRequest, origin: &str) -> Response {
    Router::new()
        .route("/auth/ice-config", post(super::ice_configuration))
        .with_state(f.app.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/ice-config")
                .header(header::HOST, &f.app.host)
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(signed).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}
#[tokio::test]
async fn signed_ice_route_is_bound_single_use_private_and_no_store() {
    let mut f = Fixture::new();
    enable_ice(&mut f);
    let signed = ice_request(&f);
    let foreign = call_ice(&f, &signed, "http://foreign.invalid").await;
    assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
    let mut bad = signed.clone();
    bad.signature = "00".repeat(64);
    assert_eq!(
        call_ice(&f, &bad, &f.app.origin).await.status(),
        StatusCode::FORBIDDEN
    );
    let response = call_ice(&f, &signed, &f.app.origin).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = to_bytes(response.into_body(), 16384).await.unwrap();
    let answer: protocol::ice::SignedIceConfig = serde_json::from_slice(&bytes).unwrap();
    answer
        .verify(
            &f.host.public_key().unwrap(),
            &signed.request,
            now().unwrap(),
        )
        .unwrap();
    let replay = call_ice(&f, &signed, &f.app.origin).await;
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(replay.into_body(), 16384).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("credential"));
    let fresh = ice_request(&f);
    f.revoke_registry();
    assert_eq!(
        call_ice(&f, &fresh, &f.app.origin).await.status(),
        StatusCode::FORBIDDEN
    );
    let headers = HeaderMap::from_iter([(header::HOST, f.app.host.parse().unwrap())]);
    let cap = crate::get_capabilities(State(f.app.clone()), headers).await;
    let value =
        String::from_utf8(to_bytes(cap.into_body(), 16384).await.unwrap().to_vec()).unwrap();
    assert!(value.contains("ice_config_enabled"));
    assert!(
        !value.contains("fixture-user")
            && !value.contains("not-a-real-credential-secret")
            && !value.contains("3478")
    );
}
#[tokio::test]
async fn signed_ice_rate_limit_and_expired_file_fail_closed() {
    let mut f = Fixture::new();
    let path = enable_ice(&mut f);
    for _ in 0..8 {
        let signed = ice_request(&f);
        assert_eq!(
            call_ice(&f, &signed, &f.app.origin).await.status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        call_ice(&f, &ice_request(&f), &f.app.origin).await.status(),
        StatusCode::FORBIDDEN
    );
    let mut f = Fixture::new();
    let expired = enable_ice(&mut f);
    let signed = ice_request(&f);
    let mut config: protocol::ice::IceConfig =
        serde_json::from_slice(&std::fs::read(&expired).unwrap()).unwrap();
    config.expires_at = now().unwrap() + 100;
    std::fs::write(&expired, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(
        call_ice(&f, &signed, &f.app.origin).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(path.exists());
}

#[tokio::test]
async fn expired_or_short_ice_credential_refuses_offer_before_session_reservation() {
    for remaining in [-1_i64, 100] {
        let mut f = Fixture::new();
        let path = enable_ice(&mut f);
        let now = now().unwrap();
        let mut config: protocol::ice::IceConfig =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config.issued_at = now - 500;
        config.expires_at = now.checked_add_signed(remaining).unwrap();
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let signed = f.signed_offer();
        let (status, _) = f
            .post(
                "/auth/offer",
                json!({"signed_offer": signed, "sdp": sdp("11")}),
                Some(&f.app.origin),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{remaining}s of ICE credential must be refused at admission"
        );
        assert!(f.app.active.lock().await.active.is_none());
    }
}

/// Refused after admission and before any session slot: the admitted entry must not survive.
#[tokio::test]
async fn offer_refused_after_admission_leaves_no_admitted_entry() {
    let mut f = Fixture::new();
    enable_ice(&mut f);
    // Refused at the codec gate: after admission, before any session slot exists.
    f.app.capabilities = json!({"h265":"unavailable","h264":"unavailable"});
    let signed = f.signed_offer();
    let request = signed.intent.request_id.clone();
    let body = json!({"signed_offer": signed, "sdp": sdp("11")});
    let (status, _) = f
        .post("/auth/offer", body.clone(), Some(&f.app.origin))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(f.app.active.lock().await.active.is_none());
    let source = f.auth().ice_config.clone().unwrap();
    assert!(
        source.bind_session(&request, "refused-session").is_err(),
        "a refused offer must not keep an admitted entry"
    );
    // accept_offer consumed the challenge, so the same signed offer cannot be admitted again.
    let (status, _) = f.post("/auth/offer", body, Some(&f.app.origin)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
/// A refused offer never reaches SessionGate::answer, so it leaves no pending gate entry:
/// all 256 gate slots stay available after it, and a leaked entry would fail the 256th answer.
#[tokio::test]
async fn refused_offer_leaves_session_gate_at_full_capacity() {
    let mut f = Fixture::new();
    enable_ice(&mut f);
    f.app.capabilities = json!({"h265":"unavailable","h264":"unavailable"});
    let signed = f.signed_offer();
    let (status, _) = f
        .post(
            "/auth/offer",
            json!({"signed_offer": signed, "sdp": sdp("11")}),
            Some(&f.app.origin),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let auth = f.auth();
    for _ in 0..256 {
        let offer = f.signed_offer();
        let accepted = auth
            .gate
            .accept_offer(&offer, &sdp("11"), now().unwrap())
            .unwrap();
        auth.gate
            .answer(
                accepted,
                uuid::Uuid::new_v4().to_string(),
                &sdp("22"),
                &f.host,
                now().unwrap(),
            )
            .expect("the refused offer must not occupy a gate slot");
    }
    // Capacity is exactly 256: the next offer is refused by the gate's own capacity check.
    let offer = f.signed_offer();
    let Err(refused) = auth.gate.accept_offer(&offer, &sdp("11"), now().unwrap()) else {
        panic!("the gate holds exactly 256 answered sessions");
    };
    assert!(
        refused.to_string().contains("session capacity"),
        "the 257th offer must fail on capacity, not another check: {refused}"
    );
}
/// Synthetic clock only. Filling the 256-entry admission table with live sessions blocks
/// the next offer. Tearing one down through the teardown path frees its slot and deadline.
#[tokio::test]
async fn torn_down_session_releases_its_deadline_and_admission_capacity() {
    let mut f = Fixture::new();
    synthetic_ice(&mut f);
    let auth = f.auth();
    let source = auth.ice_config.clone().unwrap();
    for i in 0..256 {
        source.admit_offer(&format!("request-{i}"), 100).unwrap();
        source
            .bind_session(&format!("request-{i}"), &format!("session-{i}"))
            .unwrap();
    }
    assert!(source.admit_offer("over-capacity", 100).is_err());
    auth.teardown_session("request-0", "session-0");
    source
        .admit_offer("after-teardown", 100)
        .expect("the torn-down session's admission slot is free");
    assert!(
        source.check_session("session-0", 100).is_err(),
        "a torn-down session has no deadline and fails closed"
    );
    // A session that never bound its answer also releases its request-keyed entry.
    auth.teardown_session("after-teardown", "never-bound");
    assert!(
        source.bind_session("after-teardown", "late").is_err(),
        "a torn-down unbound offer leaves no admitted entry"
    );
}
/// Synthetic clock only. While its session runs, the offer's post-response release must keep
/// the admitted entry, because the peer may still bind it. Teardown then releases it.
#[tokio::test]
async fn running_session_keeps_its_admitted_entry_until_teardown() {
    let mut f = Fixture::new();
    synthetic_ice(&mut f);
    let auth = f.auth();
    let source = auth.ice_config.clone().unwrap();
    source.admit_offer("running-request", 100).unwrap();
    let (cancel, _cancelled) = watch::channel(false);
    let (confirmed, _confirmation) = watch::channel(false);
    let (_done, finished) = watch::channel(None);
    f.app.active.lock().await.active = Some(Session {
        request: "running-request".into(),
        id: "running-session".into(),
        cancel,
        finished,
        confirmed,
        input: None,
        files: None,
    });
    release_unstarted(&f.app, "running-request").await;
    source
        .bind_session("running-request", "running-session")
        .expect("a running offer keeps its admitted entry");
    f.app.active.lock().await.active = None;
    auth.teardown_session("running-request", "running-session");
    assert!(
        source.check_session("running-session", 100).is_err(),
        "teardown releases the running session's entry"
    );
}

#[tokio::test]
async fn admitted_session_authorization_ends_at_ice_credential_deadline_on_synthetic_clock() {
    let mut f = Fixture::new();
    let path = enable_ice(&mut f);
    let config: protocol::ice::IceConfig =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let auth = f.auth();
    let source = auth.ice_config.clone().unwrap();
    // Admission in the offer handler's order: accept the signed offer, then admit its credential.
    let signed = f.signed_offer();
    let accepted = auth
        .gate
        .accept_offer(&signed, &sdp("11"), now().unwrap())
        .unwrap();
    source
        .admit_offer(&signed.intent.request_id, now().unwrap())
        .unwrap();
    // Signing binds the admitted offer to its session id; confirmation makes it active.
    let session = uuid::Uuid::new_v4().to_string();
    let answer = auth
        .sign_answer_with(accepted, &session, &sdp("22"), &f.host)
        .unwrap();
    auth.gate
        .confirm(&session, &f.confirmation(&answer), now().unwrap())
        .unwrap();
    let end = config.expires_at - crate::ice_config::SESSION_END_MARGIN_SECONDS;
    // Synthetic clock: the same step the 250 ms watch loop runs.
    assert!(authorization_check(&auth, &session, true, end - 1).is_ok());
    assert!(
        authorization_check(&auth, &session, true, end).is_err(),
        "session must end at credential expiry minus margin"
    );
    // The grant is still valid, so the credential bound is what ended the session.
    assert!(auth.gate.authorize_active(&session, "observe").is_ok());
}

#[tokio::test]
async fn source_failure_close_is_session_bound_repeatable_and_never_cleanup_proof() {
    let f = Fixture::new();
    let answer = f.pending();
    let signed = serde_json::to_value(f.signed_close(&answer)).unwrap();
    let request = answer.transcript.intent.request_id.clone();
    let session = answer.transcript.session_id.clone();
    f.complete(&answer, true).await;
    f.app.active.lock().await.source_failures.insert(
        request.clone(),
        (
            session.clone(),
            crate::source_status::SourceFailure {
                code: "guest_display_asleep",
            },
        ),
    );
    for _ in 0..2 {
        let (status, body) = f
            .post("/auth/close", signed.clone(), Some(&f.app.origin))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"closed":true,"media_failure":{"code":"guest_display_asleep"}})
        );
    }
    f.complete(&answer, false).await;
    let (status, body) = f
        .post("/auth/close", signed.clone(), Some(&f.app.origin))
        .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(!String::from_utf8(body)
        .unwrap()
        .contains("guest_display_asleep"));
    f.complete(&answer, true).await;
    f.app
        .active
        .lock()
        .await
        .source_failures
        .get_mut(&request)
        .unwrap()
        .0 = "different-session".into();
    let (_, body) = f.post("/auth/close", signed, Some(&f.app.origin)).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"closed":true})
    );
    let mut book = f.app.active.lock().await;
    book.completed.clear();
    book.prune();
    assert!(book.source_failures.is_empty());
}

// Real private Unix controller transport; no Guest, peer or encoder is started.
struct PreflightSource {
    _directory: tempfile::TempDir,
    _bridge: std::os::unix::net::UnixListener,
    state: Arc<std::sync::Mutex<Value>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    block: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
    marker: PathBuf,
}
impl Drop for PreflightSource {
    fn drop(&mut self) {
        self.release.notify_waiters();
        self.task.abort();
    }
}
impl PreflightSource {
    async fn attach(f: &mut Fixture) -> Self {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = path.join("bridge.sock");
        let bridge = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let metadata = std::fs::metadata(&socket).unwrap();
        let control = path.join("control.sock");
        let listener = tokio::net::UnixListener::bind(&control).unwrap();
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o600)).unwrap();
        let state = Arc::new(std::sync::Mutex::new(
            json!({"binding":{"version":1,"vm_id":f.grant.grant.vm_id,"worker_instance":"b".repeat(64),"bridge_instance":"c".repeat(64),"worker_pid":123,"socket_path":socket,"socket_device":metadata.dev(),"socket_inode":metadata.ino(),"guest_port":5001,"scope":"host-worker-listener-only","input_authorized":false}}),
        ));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let block = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task = {
            let (state, calls, block, entered, release) = (
                state.clone(),
                calls.clone(),
                block.clone(),
                entered.clone(),
                release.clone(),
            );
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let mut stream = BufReader::new(socket);
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    calls.fetch_add(1, Ordering::SeqCst);
                    if block.load(Ordering::SeqCst) {
                        entered.notify_one();
                        release.notified().await;
                    }
                    let response = if request["method"] == "guest_info" {
                        json!({"result":{"id":"fixture","result":{"console_identity":{"version":1,"security_session_id":42,"uid":501,"console_set":null,"display_id":1,"physical_width":2560,"physical_height":1440,"display_geometry_sha256":"a".repeat(64)},"console_identity_status":{"version":1,"identity_eligible":true}}}})
                    } else {
                        json!({"result":state.lock().unwrap().clone()})
                    };
                    let _ = stream
                        .get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await;
                }
            })
        };
        let args = vec![
            "--guest-binding-state".into(),
            path.to_string_lossy().into_owned(),
            "--guest-binding-vm".into(),
            f.grant.grant.vm_id.clone(),
            "--guest-worker-instance".into(),
            "b".repeat(64),
            "--guest-bridge-instance".into(),
            "c".repeat(64),
        ];
        let options = guest_binding::parse_options(&args).unwrap().0.unwrap();
        let source = guest_binding::Source::open(
            options,
            &socket,
            &f.grant.grant.vm_id,
            false,
            true,
            &json!({"console_identity_binding_version":1}),
        )
        .await
        .unwrap();
        f.app.guest_binding = Some(Arc::new(source));
        Arc::get_mut(f.app.auth.as_mut().unwrap())
            .unwrap()
            .test_identity = Some(Arc::new(TestIdentity::new(1)));
        // If the route accidentally reaches encoder spawn, leave an owned marker.
        let marker = path.join("encoder-ran");
        f.app.encoder = path.join("encoder.sh");
        std::fs::write(
            &f.app.encoder,
            format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&f.app.encoder, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _directory: directory,
            _bridge: bridge,
            state,
            calls,
            block,
            entered,
            release,
            task,
            marker,
        }
    }
    fn invalidate(&self) {
        self.state.lock().unwrap()["binding"]["bridge_instance"] = json!("d".repeat(64));
    }
    async fn assert_no_session_resources(&self, f: &Fixture) {
        let book = f.app.active.lock().await;
        assert!(book.active.is_none());
        assert!(book.completed.is_empty());
        assert!(book.recoveries.is_empty());
        assert!(f.auth().close_proofs.lock().unwrap().is_empty());
        assert!(!self.marker.exists());
    }
}
async fn preflight_offer(f: &Fixture, offer: &protocol::SignedOffer) -> Response {
    Router::new()
        .route("/auth/offer", post(super::offer))
        .with_state(f.app.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/offer")
                .header(header::HOST, &f.app.host)
                .header(header::ORIGIN, &f.app.origin)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"signed_offer":offer,"sdp":sdp("11")})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn assert_preflight_rejection(
    response: Response,
    f: &Fixture,
    offer: &protocol::SignedOffer,
) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let wire: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(wire.as_object().unwrap().len(), 1);
    let signed: protocol::rejection::SignedOfferRejection =
        serde_json::from_value(wire["signed_rejection"].clone()).unwrap();
    signed
        .verify(&f.host.public_key().unwrap(), &offer.intent)
        .unwrap();
    assert_eq!(signed.rejection.code, "guest_capture_unavailable");
    assert_eq!(signed.rejection.outcome, "not_admitted");
}

#[tokio::test]
async fn signed_preflight_rejection_is_exact_private_and_replay_retired_without_resources() {
    let mut f = Fixture::new();
    let source = PreflightSource::attach(&mut f).await;
    source.invalidate();
    let offer = f.signed_offer();
    let before = source.calls.load(Ordering::SeqCst);
    assert_preflight_rejection(preflight_offer(&f, &offer).await, &f, &offer).await;
    assert_eq!(source.calls.load(Ordering::SeqCst), before + 1);
    source.assert_no_session_resources(&f).await;
    assert!(f
        .app
        .active
        .lock()
        .await
        .cancelled
        .contains_key(&offer.intent.request_id));
    assert!(f
        .auth()
        .gate
        .accept_offer(&offer, &sdp("11"), now().unwrap())
        .is_err());
    assert_eq!(
        preflight_offer(&f, &offer).await.status(),
        StatusCode::FORBIDDEN
    );
    let mut fresh = f.signed_offer();
    fresh.intent.request_id = offer.intent.request_id.clone();
    fresh.signature = f.device.sign(&fresh.intent.canonical().unwrap()).unwrap();
    assert_eq!(
        preflight_offer(&f, &fresh).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        before + 1,
        "request tombstone must stop repeated RPCs"
    );
    source.assert_no_session_resources(&f).await;
}

#[tokio::test]
async fn preflight_source_errors_never_infer_sleep_from_untrusted_controller_text() {
    let mut f = Fixture::new();
    let source = PreflightSource::attach(&mut f).await;
    *source.state.lock().unwrap() = json!({"error":"SECRET /private/data display_asleep locked"});
    let offer = f.signed_offer();
    assert_preflight_rejection(preflight_offer(&f, &offer).await, &f, &offer).await;
    source.assert_no_session_resources(&f).await;
}

#[tokio::test]
async fn concurrent_preflight_gate_denies_second_offer_without_rpc_or_rejection() {
    let mut f = Fixture::new();
    let source = PreflightSource::attach(&mut f).await;
    source.invalidate();
    source.block.store(true, Ordering::SeqCst);
    let first = f.signed_offer();
    let second = f.signed_offer();
    let before = source.calls.load(Ordering::SeqCst);
    let pending = preflight_offer(&f, &first);
    tokio::pin!(pending);
    tokio::select! {
        _ = source.entered.notified() => {},
        _ = &mut pending => panic!("first request escaped blocked controller"),
        _ = tokio::time::sleep(Duration::from_secs(1)) => panic!("preflight did not reach controller"),
    }
    let response = tokio::time::timeout(Duration::from_millis(200), preflight_offer(&f, &second))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"error":"authorization rejected"})
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), before + 1);
    assert!(!f
        .app
        .active
        .lock()
        .await
        .cancelled
        .contains_key(&second.intent.request_id));
    assert!(f
        .auth()
        .gate
        .accept_offer(&second, &sdp("11"), now().unwrap())
        .is_err());
    source.release.notify_one();
    assert_preflight_rejection(
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap(),
        &f,
        &first,
    )
    .await;
    source.assert_no_session_resources(&f).await;
    assert!(f
        .app
        .guest_binding
        .as_ref()
        .unwrap()
        .admit_preflight()
        .is_ok());
}

#[tokio::test]
async fn failed_cleanup_before_or_during_preflight_cannot_be_overridden_by_rejection() {
    for during in [false, true] {
        let mut f = Fixture::new();
        let source = PreflightSource::attach(&mut f).await;
        source.invalidate();
        source.block.store(during, Ordering::SeqCst);
        let offer = f.signed_offer();
        let old_request = uuid::Uuid::new_v4().to_string();
        let old_session = uuid::Uuid::new_v4().to_string();
        let before = source.calls.load(Ordering::SeqCst);
        let pending = preflight_offer(&f, &offer);
        tokio::pin!(pending);
        if during {
            tokio::select! {
                _ = source.entered.notified() => {},
                _ = &mut pending => panic!("first request escaped blocked controller"),
                _ = tokio::time::sleep(Duration::from_secs(1)) => panic!("preflight did not reach controller"),
            }
        }
        f.app.active.lock().await.completed.insert(
            old_request.clone(),
            (Instant::now(), false, old_session.clone()),
        );
        source.release.notify_one();
        let response = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 4096).await.unwrap())
                .unwrap(),
            json!({"error":"authorization rejected"})
        );
        let book = f.app.active.lock().await;
        assert!(!book.completed[&old_request].1);
        assert_eq!(book.completed[&old_request].2, old_session);
        assert!(!book.cancelled.contains_key(&offer.intent.request_id));
        assert!(book.active.is_none());
        assert!(f.auth().close_proofs.lock().unwrap().is_empty());
        assert!(!source.marker.exists());
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            before + usize::from(during)
        );
    }
}
#[test]
fn file_opt_in_does_not_change_default_scopes_or_grant_input() {
    let fixture = Fixture::new();
    assert!(!scopes_permitted(
        &fixture.auth(),
        &["files".into(), "observe".into()]
    ));
    let mut auth = Arc::try_unwrap(fixture.app.auth.unwrap()).ok().unwrap();
    auth.files_enabled = true;
    assert!(scopes_permitted(&auth, &["files".into(), "observe".into()]));
    for scopes in [
        vec!["execute".into(), "observe".into()],
        vec!["input".into(), "observe".into()],
        vec!["files".into(), "input".into(), "observe".into()],
    ] {
        assert!(!scopes_permitted(&auth, &scopes));
    }
    assert!(from_args(&["--allow-files".into()]).is_err());
    assert!(from_args(&["--allow-files".into(), "--allow-files".into()]).is_err());
}
// Restart path: the gate is rebuilt from the durable registry file exactly as
// startup does (load, then gate_for_registry); no Keychain or real encoder is used.
fn restart_gate(f: &Fixture) -> protocol::SessionGate {
    let registry = protocol::store::Store::open(&f.auth().state)
        .unwrap()
        .load()
        .unwrap();
    gate_for_registry(&f.host.public_key().unwrap(), &registry).unwrap()
}
#[test]
fn restarted_gate_refuses_grant_revoked_before_restart() {
    let f = Fixture::new();
    f.revoke_registry();
    let restarted = restart_gate(&f);
    assert!(
        restarted
            .replace_grants(
                &f.host.public_key().unwrap(),
                std::slice::from_ref(&f.grant)
            )
            .is_err(),
        "grant revoked before restart was re-admitted"
    );
}
#[test]
fn restarted_gate_keeps_active_grant_at_durable_version() {
    let f = Fixture::new();
    let restarted = restart_gate(&f);
    let host = f.host.public_key().unwrap();
    restarted
        .replace_grants(&host, std::slice::from_ref(&f.grant))
        .unwrap();
    restarted
        .challenge(
            &f.grant.grant.device_id,
            &f.grant.grant.vm_id,
            vec!["observe".into()],
            100,
        )
        .unwrap();
}
/// Codec matrix: requested codec (the signed intent's `codec`) by host capability state. The
/// expected outcomes are read from the code, not measured:
/// - `accept_offer` runs `OfferIntent::canonical` first, which accepts only h264 and h265
///   (session-auth lib.rs:329-332). Any other codec is refused with the generic 403 before any
///   capability is read.
/// - `offer_inner` then requires `capabilities[codec] == "verified-hardware"` exactly
///   (main.rs:398). Every other value, a missing key included, is refused with 409.
/// - `stopping` is checked after that gate and before any session slot, encoder or peer exists
///   (main.rs:414). It is set in every row, so a row that passes the gate is observed as 503 and
///   nothing starts.
/// The other codec is always verified, so a silent substitution would show up as a pass.
#[tokio::test]
async fn codec_matrix_requested_codec_by_capability_state_fails_closed() {
    const PASSED_GATE: (StatusCode, &str) = (StatusCode::SERVICE_UNAVAILABLE, "service stopping");
    const CAPABILITY_REFUSED: (StatusCode, &str) = (
        StatusCode::CONFLICT,
        "required hardware encoder unavailable",
    );
    const CANONICAL_REFUSED: (StatusCode, &str) = (
        StatusCode::FORBIDDEN,
        r#"{"error":"authorization rejected"}"#,
    );
    // The value stored under the requested codec's key. None removes the key.
    let states: [(&str, Option<Value>); 7] = [
        ("available", Some(json!("verified-hardware"))),
        ("unavailable", Some(json!("unavailable"))),
        ("missing key", None),
        ("malformed bool", Some(json!(true))),
        ("malformed text", Some(json!("verified"))),
        ("malformed case", Some(json!("VERIFIED-HARDWARE"))),
        ("malformed null", Some(Value::Null)),
    ];
    for codec in ["h264", "h265", "vp9"] {
        for (state, value) in &states {
            let mut f = Fixture::new();
            enable_ice(&mut f);
            f.app.active.lock().await.stopping = true;
            let mut capabilities = json!({"h264":"verified-hardware","h265":"verified-hardware"});
            match value {
                Some(value) => capabilities[codec] = value.clone(),
                None => {
                    capabilities.as_object_mut().unwrap().remove(codec);
                }
            }
            f.app.capabilities = capabilities;
            let source = f.auth().ice_config.clone().unwrap();
            let signed = matrix_offer(&f, codec);
            let request_id = signed.intent.request_id.clone();
            let body = json!({"signed_offer": signed, "sdp": sdp("11")});
            let expected = match (codec, *state) {
                ("vp9", _) => CANONICAL_REFUSED,
                (_, "available") => PASSED_GATE,
                _ => CAPABILITY_REFUSED,
            };
            let (status, reply) = f
                .post("/auth/offer", body.clone(), Some(&f.app.origin))
                .await;
            let reply = String::from_utf8(reply).unwrap();
            assert_eq!(
                (status, reply.as_str()),
                expected,
                "codec {codec}, capability {state}"
            );
            {
                let book = f.app.active.lock().await;
                assert!(
                    book.active.is_none() && book.completed.is_empty(),
                    "codec {codec}, capability {state}: no session slot may exist"
                );
            }
            assert!(
                source.bind_session(&request_id, "matrix-probe").is_err(),
                "codec {codec}, capability {state}: no ICE admission may survive"
            );
            let (replay, _) = f.post("/auth/offer", body, Some(&f.app.origin)).await;
            assert_eq!(
                replay,
                StatusCode::FORBIDDEN,
                "codec {codec}, capability {state}: a replayed offer is refused"
            );
        }
    }
}
/// A validly signed offer for `codec`. A codec outside h264|h265 cannot be canonicalised, so it is
/// swapped in after signing and is refused by canonicalisation before any signature is checked.
fn matrix_offer(f: &Fixture, codec: &str) -> protocol::SignedOffer {
    let signing = if matches!(codec, "h264" | "h265") {
        codec
    } else {
        "h265"
    };
    let mut intent = protocol::OfferIntent {
        challenge: f
            .auth()
            .gate
            .challenge(
                &f.grant.grant.device_id,
                &f.grant.grant.vm_id,
                vec!["observe".into()],
                now().unwrap(),
            )
            .unwrap(),
        request_id: uuid::Uuid::new_v4().to_string(),
        codec: signing.into(),
        offer: protocol::SdpBinding::from_sdp(&sdp("11")).unwrap(),
    };
    let signature = f.device.sign(&intent.canonical().unwrap()).unwrap();
    intent.codec = codec.into();
    protocol::SignedOffer { intent, signature }
}
/// The unsigned route's own codec allowlist (`offer_inner`, main.rs:394) refuses a codec outside
/// h264|h265 even when a capability entry under that name says verified-hardware.
#[tokio::test]
async fn codec_matrix_unsigned_route_refuses_unknown_codec_with_verified_entry() {
    let mut f = Fixture::new();
    f.app.capabilities =
        json!({"h264":"verified-hardware","h265":"verified-hardware","vp9":"verified-hardware"});
    f.app.active.lock().await.stopping = true;
    let offer = crate::Offer {
        request_id: uuid::Uuid::new_v4().to_string(),
        sdp: sdp("11"),
        r#type: "offer".into(),
        codec: "vp9".into(),
    };
    let response = crate::offer_inner(f.app.clone(), offer, None).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(body.to_vec(), b"invalid offer".to_vec());
    assert!(f.app.active.lock().await.active.is_none());
}
