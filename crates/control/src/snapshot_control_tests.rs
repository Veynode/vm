use super::*;
use std::os::unix::fs::PermissionsExt;
struct Fixture {
    _temp: tempfile::TempDir,
    path: PathBuf,
    ops: Operations,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let ops = Operations::open(&path).unwrap();
        Self {
            _temp: temp,
            path,
            ops,
        }
    }
}
#[test]
fn durable_operation_fences_vm_until_explicit_safe_terminal_receipt() {
    let f = Fixture::new();
    let op = f
        .ops
        .begin("linux1", "before", "create", Some("1".repeat(64)))
        .unwrap();
    assert!(f.ops.active_vm("linux1"));
    assert!(!f.ops.active_vm("linux2"));
    assert!(f.ops.begin("linux1", "other", "delete", None).is_err());
    op.phase("checking_support").unwrap();
    op.phase("capturing").unwrap();
    let status = f.ops.status("linux1", op.id()).unwrap();
    assert_eq!(status["state"], "running");
    assert_eq!(status["phase"], "capturing");
    assert!(f.ops.status("linux2", op.id()).is_err());
    op.finish(json!({"capture_verified":true,"restore_verified":false}))
        .unwrap();
    assert!(!f.ops.active_vm("linux1"));
    assert_eq!(
        f.ops.status("linux1", op.id()).unwrap()["result"]["restore_verified"],
        false
    );
    assert!(op.phase("capturing").is_err());
    let reopened = Operations::open(&f.path).unwrap();
    assert!(!reopened.active_vm("linux1"));
    assert_eq!(
        reopened.status("linux1", op.id()).unwrap()["state"],
        "completed"
    );
}
#[test]
fn restart_and_timeout_are_recovery_obligations_never_completion_evidence() {
    let f = Fixture::new();
    let op = f
        .ops
        .begin("linux1", "before", "create", Some("1".repeat(64)))
        .unwrap();
    op.phase("capturing").unwrap();
    let reopened = Operations::open(&f.path).unwrap();
    assert!(reopened.active_vm("linux1"));
    assert_eq!(
        reopened.status("linux1", op.id()).unwrap()["state"],
        "recovery_required"
    );
    assert!(reopened
        .begin("linux1", "replacement", "create", None)
        .is_err());
    // Reopening an already durable recovery fence does not exhaust its receipt
    // capacity through repeated controller restarts.
    for _ in 0..35 {
        let again = Operations::open(&f.path).unwrap();
        assert!(again.active_vm("linux1"));
    }
}
#[test]
fn known_pre_pause_refusal_releases_fence_but_unknown_resume_retains_it() {
    let f = Fixture::new();
    let known = f.ops.begin("linux1", "known", "create", None).unwrap();
    known.fail("backend_support_refused", false).unwrap();
    assert!(!f.ops.active_vm("linux1"));
    let unknown = f.ops.begin("linux1", "unknown", "create", None).unwrap();
    unknown.phase("resuming").unwrap();
    unknown.fail("resume_unconfirmed", true).unwrap();
    assert!(f.ops.active_vm("linux1"));
    let reopened = Operations::open(&f.path).unwrap();
    assert!(reopened.active_vm("linux1"));
}
#[test]
fn altered_missing_or_reordered_receipts_fail_closed() {
    for variant in 0..3 {
        let f = Fixture::new();
        let op = f.ops.begin("linux1", "before", "create", None).unwrap();
        op.phase("checking_support").unwrap();
        let path = f.path.join("snapshot-operations").join(op.id());
        match variant {
            0 => {
                fs::remove_file(path.join("0000.json")).unwrap();
            }
            1 => {
                let mut value: Value =
                    serde_json::from_slice(&fs::read(path.join("0001.json")).unwrap()).unwrap();
                value["id"] = json!("other-vm");
                fs::write(path.join("0001.json"), serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => {
                fs::rename(path.join("0001.json"), path.join("0002.json")).unwrap();
            }
        }
        assert!(Operations::open(&f.path).is_err());
    }
}
#[test]
fn invalid_operation_names_or_cross_vm_ids_have_no_durable_side_effect() {
    let f = Fixture::new();
    for name in ["../disk.raw", "", ".hidden", "/absolute", "a/b"] {
        assert!(f.ops.begin("linux1", name, "create", None).is_err());
    }
    assert!(f.ops.begin("../linux1", "before", "create", None).is_err());
    assert!(f.ops.begin("linux1", "before", "other", None).is_err());
    assert!(fs::read_dir(f.path.join("snapshot-operations"))
        .unwrap()
        .next()
        .is_none());
}
fn config() -> Config {
    serde_json::from_value(json!({"id":"linux1","guest":"linux","cpus":2,"memory_mib":1024,"machine_identity_dir":"/identity","disk":"/disk.raw"})).unwrap()
}
fn support() -> Value {
    json!({"can_save_restore":true,"support_query_passed":true,"runtime_restore_verified":false,"disk_bytes":512,
    "machine_identity_sha256":"1".repeat(64),"configuration_sha256":"2".repeat(64),"device_configuration_sha256":"3".repeat(64),
    "backend_version":"macos-fixture-build","machine":"Apple-VZGeneric-v1","host_architecture":"aarch64"})
}
#[test]
fn apple_support_projection_requires_explicit_identity_and_support_not_restore_claim() {
    let c = config();
    let s = support();
    let compatibility = apple_compatibility(&c, &s, "4".repeat(64)).unwrap();
    assert_eq!(compatibility.backend, "apple_virtualization");
    assert_eq!(compatibility.config_sha256, "2".repeat(64));
    assert_eq!(s["runtime_restore_verified"], false);
    let mut no_identity = c.clone();
    no_identity.machine_identity_dir = None;
    assert!(apple_compatibility(&no_identity, &s, "4".repeat(64)).is_err());
    for key in [
        "can_save_restore",
        "support_query_passed",
        "configuration_sha256",
        "machine_identity_sha256",
        "disk_bytes",
    ] {
        let mut missing = s.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            apple_compatibility(&c, &missing, "4".repeat(64)).is_err(),
            "accepted missing {key}"
        );
    }
}
#[test]
fn resume_ack_requires_original_capture_and_actual_running_state() {
    let id = "1".repeat(32);
    assert!(verify_resume(&json!({"capture_id":id,"state":"running"}), &id).is_ok());
    assert!(verify_resume(&json!({"capture_id":"2".repeat(32),"state":"running"}), &id).is_err());
    assert!(verify_resume(&json!({"capture_id":id,"state":"paused"}), &id).is_err());
}

#[test]
fn unpublished_admission_bytes_are_preserved_without_becoming_operations() {
    for partial in [None, Some(b"{\"version\":".as_slice())] {
        let f = Fixture::new();
        let nonce = "a".repeat(32);
        let name = format!(".pending-admission-{nonce}");
        let (staging, _) = f.ops.directory.child(&name, true).unwrap();
        if let Some(bytes) = partial {
            let file = staging.path.join(format!(".pending-{}", "b".repeat(32)));
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)
                .unwrap()
                .write_all(bytes)
                .unwrap();
        }
        let reopened = Operations::open(&f.path).unwrap();
        assert!(!reopened.active_vm("linux1"));
        assert!(reopened.status("linux1", &nonce).is_err());
        assert!(staging.path.is_dir());
        if let Some(bytes) = partial {
            assert_eq!(
                fs::read(staging.path.join(format!(".pending-{}", "b".repeat(32)))).unwrap(),
                bytes
            );
        }
        let next = reopened.begin("linux1", "after", "create", None).unwrap();
        let public = f.path.join("snapshot-operations").join(next.id());
        assert!(public.join("0000.json").is_file());
        assert!(!f
            .path
            .join("snapshot-operations")
            .join(format!(".pending-admission-{}", next.id()))
            .exists());
        let again = Operations::open(&f.path).unwrap();
        assert_eq!(
            again.status("linux1", next.id()).unwrap()["state"],
            "recovery_required"
        );
    }
}

#[test]
fn unpublished_admission_does_not_hide_unsafe_directory_or_malformed_name() {
    use std::os::unix::fs::symlink;
    for malformed in [false, true] {
        let f = Fixture::new();
        let path = f.path.join("snapshot-operations");
        if malformed {
            fs::create_dir(path.join(".pending-admission-not-an-id")).unwrap();
        } else {
            symlink(
                &f.path,
                path.join(format!(".pending-admission-{}", "a".repeat(32))),
            )
            .unwrap();
        }
        assert!(Operations::open(&f.path).is_err());
    }
}

#[test]
fn failure_diagnostics_are_bounded_private_and_preserve_valid_operation_history() {
    let f = Fixture::new();
    let op = f.ops.begin("linux1", "before", "create", None).unwrap();
    let secret = "private /host/path ".to_owned() + &"é".repeat(3000);
    op.fail_error(
        "capture_failed_recovery_required",
        true,
        &anyhow::anyhow!(secret.clone()),
    )
    .unwrap();
    assert!(!f
        .ops
        .status("linux1", op.id())
        .unwrap()
        .to_string()
        .contains("private"));
    let path = f
        .path
        .join("snapshot-operations")
        .join(op.id())
        .join("0001.json");
    let record: Record = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let diagnostic = record.local_error.unwrap();
    assert!(diagnostic.len() <= 4096 && secret.starts_with(&diagnostic));
    assert!(Operations::open(&f.path).unwrap().active_vm("linux1"));
}

#[test]
fn failed_failure_receipt_is_reported_and_original_cause_survives_recovery() {
    let f = Fixture::new();
    let op = f
        .ops
        .begin("linux1", "before", "create", Some("1".repeat(64)))
        .unwrap();
    op.phase("capturing").unwrap();
    let journal = f.path.join("snapshot-operations").join(op.id());
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o500)).unwrap();
    let failed = op.fail_error(
        "capture_failed_recovery_required",
        true,
        &anyhow::anyhow!("private-native-enospc"),
    );
    assert!(failed.is_err());
    let again = op.fail_error(
        "snapshot_outcome_unconfirmed",
        true,
        &anyhow::anyhow!("secondary-journal-error"),
    );
    assert!(again.is_err());
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o700)).unwrap();
    let projected = f.ops.status("linux1", op.id()).unwrap();
    assert_eq!(projected["state"], "recovery_required");
    assert_eq!(projected["failure_persisted"], false);
    assert_eq!(projected["durable_state"], "running");
    assert!(!projected.to_string().contains("private-native-enospc"));
    assert!(f.ops.active_vm("linux1"));
    let serialized = serde_json::to_vec(&f.ops.records.lock().unwrap()[op.id()]).unwrap();
    assert!(!String::from_utf8_lossy(&serialized).contains("private-native-enospc"));
    let decoded: Record = serde_json::from_slice(&serialized).unwrap();
    assert!(decoded.public().get("failure_persisted").is_none());
    assert_eq!(decoded.state, "running");
    // This ledger-only test supplies no backend acknowledgement or Guest proof.
    op.update(
        "source_resumed_after_failure",
        "failed",
        None,
        Some("capture_source_resumed"),
        None,
    )
    .unwrap();
    let row = f.ops.records.lock().unwrap()[op.id()].clone();
    assert!(row
        .local_error
        .as_ref()
        .unwrap()
        .contains("private-native-enospc"));
    assert!(!row
        .local_error
        .as_ref()
        .unwrap()
        .contains("secondary-journal-error"));
    assert!(f
        .ops
        .status("linux1", op.id())
        .unwrap()
        .get("failure_persisted")
        .is_none());
}

#[test]
fn deferred_failure_can_be_persisted_without_overwriting_cause_or_utf8_bound() {
    let f = Fixture::new();
    let op = f
        .ops
        .begin("linux1", "before", "create", Some("1".repeat(64)))
        .unwrap();
    let journal = f.path.join("snapshot-operations").join(op.id());
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o500)).unwrap();
    let cause = "原始磁盘写满".repeat(1000);
    assert!(op
        .fail_error(
            "original_capture_failed",
            true,
            &anyhow::anyhow!(cause.clone())
        )
        .is_err());
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o700)).unwrap();
    op.fail_error(
        "secondary_journal_failed",
        true,
        &anyhow::anyhow!("secondary"),
    )
    .unwrap();
    let row = f.ops.records.lock().unwrap()[op.id()].clone();
    assert_eq!(row.error_code.as_deref(), Some("original_capture_failed"));
    assert_eq!(
        row.local_error.as_ref().unwrap(),
        &bounded_diagnostic(&cause)
    );
    assert!(row.local_error.as_ref().unwrap().len() <= 4096);
    assert!(f
        .ops
        .status("linux1", op.id())
        .unwrap()
        .get("failure_persisted")
        .is_none());
    assert!(f.ops.active_vm("linux1"));
}
