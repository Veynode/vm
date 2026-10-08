//! Explicitly opted-in actual Keychain test. All operations target one fresh test identity.
#![cfg(target_os = "macos")]
use std::io::Write;
use std::process::{Command, Stdio};
use vm_session_auth::{keychain::*, *};
#[test]
#[ignore = "creates and removes only a fresh test Keychain identity; run explicitly"]
fn actual_same_binary_cli_pair_approve_revoke_and_cross_binary_denied() {
    disable_interaction_for_process().unwrap();
    let namespace = format!("test-{}", random32());
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_vm-pair"))
        .args(["init-host", state.to_str().unwrap(), &namespace])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let identity = KeychainIdentity::open(&namespace).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let public = identity.public_key().unwrap();
        assert!(KeychainIdentity::create(&namespace).is_err());
        // This unsigned development harness intentionally has a different code identity.
        let denied = identity
            .sign(b"Veynode/cross-executable-access-test/v1\0")
            .unwrap_err();
        assert!(
            denied.to_string().contains("AUTH_DENIED") || denied.to_string().contains("-25293"),
            "{denied:#}"
        );
        let golden: serde_json::Value = serde_json::from_str(include_str!("golden.json")).unwrap();
        let pairing = Pairing {
            host_public_key: public.clone(),
            device_public_key: golden["device_public_key"].as_str().unwrap().into(),
            nonce: random32(),
            vm_id: "test-vm".into(),
            scopes: vec!["observe".into()],
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_vm-pair"))
            .args([
                "approve",
                state.to_str().unwrap(),
                &namespace,
                &pairing.digest().unwrap(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&pairing).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "same-executable CLI signing: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let signed: SignedGrant = serde_json::from_slice(&output.stdout).unwrap();
        signed.verify(&public).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_vm-pair"))
            .args([
                "revoke",
                state.to_str().unwrap(),
                &namespace,
                &signed.grant.device_id,
                "test-vm",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(store::Store::open(&state)
            .unwrap()
            .load()
            .unwrap()
            .grants
            .is_empty());
    }));
    identity.delete_test_identity(&namespace).unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e)
    }
}
