use p256::ecdsa::{signature::Signer, SigningKey};
use vm_session_auth::remote::*;
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
/// An ECDH public key: any valid point works for the signed handshake.
fn ecdh(n: u8) -> String {
    TestKey::new(n).public_key().unwrap()
}
const ORIGIN: &str = "https://desk.example.com";
const NOW: u64 = 1_000;

fn grant(h: &TestKey, d: &TestKey, scopes: &[&str], version: u64) -> SignedGrant {
    SignedGrant::new(
        Grant {
            host_id: identity_id(&h.public_key().unwrap()).unwrap(),
            device_id: identity_id(&d.public_key().unwrap()).unwrap(),
            device_public_key: d.public_key().unwrap(),
            vm_id: "linux".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            version,
        },
        h,
    )
    .unwrap()
}

fn fixture() -> (TestKey, TestKey, SignedGrant, SessionGate) {
    let (h, d) = (TestKey::new(1), TestKey::new(2));
    let g = grant(&h, &d, &["input", "observe"], 1);
    let gate = SessionGate::new(&h.public_key().unwrap(), std::slice::from_ref(&g)).unwrap();
    (h, d, g, gate)
}

fn offer(gate: &SessionGate, d: &TestKey, scopes: &[&str], transport: &str) -> SignedRemoteOffer {
    let device_id = identity_id(&d.public_key().unwrap()).unwrap();
    let challenge = gate
        .challenge(
            &device_id,
            "linux",
            scopes.iter().map(|s| s.to_string()).collect(),
            NOW,
        )
        .unwrap();
    let intent = RemoteOfferIntent {
        challenge,
        request_id: "r1".into(),
        codecs: vec!["h264".into(), "h265".into()],
        browser_ecdh: ecdh(9),
        transport: transport.into(),
        origin: ORIGIN.into(),
    };
    let signature = d.sign(&intent.canonical().unwrap()).unwrap();
    SignedRemoteOffer { intent, signature }
}

fn handshake(gate: &SessionGate, h: &TestKey, d: &TestKey, scopes: &[&str]) -> RemoteTranscript {
    let signed = offer(gate, d, scopes, "wt");
    let accepted = gate
        .accept_remote_offer(&signed, "wt", ORIGIN, NOW + 1)
        .unwrap();
    let answer = gate
        .remote_answer(
            accepted,
            RemoteAnswerParams {
                session_id: "s1".into(),
                host_ecdh: ecdh(10),
                codec: "h265".into(),
                lifetime_seconds: 3600,
            },
            h,
            NOW + 1,
        )
        .unwrap();
    answer
        .verify(&h.public_key().unwrap(), &signed.intent)
        .unwrap();
    let digest = answer.transcript.digest().unwrap();
    let confirmation = Confirmation {
        signature: d
            .sign(&remote_confirmation_bytes(&digest).unwrap())
            .unwrap(),
        transcript_sha256: digest,
    };
    gate.remote_confirm("s1", &confirmation, NOW + 2).unwrap()
}

#[test]
fn full_handshake_authorizes_granted_scopes_until_expiry() {
    let (h, d, _, gate) = fixture();
    let t = handshake(&gate, &h, &d, &["input", "observe"]);
    assert_eq!(t.expires_at, NOW + 1 + 3600);
    gate.authorize_remote("s1", "input", NOW + 10).unwrap();
    assert!(gate.authorize_remote("s1", "files", NOW + 10).is_err());
    assert!(gate
        .authorize_remote("s1", "observe", t.expires_at)
        .is_err());
    assert_eq!(gate.expired_remote(t.expires_at), vec!["s1".to_string()]);
}

#[test]
fn observe_only_session_cannot_send_input() {
    let (h, d, _, gate) = fixture();
    handshake(&gate, &h, &d, &["observe"]);
    gate.authorize_remote("s1", "observe", NOW + 3).unwrap();
    assert!(gate.authorize_remote("s1", "input", NOW + 3).is_err());
}

#[test]
fn challenge_is_single_use_and_bound_to_transport_and_origin() {
    let (_, d, _, gate) = fixture();
    let signed = offer(&gate, &d, &["observe"], "wss");
    assert!(gate
        .accept_remote_offer(&signed, "wt", ORIGIN, NOW + 1)
        .is_err());
    assert!(gate
        .accept_remote_offer(&signed, "wss", "https://evil.example", NOW + 1)
        .is_err());
    gate.accept_remote_offer(&signed, "wss", ORIGIN, NOW + 1)
        .unwrap();
    assert!(gate
        .accept_remote_offer(&signed, "wss", ORIGIN, NOW + 1)
        .is_err());
}

#[test]
fn tampered_offer_and_substituted_answer_key_are_rejected() {
    let (h, d, _, gate) = fixture();
    let mut signed = offer(&gate, &d, &["observe"], "wt");
    let original = signed.intent.clone();
    signed.intent.browser_ecdh = ecdh(11);
    assert!(gate
        .accept_remote_offer(&signed, "wt", ORIGIN, NOW + 1)
        .is_err());
    signed.intent = original;
    let accepted = gate
        .accept_remote_offer(&signed, "wt", ORIGIN, NOW + 1)
        .unwrap();
    let mut answer = gate
        .remote_answer(
            accepted,
            RemoteAnswerParams {
                session_id: "s1".into(),
                host_ecdh: ecdh(10),
                codec: "h264".into(),
                lifetime_seconds: 60,
            },
            &h,
            NOW + 1,
        )
        .unwrap();
    answer.transcript.host_ecdh = ecdh(12);
    assert!(answer
        .verify(&h.public_key().unwrap(), &signed.intent)
        .is_err());
}

#[test]
fn answer_rejects_unoffered_codec_and_long_lifetime() {
    let (h, d, _, gate) = fixture();
    let mut signed = offer(&gate, &d, &["observe"], "wt");
    signed.intent.codecs = vec!["h264".into()];
    signed.signature = d.sign(&signed.intent.canonical().unwrap()).unwrap();
    let accepted = gate
        .accept_remote_offer(&signed, "wt", ORIGIN, NOW + 1)
        .unwrap();
    assert!(gate
        .remote_answer(
            accepted,
            RemoteAnswerParams {
                session_id: "s1".into(),
                host_ecdh: ecdh(10),
                codec: "h265".into(),
                lifetime_seconds: 60
            },
            &h,
            NOW + 1
        )
        .is_err());
    let signed = offer(&gate, &d, &["observe"], "wt");
    let accepted = gate
        .accept_remote_offer(&signed, "wt", ORIGIN, NOW + 1)
        .unwrap();
    assert!(gate
        .remote_answer(
            accepted,
            RemoteAnswerParams {
                session_id: "s2".into(),
                host_ecdh: ecdh(10),
                codec: "h264".into(),
                lifetime_seconds: MAX_SESSION_SECONDS + 1
            },
            &h,
            NOW + 1
        )
        .is_err());
}

#[test]
fn revocation_returns_remote_session_for_teardown() {
    let (h, d, _, gate) = fixture();
    handshake(&gate, &h, &d, &["input", "observe"]);
    let host_public = h.public_key().unwrap();
    let revoked = gate.replace_grants(&host_public, &[]).unwrap();
    assert_eq!(revoked, vec!["s1".to_string()]);
    assert!(gate.authorize_remote("s1", "observe", NOW + 3).is_err());
    // Narrowing scopes (a new version without input) also ends the session.
    let (h, d, _, gate) = fixture();
    handshake(&gate, &h, &d, &["input", "observe"]);
    let narrowed = grant(&h, &d, &["observe"], 2);
    assert_eq!(
        gate.replace_grants(&host_public, &[narrowed]).unwrap(),
        vec!["s1".to_string()]
    );
}

#[test]
fn wrong_confirmation_does_not_activate() {
    let (h, d, _, gate) = fixture();
    let signed = offer(&gate, &d, &["observe"], "wt");
    let accepted = gate
        .accept_remote_offer(&signed, "wt", ORIGIN, NOW + 1)
        .unwrap();
    let answer = gate
        .remote_answer(
            accepted,
            RemoteAnswerParams {
                session_id: "s1".into(),
                host_ecdh: ecdh(10),
                codec: "h264".into(),
                lifetime_seconds: 60,
            },
            &h,
            NOW + 1,
        )
        .unwrap();
    let digest = answer.transcript.digest().unwrap();
    let forged = Confirmation {
        signature: h
            .sign(&remote_confirmation_bytes(&digest).unwrap())
            .unwrap(),
        transcript_sha256: digest,
    };
    assert!(gate.remote_confirm("s1", &forged, NOW + 2).is_err());
    assert!(gate.authorize_remote("s1", "observe", NOW + 2).is_err());
}
