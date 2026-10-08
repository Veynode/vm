use p256::ecdsa::{signature::Signer, SigningKey};
use std::sync::{Arc, Barrier};
use vm_session_auth::*;
struct TestKey(SigningKey);
impl TestKey {
    fn new(n: u8) -> Self {
        let mut b = [0; 32];
        b[31] = n;
        Self(SigningKey::from_bytes((&b).into()).unwrap())
    }
}
impl Identity for TestKey {
    fn public_key(&self) -> anyhow::Result<String> {
        Ok(hex(self
            .0
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()))
    }
    fn sign(&self, b: &[u8]) -> anyhow::Result<String> {
        let s: p256::ecdsa::Signature = self.0.sign(b);
        Ok(hex(&s.to_bytes()))
    }
}
fn sdp(v: &str) -> String {
    format!(
        "v=0\r\na=fingerprint:sha-256 {}\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=recvonly\r\n",
        vec![v; 32].join(":")
    )
}
fn fixture() -> (TestKey, TestKey, SignedGrant, SessionGate) {
    let h = TestKey::new(1);
    let d = TestKey::new(2);
    let grant = SignedGrant::new(
        Grant {
            host_id: identity_id(&h.public_key().unwrap()).unwrap(),
            device_id: identity_id(&d.public_key().unwrap()).unwrap(),
            device_public_key: d.public_key().unwrap(),
            vm_id: "linux".into(),
            scopes: vec!["input".into(), "observe".into()],
            version: 1,
        },
        &h,
    )
    .unwrap();
    let gate = SessionGate::new(&h.public_key().unwrap(), std::slice::from_ref(&grant)).unwrap();
    (h, d, grant, gate)
}
fn offer(gate: &SessionGate, grant: &SignedGrant, d: &TestKey) -> SignedOffer {
    let challenge = gate
        .challenge(&grant.grant.device_id, "linux", vec!["observe".into()], 100)
        .unwrap();
    let intent = OfferIntent {
        challenge,
        request_id: random32(),
        codec: "h265".into(),
        offer: SdpBinding::from_sdp(&sdp("11")).unwrap(),
    };
    SignedOffer {
        signature: d.sign(&intent.canonical().unwrap()).unwrap(),
        intent,
    }
}
fn answer(gate: &SessionGate, g: &SignedGrant, h: &TestKey, d: &TestKey) -> SignedAnswer {
    let offer = offer(gate, g, d);
    let accepted = gate.accept_offer(&offer, &sdp("11"), 101).unwrap();
    gate.answer(accepted, random32(), &sdp("22"), h, 102)
        .unwrap()
}
fn confirmation(a: &SignedAnswer, d: &TestKey) -> Confirmation {
    let digest = a.transcript.digest().unwrap();
    Confirmation {
        signature: d.sign(&confirmation_bytes(&digest).unwrap()).unwrap(),
        transcript_sha256: digest,
    }
}
#[test]
fn real_crypto_complete_handshake_and_signed_close() {
    let (h, d, g, gate) = fixture();
    let a = answer(&gate, &g, &h, &d);
    a.verify(&h.public_key().unwrap(), &a.transcript.intent, &sdp("22"))
        .unwrap();
    gate.authorize_session(&a.transcript.session_id, "observe")
        .unwrap();
    assert!(gate
        .authorize_active(&a.transcript.session_id, "observe")
        .is_err());
    gate.confirm(&a.transcript.session_id, &confirmation(&a, &d), 103)
        .unwrap();
    gate.authorize_active(&a.transcript.session_id, "observe")
        .unwrap();
    assert!(gate
        .authorize_active(&a.transcript.session_id, "input")
        .is_err());
    let digest = a.transcript.digest().unwrap();
    let close = SignedClose {
        session_id: a.transcript.session_id.clone(),
        transcript_sha256: digest.clone(),
        signature: d
            .sign(&close_bytes(&a.transcript.session_id, &digest).unwrap())
            .unwrap(),
    };
    gate.verify_close(&close).unwrap();
    let mut bad = close.clone();
    bad.session_id = "other-vm".into();
    assert!(gate.verify_close(&bad).is_err());
    gate.close(&close.session_id);
    assert!(gate.authorize_active(&close.session_id, "observe").is_err());
}
#[test]
fn signature_failure_does_not_consume_but_concurrent_replay_has_one_winner() {
    let (_, d, g, gate) = fixture();
    let signed = offer(&gate, &g, &d);
    let mut bad = signed.clone();
    bad.signature = "00".repeat(64);
    assert!(gate.accept_offer(&bad, &sdp("11"), 101).is_err());
    let gate = Arc::new(gate);
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let gate = gate.clone();
            let signed = signed.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                gate.accept_offer(&signed, &sdp("11"), 101).is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().unwrap().then_some(()))
            .count(),
        1
    );
}
#[test]
fn exact_sdp_and_context_tampering_rejected() {
    let (_, d, g, gate) = fixture();
    let signed = offer(&gate, &g, &d);
    assert!(gate
        .accept_offer(&signed, &sdp("11").replace("recvonly", "sendrecv"), 101)
        .is_err());
    for change in 0..7 {
        let mut x = signed.clone();
        match change {
            0 => x.intent.challenge.vm_id = "other".into(),
            1 => x.intent.challenge.host_id = "00".repeat(32),
            2 => x.intent.challenge.device_id = "00".repeat(32),
            3 => x.intent.challenge.grant_version += 1,
            4 => x.intent.challenge.expires_at += 1,
            5 => x.intent.codec = "h264".into(),
            _ => x.intent.challenge.scopes = vec!["execute".into()],
        };
        assert!(gate.accept_offer(&x, &sdp("11"), 101).is_err());
    }
    gate.accept_offer(&signed, &sdp("11"), 101).unwrap();
}
#[test]
fn expiry_restart_and_wrong_device_rejected() {
    let (h, d, g, gate) = fixture();
    let signed = offer(&gate, &g, &d);
    assert!(gate.accept_offer(&signed, &sdp("11"), 99).is_err());
    let signed = offer(&gate, &g, &d);
    assert!(gate.accept_offer(&signed, &sdp("11"), 160).is_err());
    let signed = offer(&gate, &g, &d);
    let other = SessionGate::new(&h.public_key().unwrap(), std::slice::from_ref(&g)).unwrap();
    assert!(other.accept_offer(&signed, &sdp("11"), 101).is_err());
    let mut wrong = signed.clone();
    wrong.signature = TestKey::new(3)
        .sign(&wrong.intent.canonical().unwrap())
        .unwrap();
    assert!(gate.accept_offer(&wrong, &sdp("11"), 101).is_err());
}
#[test]
fn revocation_blocks_pending_active_and_old_grant_reinstatement() {
    let (h, d, g, gate) = fixture();
    let a = answer(&gate, &g, &h, &d);
    gate.confirm(&a.transcript.session_id, &confirmation(&a, &d), 103)
        .unwrap();
    let b = answer(&gate, &g, &h, &d);
    let revoked = gate.replace_grants(&h.public_key().unwrap(), &[]).unwrap();
    assert_eq!(revoked.len(), 2);
    assert!(gate
        .authorize_active(&a.transcript.session_id, "observe")
        .is_err());
    assert!(gate
        .confirm(&b.transcript.session_id, &confirmation(&b, &d), 103)
        .is_err());
    assert!(gate.replace_grants(&h.public_key().unwrap(), &[g]).is_err());
}
#[test]
fn answer_and_confirm_cannot_be_substituted() {
    let (h, d, g, gate) = fixture();
    let a = answer(&gate, &g, &h, &d);
    assert!(a
        .verify(&h.public_key().unwrap(), &a.transcript.intent, &sdp("33"))
        .is_err());
    let mut x = a.clone();
    x.transcript.session_id = "attacker".into();
    assert!(x
        .verify(&h.public_key().unwrap(), &a.transcript.intent, &sdp("22"))
        .is_err());
    let mut c = confirmation(&a, &d);
    c.transcript_sha256 = "00".repeat(32);
    assert!(gate.confirm(&a.transcript.session_id, &c, 103).is_err());
    gate.confirm(&a.transcript.session_id, &confirmation(&a, &d), 103)
        .unwrap();
    assert!(gate
        .confirm(&a.transcript.session_id, &confirmation(&a, &d), 103)
        .is_err());
}
#[test]
fn strict_fingerprint_scope_hex_and_size_boundaries() {
    for bad in [
        sdp("11").replace("sha-256", "sha-1"),
        format!(
            "{}a=fingerprint:sha-256 {}\r\n",
            sdp("11"),
            vec!["22"; 32].join(":")
        ),
        sdp("11").replace("\r\n", "\r\r\n"),
        "x".repeat(MAX_SDP + 1),
    ] {
        assert!(SdpBinding::from_sdp(&bad).is_err());
    }
    let per = sdp("11").replace(
        "a=fingerprint",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=fingerprint",
    );
    assert!(SdpBinding::from_sdp(&per).is_err());
    assert!(validate_scopes(&["observe".into(), "input".into()]).is_err());
    assert!(validate_scopes(&["observe".into(), "observe".into()]).is_err());
    assert!(unhex(&"AA".repeat(32), 32).is_err());
    assert!(identity_id(&"04".repeat(65)).is_err());
}
#[test]
fn capacity_fails_closed_no_active_eviction() {
    let (_, _, g, gate) = fixture();
    for _ in 0..256 {
        gate.challenge(&g.grant.device_id, "linux", vec!["observe".into()], 100)
            .unwrap();
    }
    assert!(gate
        .challenge(&g.grant.device_id, "linux", vec!["observe".into()], 100)
        .is_err());
    gate.challenge(&g.grant.device_id, "linux", vec!["observe".into()], 160)
        .unwrap();
}
#[test]
fn golden_bytes_and_signatures() {
    let v: serde_json::Value = serde_json::from_str(include_str!("golden.json")).unwrap();
    let grant: SignedGrant = serde_json::from_value(v["signed_grant"].clone()).unwrap();
    let offer: SignedOffer = serde_json::from_value(v["signed_offer"].clone()).unwrap();
    let answer: SignedAnswer = serde_json::from_value(v["signed_answer"].clone()).unwrap();
    assert_eq!(
        hex(&offer.intent.canonical().unwrap()),
        v["canonical_hex"]["offer"]
    );
    assert_eq!(
        hex(&answer.transcript.canonical().unwrap()),
        v["canonical_hex"]["answer"]
    );
    grant
        .verify(v["host_public_key"].as_str().unwrap())
        .unwrap();
    verify(
        v["device_public_key"].as_str().unwrap(),
        &offer.intent.canonical().unwrap(),
        &offer.signature,
    )
    .unwrap();
    answer
        .verify(
            v["host_public_key"].as_str().unwrap(),
            &offer.intent,
            v["answer_sdp"].as_str().unwrap(),
        )
        .unwrap();
}
#[test]
fn durable_registry_permissions_lock_and_repairless_revoke() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use store::*;
    let (h, d, _, _) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private");
    let store = Store::open(&path).unwrap();
    assert!(Store::open(&path).is_err());
    let mut registry = Registry {
        host_public_key: h.public_key().unwrap(),
        ..Registry::default()
    };
    let pairing = Pairing {
        host_public_key: h.public_key().unwrap(),
        device_public_key: d.public_key().unwrap(),
        nonce: random32(),
        vm_id: "linux".into(),
        scopes: vec!["observe".into()],
    };
    assert!(registry.approve(&pairing, &"00".repeat(32), &h).is_err());
    let grant = registry
        .approve(&pairing, &pairing.digest().unwrap(), &h)
        .unwrap();
    store.save(&registry).unwrap();
    assert_eq!(store.load().unwrap().grants.len(), 1);
    assert!(registry
        .approve(&pairing, &pairing.digest().unwrap(), &h)
        .is_err());
    registry.revoke(&grant.grant.device_id, "linux").unwrap();
    store.save(&registry).unwrap();
    let new = registry
        .approve(&pairing, &pairing.digest().unwrap(), &h)
        .unwrap();
    assert_eq!(new.grant.version, 3);
    assert_eq!(
        std::fs::metadata(path.join("grants.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    drop(store);
    let alias = dir.path().join("alias");
    symlink(&path, &alias).unwrap();
    assert!(Store::open(&alias).is_err());
}
