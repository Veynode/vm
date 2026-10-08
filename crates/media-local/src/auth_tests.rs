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
            host_public_key: host_public_key.clone(),
            vm_id: grant.grant.vm_id.clone(),
            gate: protocol::SessionGate::new(&host_public_key, std::slice::from_ref(&grant))
                .unwrap(),
            state,
            namespace: "not-used-no-keychain".into(),
            close_proofs: std::sync::Mutex::new(BTreeMap::new()),
        });
        let app = App {
            host: "127.0.0.1:8787".into(),
            origin: "http://127.0.0.1:8787".into(),
            encoder: PathBuf::from("must-not-start-encoder"),
            socket: PathBuf::new(),
            web: PathBuf::new(),
            capabilities: json!({"h265":"verified-hardware","h264":"verified-hardware"}),
            fault_drop_first_keyframe: false,
            fault_suppress_hevc_frames: false,
            active: Arc::new(Mutex::new(SessionBook::default())),
            auth: Some(auth),
            cleanup_failure_exit: None,
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
async fn wrong_confirmation_cannot_start_encoder_watch_and_valid_confirmation_is_single_use() {
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
        StatusCode::FORBIDDEN
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

    // Completion receipts expire at 60s while signed close proofs live for 240s.
    // Simulate only that elapsed clock boundary, preserving the old valid proof.
    {
        let mut book = f.app.active.lock().await;
        book.completed.get_mut(&request_id).unwrap().0 = Instant::now() - Duration::from_secs(61);
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
        f.post("/auth/offer", json!({"signed_offer":fresh,"sdp":sdp("11")}), Some(&f.app.origin)),
        crate::await_shutdown_cleanup(finished, Duration::from_millis(50))
    );
    wait.unwrap();
    assert_eq!(receipt.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(String::from_utf8_lossy(&receipt.1).contains("service stopping"));
    assert!(f.app.active.lock().await.active.is_none());
    // Authentication succeeded/consumed the one-use challenge before admission
    // refused it; this does not create a replayable token or pending session.
    assert!(f.auth().gate.accept_offer(&fresh, &sdp("11"), now().unwrap()).is_err());
}
