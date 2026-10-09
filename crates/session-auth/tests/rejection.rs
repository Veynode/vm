use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::{json, Value};
use vm_session_auth::{rejection::*, *};
struct Key(SigningKey);
impl Key {
    fn new(n: u8) -> Self {
        let mut bytes = [0; 32];
        bytes[31] = n;
        Self(SigningKey::from_bytes((&bytes).into()).unwrap())
    }
}
impl Identity for Key {
    fn public_key(&self) -> anyhow::Result<String> {
        Ok(hex(self
            .0
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()))
    }
    fn sign(&self, bytes: &[u8]) -> anyhow::Result<String> {
        let signature: Signature = self.0.sign(bytes);
        Ok(hex(&signature.to_bytes()))
    }
}
fn fixture() -> (Value, OfferIntent) {
    let value: Value = serde_json::from_str(include_str!("offer-rejection-golden.json")).unwrap();
    let intent = serde_json::from_value(value["expected_intent"].clone()).unwrap();
    (value, intent)
}
#[test]
fn all_fixed_rejections_match_canonical_golden_and_real_p256_signature() {
    let (fixture, intent) = fixture();
    let public = fixture["host_public_key"].as_str().unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let signed: SignedOfferRejection = serde_json::from_value(case["signed"].clone()).unwrap();
        assert_eq!(
            hex(&signed.rejection.canonical().unwrap()),
            case["canonical_hex"]
        );
        assert!(signed
            .rejection
            .canonical()
            .unwrap()
            .starts_with(b"Veynode/offer-rejection/v1\0"));
        signed.verify(public, &intent).unwrap();
        let recreated = sign(intent.clone(), &signed.rejection.code, &Key::new(1)).unwrap();
        assert_eq!(recreated.signature, signed.signature);
        assert_ne!(
            signed.rejection.canonical().unwrap(),
            intent.canonical().unwrap()
        );
    }
}
#[test]
fn rejection_cannot_replay_across_request_challenge_device_vm_offer_or_codec() {
    let (fixture, intent) = fixture();
    let public = fixture["host_public_key"].as_str().unwrap();
    let signed: SignedOfferRejection =
        serde_json::from_value(fixture["cases"][0]["signed"].clone()).unwrap();
    let mut variants = Vec::new();
    let mut v = intent.clone();
    v.request_id = "other-request".into();
    variants.push(v);
    let mut v = intent.clone();
    v.codec = if v.codec == "h264" { "h265" } else { "h264" }.into();
    variants.push(v);
    let mut v = intent.clone();
    v.offer.sha256 = hash(b"different complete SDP");
    variants.push(v);
    let mut v = intent.clone();
    v.offer.fingerprint_sha256 = "cc".repeat(32);
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.nonce = "ee".repeat(32);
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.instance_nonce = "ee".repeat(32);
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.host_id = "ee".repeat(32);
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.device_id = "ee".repeat(32);
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.vm_id = "other-vm".into();
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.grant_version += 1;
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.scopes = vec!["execute".into(), "observe".into()];
    variants.push(v);
    let mut v = intent.clone();
    v.challenge.issued_at += 1;
    v.challenge.expires_at += 1;
    variants.push(v);
    for changed in variants {
        assert!(signed.verify(public, &changed).is_err());
    }
}
#[test]
fn fixed_statement_and_host_signature_cannot_be_repurposed_as_cleanup_or_authority() {
    let (fixture, intent) = fixture();
    let public = fixture["host_public_key"].as_str().unwrap();
    let original: SignedOfferRejection =
        serde_json::from_value(fixture["cases"][0]["signed"].clone()).unwrap();
    assert!(sign(intent.clone(), "arbitrary_error", &Key::new(1)).is_err());
    assert!(sign(intent.clone(), CODES[0], &Key::new(2)).is_err());
    assert!(original
        .verify(&Key::new(2).public_key().unwrap(), &intent)
        .is_err());
    for field in ["code", "outcome", "version", "signature"] {
        let mut changed = original.clone();
        match field {
            "code" => changed.rejection.code = CODES[1].into(),
            "outcome" => changed.rejection.outcome = "cleanup_confirmed".into(),
            "version" => changed.rejection.version = 2,
            _ => changed.signature = "00".repeat(64),
        }
        assert!(changed.verify(public, &intent).is_err(), "{field}");
    }
    let mut changed = original.clone();
    changed.signature = changed.signature.to_uppercase();
    assert!(changed.verify(public, &intent).is_err());
    changed.signature = "00".repeat(65);
    assert!(changed.verify(public, &intent).is_err());
}
#[test]
fn wire_rejects_unknown_missing_duplicate_and_wrong_typed_fields() {
    let (fixture, _) = fixture();
    let original = fixture["cases"][0]["signed"].clone();
    for path in [
        vec![],
        vec!["rejection"],
        vec!["rejection", "intent"],
        vec!["rejection", "intent", "challenge"],
    ] {
        let mut value = original.clone();
        let mut target = &mut value;
        for field in path {
            target = &mut target[field];
        }
        target["extra"] = json!(true);
        assert!(serde_json::from_value::<SignedOfferRejection>(value).is_err());
    }
    for bad in [json!(true), json!(1.5), Value::Null] {
        let mut value = original.clone();
        value["rejection"]["version"] = bad;
        assert!(serde_json::from_value::<SignedOfferRejection>(value).is_err());
    }
    for field in ["signature", "rejection"] {
        let mut value = original.clone();
        value.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<SignedOfferRejection>(value).is_err());
    }
    let raw = serde_json::to_string(&original).unwrap();
    let duplicate = format!("{{\"signature\":\"{}\",{}", "00".repeat(64), &raw[1..]);
    assert!(serde_json::from_str::<SignedOfferRejection>(&duplicate).is_err());
}
#[test]
fn actual_cli_verifies_only_exact_offer_rejection() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let (fixture, intent) = fixture();
    for valid in [true, false] {
        let mut expected = intent.clone();
        if !valid {
            expected.request_id = "another-request".into();
        }
        let request = json!({"kind":"offer_rejection","host_public_key":fixture["host_public_key"],"expected_intent":expected,"signed":fixture["cases"][0]["signed"]});
        let mut child = Command::new(env!("CARGO_BIN_EXE_vm-pair"))
            .arg("verify")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.success(), valid);
        if valid {
            let result: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["authorization_checked"], false);
        }
    }
}
