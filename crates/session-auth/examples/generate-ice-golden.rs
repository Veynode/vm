//! Fixed public test identities and synthetic credential only.
use anyhow::Result;
use p256::ecdsa::{signature::Signer, SigningKey};
use serde_json::json;
use vm_session_auth::{ice::*, *};
struct Key(SigningKey);
impl Key {
    fn new(v: u8) -> Self {
        let mut b = [0; 32];
        b[31] = v;
        Self(SigningKey::from_bytes((&b).into()).unwrap())
    }
}
impl Identity for Key {
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
fn main() -> Result<()> {
    let original: serde_json::Value = serde_json::from_str(include_str!("../tests/golden.json"))?;
    let answer: SignedAnswer = serde_json::from_value(original["signed_answer"].clone())?;
    let host = Key::new(1);
    let device = Key::new(2);
    let request = IceRequest {
        challenge: answer.transcript.intent.challenge,
        request_nonce: "ab".repeat(32),
    };
    let signed = SignedIceRequest {
        signature: device.sign(&request.canonical()?)?,
        request: request.clone(),
    };
    let config = IceConfig {
        version: 1,
        transport_policy: "relay".into(),
        issued_at: 1700000000,
        expires_at: 1700000600,
        ice_servers: vec![IceServer {
            urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
            username: "1700000600:test-device".into(),
            credential: "synthetic-fixture-only".into(),
        }],
    };
    let response = SignedIceConfig::new(request, config.clone(), &host)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"fixture_only":true,"now":1700000010u64,"host_public_key":host.public_key()?,"device_public_key":device.public_key()?,"signed_request":signed,"response":response,"request_canonical_hex":hex(&response.request.canonical()?),"config_canonical_hex":hex(&config.canonical()?),"response_canonical_hex":hex(&SignedIceConfig::signing_bytes(&response.request,&config)?)})
        )?
    );
    Ok(())
}
