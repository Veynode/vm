use serde_json::{json, Value};
use vm_session_auth::{control::SignedControl, hex, verify, MAX_SAFE_INTEGER};
fn fixture() -> Value {
    serde_json::from_str(include_str!("control-golden.json")).unwrap()
}
#[test]
fn all_control_goldens_match_real_signature_and_reject_tamper() {
    let fixture = fixture();
    for case in fixture["cases"].as_array().unwrap() {
        let signed: SignedControl = serde_json::from_value(case["signed"].clone()).unwrap();
        let bytes = signed.request.canonical().unwrap();
        assert_eq!(hex(&bytes), case["canonical_hex"]);
        verify(
            fixture["device_public_key"].as_str().unwrap(),
            &bytes,
            &signed.signature,
        )
        .unwrap();
        let mut changed = signed.request.clone();
        changed.sequence += 1;
        assert!(verify(
            fixture["device_public_key"].as_str().unwrap(),
            &changed.canonical().unwrap(),
            &signed.signature
        )
        .is_err());
        signed.request.check_time(1700000010).unwrap();
        assert!(signed.request.check_time(1700000015).is_err());
        assert!(signed.request.check_time(1700000009).is_err());
    }
}
#[test]
fn control_rejects_ambiguous_and_out_of_bound_values() {
    let f = fixture();
    let cases = f["cases"].as_array().unwrap();
    let original = cases
        .iter()
        .find(|c| c["signed"]["request"]["action"]["kind"] == "key")
        .unwrap()["signed"]
        .clone();
    for keys in [
        json!([""]),
        json!(["a", "a"]),
        json!(["é"]),
        json!(["LEFT"]),
        json!([]),
    ] {
        let mut v = original.clone();
        v["request"]["action"]["keys"] = keys;
        let s: SignedControl = serde_json::from_value(v).unwrap();
        assert!(s.request.canonical().is_err());
    }
    let mut v = original.clone();
    v["request"]["action"]["epoch"] = json!(1);
    assert!(serde_json::from_value::<SignedControl>(v).is_err());
    let mut v = original.clone();
    v["request"]["action"]["frame"]["source_generation"] = json!(MAX_SAFE_INTEGER + 1);
    assert!(serde_json::from_value::<SignedControl>(v)
        .unwrap()
        .request
        .canonical()
        .is_err());
    let mut v = original.clone();
    v["request"]["action"]["frame"]["width"] = json!(8193);
    assert!(serde_json::from_value::<SignedControl>(v)
        .unwrap()
        .request
        .canonical()
        .is_err());
    let mut v = original;
    v["request"]["action"]["vm_id"] = json!("foreign");
    assert!(serde_json::from_value::<SignedControl>(v).is_err());
}
