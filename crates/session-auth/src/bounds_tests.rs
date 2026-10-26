//! Numeric bounds of the session gate, the SDP binding and the ICE configuration. Each assertion
//! names the bound it protects. A change to any value here must fail a test and be decided on
//! purpose. Synthetic clocks and in-memory keys only: no sleeps and no I/O.
use super::*;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};

struct TestIdentity(SigningKey);

impl TestIdentity {
    fn new(scalar: u8) -> Self {
        let mut bytes = [0u8; 32];
        bytes[31] = scalar;
        Self(SigningKey::from_bytes((&bytes).into()).unwrap())
    }
}

impl Identity for TestIdentity {
    fn public_key(&self) -> Result<String> {
        Ok(hex(self
            .0
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()))
    }

    fn sign(&self, message: &[u8]) -> Result<String> {
        let signature: Signature = self.0.sign(message);
        Ok(hex(&signature.to_bytes()))
    }
}

fn sdp(octet: &str) -> String {
    format!(
        "v=0\r\na=fingerprint:sha-256 {}\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=recvonly\r\n",
        vec![octet; 32].join(":")
    )
}

/// One host, one device, and a gate that trusts exactly one observe grant for that device.
struct Setup {
    host: TestIdentity,
    host_public: String,
    device: TestIdentity,
    grant: SignedGrant,
    gate: SessionGate,
}

impl Setup {
    fn new() -> Self {
        let host = TestIdentity::new(1);
        let device = TestIdentity::new(2);
        let host_public = host.public_key().unwrap();
        let grant = SignedGrant::new(
            Grant {
                host_id: identity_id(&host_public).unwrap(),
                device_id: identity_id(&device.public_key().unwrap()).unwrap(),
                device_public_key: device.public_key().unwrap(),
                vm_id: "vm-bounds".into(),
                scopes: vec!["observe".into()],
                version: 1,
            },
            &host,
        )
        .unwrap();
        let gate = SessionGate::new(&host_public, std::slice::from_ref(&grant)).unwrap();
        Self {
            host,
            host_public,
            device,
            grant,
            gate,
        }
    }

    fn challenge(&self, now: u64) -> Challenge {
        self.gate
            .challenge(
                &self.grant.grant.device_id,
                &self.grant.grant.vm_id,
                vec!["observe".into()],
                now,
            )
            .unwrap()
    }

    fn offer(&self, request_id: &str, sdp_text: &str, now: u64) -> SignedOffer {
        let intent = OfferIntent {
            challenge: self.challenge(now),
            request_id: request_id.into(),
            codec: "h265".into(),
            offer: SdpBinding::from_sdp(sdp_text).unwrap(),
        };
        let signature = self.device.sign(&intent.canonical().unwrap()).unwrap();
        SignedOffer { intent, signature }
    }
}

fn turn(ports: &[u32], username: &str, credential: &str) -> ice::IceServer {
    ice::IceServer {
        urls: ports
            .iter()
            .map(|port| format!("turn:127.0.0.1:{port}?transport=udp"))
            .collect(),
        username: username.into(),
        credential: credential.into(),
    }
}

fn ice_config(servers: Vec<ice::IceServer>) -> ice::IceConfig {
    ice::IceConfig {
        version: 1,
        transport_policy: "all".into(),
        issued_at: 100,
        expires_at: 700,
        ice_servers: servers,
    }
}

/// MAX_SDP (lib.rs:20) bounds the SDP an offer or answer binds to (lib.rs:252). Exactly MAX_SDP
/// bytes binds; one more byte is refused with "invalid SDP size/content". The gate applies the
/// same bound before it checks the signature (lib.rs:595).
#[test]
fn sdp_binding_accepts_exactly_max_sdp_bytes_and_refuses_one_more() {
    assert_eq!(MAX_SDP, 128 * 1024);
    let mut exact = sdp("11");
    let padding = MAX_SDP - exact.len() - "a=pad:\r\n".len();
    exact.push_str("a=pad:");
    exact.push_str(&"x".repeat(padding));
    exact.push_str("\r\n");
    assert_eq!(exact.len(), MAX_SDP);
    assert!(SdpBinding::from_sdp(&exact).is_ok());
    let over = format!("{exact}x");
    assert_eq!(
        SdpBinding::from_sdp(&over).err().unwrap().to_string(),
        "invalid SDP size/content"
    );
    let setup = Setup::new();
    let signed = setup.offer("request-exact", &exact, 1_000);
    assert!(setup.gate.accept_offer(&signed, &exact, 1_000).is_ok());
    let signed_over = setup.offer("request-over", &exact, 1_000);
    assert_eq!(
        setup
            .gate
            .accept_offer(&signed_over, &over, 1_000)
            .err()
            .unwrap()
            .to_string(),
        "invalid SDP size/content"
    );
}

/// MAX_SAFE_INTEGER (lib.rs:21) is 2^53 - 1, the largest integer a JSON peer carries exactly.
#[test]
fn safe_integer_bound_is_two_to_the_53_minus_one() {
    assert_eq!(MAX_SAFE_INTEGER, (1u64 << 53) - 1);
}

/// SCOPES (lib.rs:22) is the whole scope vocabulary, and validate_scopes caps a request at its
/// length (lib.rs:58). A sixth entry is refused for its length before any name is checked.
#[test]
fn scope_vocabulary_is_five_names_and_requests_are_capped_at_five() {
    assert_eq!(
        SCOPES,
        ["execute", "files", "input", "lifecycle", "observe"]
    );
    let all: Vec<String> = SCOPES.iter().map(|s| s.to_string()).collect();
    assert!(validate_scopes(&all).is_ok());
    let mut six = all;
    six.push("observe".into());
    assert_eq!(
        validate_scopes(&six).err().unwrap().to_string(),
        "invalid scopes"
    );
}

/// identifier() (lib.rs:46-54) caps request, session and VM identifiers at 128 bytes.
#[test]
fn identifiers_are_capped_at_128_bytes() {
    assert!(identifier(&"a".repeat(128)).is_ok());
    assert_eq!(
        identifier(&"a".repeat(129)).err().unwrap().to_string(),
        "invalid identifier"
    );
}

/// Challenge lifetime: 60 s. challenge() issues expires_at = issued_at + 60 (lib.rs:579), and
/// Challenge::validate refuses any longer lifetime (lib.rs:226).
#[test]
fn challenge_lifetime_is_sixty_seconds() {
    let setup = Setup::new();
    let mut challenge = setup.challenge(1_000);
    assert_eq!(challenge.expires_at - challenge.issued_at, 60);
    assert!(challenge.validate().is_ok());
    challenge.expires_at += 1;
    assert_eq!(
        challenge.validate().err().unwrap().to_string(),
        "invalid challenge lifetime"
    );
}

/// Outstanding challenges: at most 256 (lib.rs:564). The 257th is refused for capacity.
#[test]
fn challenge_capacity_is_256_outstanding() {
    let setup = Setup::new();
    for _ in 0..256 {
        setup.challenge(1_000);
    }
    let refused = setup
        .gate
        .challenge(
            &setup.grant.grant.device_id,
            &setup.grant.grant.vm_id,
            vec!["observe".into()],
            1_000,
        )
        .expect_err("the 257th challenge must be refused");
    assert_eq!(refused.to_string(), "challenge capacity");
}

/// Pending plus active sessions: at most 256 (lib.rs:605-606 for offers, lib.rs:650 for answers).
/// A 257th offer is refused by accept_offer for capacity, before its challenge is examined.
#[test]
fn session_capacity_is_256_pending_or_active() {
    let setup = Setup::new();
    let text = sdp("11");
    for i in 0..256 {
        let signed = setup.offer(&format!("request-{i}"), &text, 1_000);
        let accepted = setup.gate.accept_offer(&signed, &text, 1_000).unwrap();
        setup
            .gate
            .answer(
                accepted,
                format!("session-{i}"),
                &sdp("22"),
                &setup.host,
                1_000,
            )
            .unwrap();
    }
    let signed = setup.offer("request-256", &text, 1_000);
    assert_eq!(
        setup
            .gate
            .accept_offer(&signed, &text, 1_000)
            .err()
            .unwrap()
            .to_string(),
        "session capacity"
    );
}

/// Grant capacity: at most 1024 grants (lib.rs:480), and at most 1024 persisted versions
/// (lib.rs:440). Both are refused before any entry is verified, so 1025 placeholders are enough.
#[test]
fn grant_and_persisted_version_capacity_is_1024() {
    let setup = Setup::new();
    let placeholder = SignedGrant {
        grant: Grant {
            host_id: String::new(),
            device_id: String::new(),
            device_public_key: String::new(),
            vm_id: "vm-bounds".into(),
            scopes: Vec::new(),
            version: 1,
        },
        signature: String::new(),
    };
    let grants = vec![placeholder; 1025];
    assert_eq!(
        setup
            .gate
            .replace_grants(&setup.host_public, &grants)
            .err()
            .unwrap()
            .to_string(),
        "grant capacity"
    );
    let versions: BTreeMap<String, u64> = (0..1025).map(|i| (format!("key-{i}"), 1)).collect();
    assert_eq!(
        SessionGate::with_versions(&setup.host_public, &[], &versions)
            .err()
            .unwrap()
            .to_string(),
        "grant history capacity"
    );
}

/// A pending session can be confirmed only until its challenge expires: confirm() prunes it at
/// expires_at (lib.rs:777-784, 664-676). At expires_at - 1 the confirmation is still examined and
/// fails on its transcript; at expires_at the pending entry is gone.
#[test]
fn pending_session_confirmation_window_ends_with_the_challenge() {
    let setup = Setup::new();
    let text = sdp("11");
    let signed = setup.offer("request-confirm", &text, 1_000);
    let expires_at = signed.intent.challenge.expires_at;
    let accepted = setup.gate.accept_offer(&signed, &text, 1_000).unwrap();
    setup
        .gate
        .answer(
            accepted,
            "session-confirm".into(),
            &sdp("22"),
            &setup.host,
            1_000,
        )
        .unwrap();
    let wrong = Confirmation {
        transcript_sha256: "00".repeat(32),
        signature: String::new(),
    };
    assert_eq!(
        setup
            .gate
            .confirm("session-confirm", &wrong, expires_at - 1)
            .err()
            .unwrap()
            .to_string(),
        "transcript substituted"
    );
    assert_eq!(
        setup
            .gate
            .confirm("session-confirm", &wrong, expires_at)
            .err()
            .unwrap()
            .to_string(),
        "pending session unavailable"
    );
}

/// ICE credential lifetime: at most 600 s (ice.rs:31). Servers: at most four per configuration
/// (ice.rs:34). URLs: one to eight per server (ice.rs:45), and eight unique across the whole
/// configuration (ice.rs:58).
#[test]
fn ice_config_lifetime_server_and_url_counts_are_capped() {
    assert!(ice_config(vec![turn(&[3478], "user", "credential")])
        .canonical()
        .is_ok());
    let mut long_lived = ice_config(vec![turn(&[3478], "user", "credential")]);
    long_lived.expires_at = 701;
    assert_eq!(
        long_lived.canonical().err().unwrap().to_string(),
        "invalid ICE lifetime"
    );

    let four: Vec<_> = (0..4)
        .map(|i| turn(&[3478 + i], "user", "credential"))
        .collect();
    assert!(ice_config(four).canonical().is_ok());
    let five: Vec<_> = (0..5)
        .map(|i| turn(&[3478 + i], "user", "credential"))
        .collect();
    assert_eq!(
        ice_config(five).canonical().err().unwrap().to_string(),
        "ICE server count exceeded"
    );

    let eight: Vec<u32> = (0..8).map(|i| 3478 + i).collect();
    assert!(ice_config(vec![turn(&eight, "user", "credential")])
        .canonical()
        .is_ok());
    let nine: Vec<u32> = (0..9).map(|i| 3478 + i).collect();
    assert_eq!(
        ice_config(vec![turn(&nine, "user", "credential")])
            .canonical()
            .err()
            .unwrap()
            .to_string(),
        "invalid ICE URL count"
    );
    assert_eq!(
        ice_config(vec![turn(&[], "user", "credential")])
            .canonical()
            .err()
            .unwrap()
            .to_string(),
        "invalid ICE URL count"
    );

    let eight_across_four = vec![
        turn(&[3478, 3479], "user", "credential"),
        turn(&[3480, 3481], "user", "credential"),
        turn(&[3482, 3483], "user", "credential"),
        turn(&[3484, 3485], "user", "credential"),
    ];
    assert!(ice_config(eight_across_four).canonical().is_ok());
    let nine_across_four = vec![
        turn(&[3478, 3479, 3480], "user", "credential"),
        turn(&[3481, 3482], "user", "credential"),
        turn(&[3483, 3484], "user", "credential"),
        turn(&[3485, 3486], "user", "credential"),
    ];
    assert_eq!(
        ice_config(nine_across_four)
            .canonical()
            .err()
            .unwrap()
            .to_string(),
        "duplicate or excessive ICE URLs"
    );
}

/// ICE URL length: at most 128 bytes (ice.rs:102). A 128-byte URL passes that check and then fails
/// the port check, so only a 129-byte URL reports "ICE URL too long". The fixed text of a TURN URL
/// is 29 bytes, so the port digits fill the rest.
#[test]
fn ice_url_length_is_capped_at_128_bytes() {
    let url = |len: usize| format!("turn:127.0.0.1:{}?transport=udp", "1".repeat(len - 29));
    assert_eq!(url(129).len(), 129);
    assert_eq!(
        ice::validate_local_url(&url(129))
            .err()
            .unwrap()
            .to_string(),
        "ICE URL too long"
    );
    assert_eq!(
        ice::validate_local_url(&url(128))
            .err()
            .unwrap()
            .to_string(),
        "invalid ICE port"
    );
}

/// TURN username at most 256 bytes and credential at most 1024 bytes (ice.rs:63-66).
#[test]
fn turn_username_and_credential_lengths_are_capped() {
    assert!(
        ice_config(vec![turn(&[3478], &"u".repeat(256), &"c".repeat(1024))])
            .canonical()
            .is_ok()
    );
    assert_eq!(
        ice_config(vec![turn(&[3478], &"u".repeat(257), &"c".repeat(1024))])
            .canonical()
            .err()
            .unwrap()
            .to_string(),
        "invalid short-lived TURN credential"
    );
    assert_eq!(
        ice_config(vec![turn(&[3478], "user", &"c".repeat(1025))])
            .canonical()
            .err()
            .unwrap()
            .to_string(),
        "invalid short-lived TURN credential"
    );
}

/// ICE port: 1 to 65535 (ice.rs:117-125).
#[test]
fn ice_port_is_one_to_sixty_five_thousand_five_hundred_thirty_five() {
    assert!(ice::validate_local_url("stun:127.0.0.1:65535").is_ok());
    assert_eq!(
        ice::validate_local_url("stun:127.0.0.1:65536")
            .err()
            .unwrap()
            .to_string(),
        "invalid ICE port"
    );
    assert_eq!(
        ice::validate_local_url("stun:127.0.0.1:0")
            .err()
            .unwrap()
            .to_string(),
        "invalid ICE port"
    );
}

/// Credential time left at check: at least 210 s (ice.rs:90). 210 s remaining passes; 209 s fails.
#[test]
fn ice_credential_needs_210_seconds_remaining() {
    let config = ice_config(vec![turn(&[3478], "user", "credential")]);
    assert!(config.check_time(490).is_ok());
    assert_eq!(
        config.check_time(491).err().unwrap().to_string(),
        "ICE credentials expired or insufficient lifetime"
    );
}
