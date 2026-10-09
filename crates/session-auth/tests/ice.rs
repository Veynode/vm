use p256::ecdsa::{signature::Signer, SigningKey};
use std::sync::{Arc, Barrier};
use vm_session_auth::ice::*;
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
fn request(gate: &SessionGate, g: &SignedGrant, d: &TestKey) -> SignedIceRequest {
    let request = IceRequest {
        challenge: gate
            .challenge(&g.grant.device_id, "linux", vec!["observe".into()], 100)
            .unwrap(),
        request_nonce: random32(),
    };
    SignedIceRequest {
        signature: d.sign(&request.canonical().unwrap()).unwrap(),
        request,
    }
}
fn config() -> IceConfig {
    IceConfig {
        version: 1,
        transport_policy: "relay".into(),
        issued_at: 100,
        expires_at: 700,
        ice_servers: vec![IceServer {
            urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
            username: "700:test".into(),
            credential: "fixture-secret".into(),
        }],
    }
}
#[test]
fn signed_ice_exact_response_golden_and_tamper() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("ice-golden.json")).unwrap();
    let response: SignedIceConfig = serde_json::from_value(fixture["response"].clone()).unwrap();
    let request: SignedIceRequest =
        serde_json::from_value(fixture["signed_request"].clone()).unwrap();
    assert_eq!(
        hex(&request.request.canonical().unwrap()),
        fixture["request_canonical_hex"]
    );
    assert_eq!(
        hex(&response.config.canonical().unwrap()),
        fixture["config_canonical_hex"]
    );
    assert_eq!(
        hex(&SignedIceConfig::signing_bytes(&request.request, &response.config).unwrap()),
        fixture["response_canonical_hex"]
    );
    let host = fixture["host_public_key"].as_str().unwrap();
    let now = fixture["now"].as_u64().unwrap();
    response.verify(host, &request.request, now).unwrap();
    for change in 0..4 {
        let mut bad = response.clone();
        match change {
            0 => bad.config.transport_policy = "all".into(),
            1 => bad.config.ice_servers[0].credential = "substitution".into(),
            2 => bad.request.request_nonce = random32(),
            _ => bad.config.expires_at -= 1,
        };
        assert!(bad.verify(host, &request.request, now).is_err());
    }
    assert!(response.verify(host, &request.request, now + 601).is_err());
}
#[test]
fn bad_signature_does_not_consume_concurrent_replay_one_winner_and_no_offer_reuse() {
    let (_, d, g, gate) = fixture();
    let signed = request(&gate, &g, &d);
    let mut bad = signed.clone();
    bad.signature = "00".repeat(64);
    assert!(gate.accept_ice_request(&bad, 101).is_err());
    let gate = Arc::new(gate);
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (g, b, s) = (gate.clone(), barrier.clone(), signed.clone());
            std::thread::spawn(move || {
                b.wait();
                g.accept_ice_request(&s, 101).is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap().then_some(()))
            .count(),
        1
    );
    let intent = OfferIntent {
        challenge: signed.request.challenge,
        request_id: random32(),
        codec: "h264".into(),
        offer: SdpBinding::from_sdp(&sdp("11")).unwrap(),
    };
    let offer = SignedOffer {
        signature: d.sign(&intent.canonical().unwrap()).unwrap(),
        intent,
    };
    assert!(gate.accept_offer(&offer, &sdp("11"), 101).is_err());
}
#[test]
fn current_grant_and_process_restart_and_cross_vm_rejected() {
    let (h, d, g, gate) = fixture();
    let signed = request(&gate, &g, &d);
    let fresh = SessionGate::new(&h.public_key().unwrap(), std::slice::from_ref(&g)).unwrap();
    assert!(fresh.accept_ice_request(&signed, 101).is_err());
    let mut bad = signed.clone();
    bad.request.challenge.vm_id = "other".into();
    bad.signature = d.sign(&bad.request.canonical().unwrap()).unwrap();
    assert!(gate.accept_ice_request(&bad, 101).is_err());
    gate.replace_grants(&h.public_key().unwrap(), &[]).unwrap();
    assert!(gate.accept_ice_request(&signed, 101).is_err());
    assert!(gate.authorize_ice_grant(&signed.request.challenge).is_err());
}
#[test]
fn credential_policy_limits_and_no_external_destinations() {
    config().check_time(490).unwrap();
    assert!(config().check_time(491).is_err());
    assert!(config().check_time(99).is_err());
    for url in [
        "turn:example.com:3478?transport=udp",
        "turn:127.0.0.1:3478?transport=tcp",
        "turns:127.0.0.1:3478",
        "turn:127.0.0.1:03478?transport=udp",
        "turn:127.0.0.1:0?transport=udp",
        "turn:127.0.0.1:65536?transport=udp",
        "stun:127.0.0.1:3478?transport=udp",
    ] {
        assert!(validate_local_url(url).is_err());
    }
    let mut c = config();
    let duplicate = c.ice_servers[0].urls[0].clone();
    c.ice_servers[0].urls.push(duplicate);
    assert!(c.canonical().is_err());
    let mut c = config();
    c.expires_at += 1;
    assert!(c.canonical().is_err());
    let mut c = config();
    c.ice_servers.clear();
    assert!(c.canonical().is_err());
    c.transport_policy = "all".into();
    c.canonical().unwrap();
    let mut c = config();
    c.ice_servers[0].urls = vec!["stun:127.0.0.1:3478".into()];
    assert!(c.canonical().is_err());
}
