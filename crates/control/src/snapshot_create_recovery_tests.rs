use super::*;
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _temp: tempfile::TempDir,
    path: PathBuf,
    config: Config,
    ops: Operations,
    operation: Operation,
    record: Record,
}
fn write(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn fixture(with_disk: bool) -> Fixture {
    fixture_named(with_disk, "disk.raw")
}
fn fixture_named(with_disk: bool, disk_name: &str) -> Fixture {
    fixture_phase(with_disk, disk_name, "checking_support")
}
fn fixture_phase(with_disk: bool, disk_name: &str, phase: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let identity = path.join("identity");
    fs::create_dir(&identity).unwrap();
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o700)).unwrap();
    write(&identity.join("linux-identity-v1.json"), br#"{"schema":1,"vm":"linux1","machine_base64":"b2ZmbGluZSBmaXh0dXJl","mac_address":"02:11:22:33:44:55"}"#);
    write(&identity.join(".linux-snapshot.lock"), b"");
    let disk = with_disk.then(|| path.join(disk_name));
    if let Some(disk) = &disk {
        write(disk, &[0; 512]);
    }
    let config: Config = serde_json::from_value(
        json!({"id":"linux1","guest":"linux","cpus":1,"memory_mib":512,
        "machine_identity_dir":identity,"disk":disk}),
    )
    .unwrap();
    let initial = Operations::open(&path).unwrap();
    let original = initial
        .begin_create(&config, "before", &"a".repeat(64))
        .unwrap();
    if phase == "bound_capture" {
        original
            .capturing(&"b".repeat(32), &"c".repeat(64))
            .unwrap();
    } else {
        original.phase(phase).unwrap();
    }
    let ops = Operations::open(&path).unwrap();
    let record = ops.records.lock().unwrap()[original.id()].clone();
    let operation = Operation {
        operations: ops.clone(),
        id: original.id().into(),
    };
    Fixture {
        _temp: temp,
        path,
        config,
        ops,
        operation,
        record,
    }
}
#[test]
fn interrupted_create_reconciles_only_with_bound_exclusive_retirement_proof() {
    for with_disk in [false, true] {
        let f = fixture(with_disk);
        let journal = f.path.join("snapshot-operations").join(f.operation.id());
        let before: BTreeMap<_, _> = fs::read_dir(&journal)
            .unwrap()
            .map(|p| {
                let p = p.unwrap().path();
                let bytes = fs::read(&p).unwrap();
                (p, bytes)
            })
            .collect();
        let disk_before = f.config.disk.as_ref().map(|p| fs::read(p).unwrap());
        let proof = prove(&f.path, &f.config, &f.record).unwrap();
        proof.verify().unwrap();
        assert_eq!(proof.receipt["restored"], false);
        assert_eq!(proof.receipt["snapshot_result_verified"], false);
        assert!(prove(&f.path, &f.config, &f.record).is_err());
        if let Some(path) = &f.config.disk {
            assert!(qemu_disk::DiskGuard::open(path).is_err());
        }
        f.operation
            .update(
                "reconciled_after_retirement",
                "failed",
                Some(proof.receipt.clone()),
                Some("create_attempt_retired"),
                None,
            )
            .unwrap();
        drop(proof);
        assert!(!f.ops.active_vm("linux1"));
        for (path, bytes) in before {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert_eq!(
            f.config.disk.as_ref().map(|p| fs::read(p).unwrap()),
            disk_before
        );
        assert_eq!(
            Operations::open(&f.path)
                .unwrap()
                .status("linux1", f.operation.id())
                .unwrap()["state"],
            "failed"
        );
    }
}
#[test]
fn live_identity_or_disk_owner_keeps_interrupted_create_fenced() {
    let f = fixture(true);
    let identity = Dir::open(f.config.machine_identity_dir.as_deref().unwrap()).unwrap();
    let lock = identity.file(".linux-snapshot.lock", false).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(prove(&f.path, &f.config, &f.record).is_err());
    drop(lock);
    let disk = qemu_disk::DiskGuard::open(f.config.disk.as_deref().unwrap()).unwrap();
    assert!(prove(&f.path, &f.config, &f.record).is_err());
    drop(disk);
    let raw = File::open(f.config.disk.as_deref().unwrap()).unwrap();
    assert_eq!(
        unsafe { libc::flock(raw.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(prove(&f.path, &f.config, &f.record).is_err());
    drop(raw);
    assert!(f.ops.active_vm("linux1"));
    prove(&f.path, &f.config, &f.record)
        .unwrap()
        .verify()
        .unwrap();
}
#[test]
fn replacement_corruption_and_changed_registration_never_supply_retirement() {
    for target in ["identity", "lock", "disk", "intent", "config"] {
        let f = fixture(true);
        let path = match target {
            "identity" => f
                .config
                .machine_identity_dir
                .as_ref()
                .unwrap()
                .join("linux-identity-v1.json"),
            "lock" => f
                .config
                .machine_identity_dir
                .as_ref()
                .unwrap()
                .join(".linux-snapshot.lock"),
            "disk" => f.config.disk.clone().unwrap(),
            "intent" => f
                .path
                .join("snapshot-operations")
                .join(f.operation.id())
                .join("create-intent.json"),
            _ => f.path.join("unused"),
        };
        let mut registered = f.config.clone();
        if target == "config" {
            registered.memory_mib += 1;
        } else if target == "intent" {
            let mut bytes = fs::read(&path).unwrap();
            bytes[0] = b'[';
            fs::write(&path, bytes).unwrap();
        } else {
            // Keep the displaced inode alive so exact inode reuse cannot make
            // this replacement test spuriously indistinguishable from original.
            let retained = path.with_extension("retained");
            let bytes = fs::read(&path).unwrap();
            fs::rename(&path, retained).unwrap();
            write(&path, &bytes);
        }
        assert!(prove(&f.path, &registered, &f.record).is_err(), "{target}");
        assert!(f.ops.active_vm("linux1"));
    }
}
#[test]
fn legacy_or_cross_vm_records_are_not_given_fabricated_witnesses() {
    let f = fixture(true);
    let mut legacy = f.record.clone();
    legacy.create_intent_sha256 = None;
    assert!(prove(&f.path, &f.config, &legacy).is_err());
    let mut other = f.record.clone();
    other.id = "linux2".into();
    assert!(prove(&f.path, &f.config, &other).is_err());
    let mut altered = f.record.clone();
    altered.worker_instance = Some("b".repeat(64));
    assert!(prove(&f.path, &f.config, &altered).is_err());
    assert!(f.ops.active_vm("linux1"));
}

#[test]
fn apple_raw_image_suffix_is_preserved_without_relaxing_qemu_registration() {
    let f = fixture_named(true, "disk.img");
    let path = f.config.disk.as_deref().unwrap();
    assert!(qemu_disk::validate_path(path).is_err());
    let proof = prove(&f.path, &f.config, &f.record).unwrap();
    proof.verify().unwrap();
    assert!(qemu_disk::DiskGuard::open_apple_raw(path).is_err());
    drop(proof);
    assert!(qemu_disk::DiskGuard::open_apple_raw(path).is_ok());
}

#[test]
fn possibly_dispatched_native_save_cannot_retire_on_process_locks_alone() {
    for phase in ["capturing", "committing", "resuming"] {
        let f = fixture_phase(true, "disk.img", phase);
        let history = f.path.join("snapshot-operations").join(f.operation.id());
        let before: BTreeMap<_, _> = fs::read_dir(&history)
            .unwrap()
            .map(|p| {
                let p = p.unwrap().path();
                let bytes = fs::read(&p).unwrap();
                (p, bytes)
            })
            .collect();
        assert!(prove(&f.path, &f.config, &f.record)
            .err()
            .unwrap()
            .to_string()
            .contains("asynchronous capture retirement"));
        assert!(f.ops.active_vm("linux1"));
        for (path, bytes) in before {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
}

fn native_receipt(f: &Fixture) -> Value {
    json!({"schema":1,"operation_id":f.operation.id(),"id":"linux1","worker_instance":"a".repeat(64),
        "capture_id":"b".repeat(32),"configuration_sha256":"c".repeat(64),
        "capture_io_drained":true,"backend_stopped":true,"normal_guest_shutdown":false})
}
#[test]
fn capture_recovery_requires_bound_durable_native_drain_and_stop_receipt() {
    let f = fixture_phase(true, "disk.img", "bound_capture");
    let path = f
        .path
        .join("snapshot-operations")
        .join(f.operation.id())
        .join("native-retired.json");
    assert!(prove(&f.path, &f.config, &f.record).is_err());
    write(&path, &serde_json::to_vec(&native_receipt(&f)).unwrap());
    let proof = prove(&f.path, &f.config, &f.record).unwrap();
    proof.verify().unwrap();
    assert_eq!(proof.receipt["native_io_retirement_verified"], true);
    f.operation
        .update(
            "reconciled_after_retirement",
            "failed",
            Some(proof.receipt.clone()),
            Some("create_attempt_retired"),
            None,
        )
        .unwrap();
    drop(proof);
    assert!(!Operations::open(&f.path).unwrap().active_vm("linux1"));
}
#[test]
fn foreign_partial_false_or_changed_retirement_receipts_never_clear_capture_fence() {
    for key in [
        "operation_id",
        "id",
        "worker_instance",
        "capture_id",
        "configuration_sha256",
        "capture_io_drained",
        "backend_stopped",
        "normal_guest_shutdown",
        "extra",
    ] {
        let f = fixture_phase(true, "disk.img", "bound_capture");
        let path = f
            .path
            .join("snapshot-operations")
            .join(f.operation.id())
            .join("native-retired.json");
        let mut receipt = native_receipt(&f);
        receipt[key] = match key {
            "capture_io_drained" | "backend_stopped" => json!(false),
            "normal_guest_shutdown" => json!(true),
            _ => json!("foreign"),
        };
        write(&path, &serde_json::to_vec(&receipt).unwrap());
        assert!(prove(&f.path, &f.config, &f.record).is_err(), "{key}");
        assert!(f.ops.active_vm("linux1"));
    }
    let f = fixture_phase(true, "disk.img", "bound_capture");
    let path = f
        .path
        .join("snapshot-operations")
        .join(f.operation.id())
        .join("native-retired.json");
    write(&path, &serde_json::to_vec(&native_receipt(&f)).unwrap());
    let proof = prove(&f.path, &f.config, &f.record).unwrap();
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b" ")
        .unwrap();
    assert!(proof.verify().is_err());
    assert!(f.ops.active_vm("linux1"));
}

#[test]
fn live_source_intent_validation_preserves_owned_locks_and_rejects_changed_nodes() {
    let f = fixture(true);
    let identity = Dir::open(f.config.machine_identity_dir.as_deref().unwrap()).unwrap();
    let lock = identity.file(".linux-snapshot.lock", false).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let disk = qemu_disk::DiskGuard::open_apple_raw(f.config.disk.as_deref().unwrap()).unwrap();
    validate_live_intent(&f.path, &f.config, &f.record).unwrap();
    assert!(qemu_disk::DiskGuard::open_apple_raw(f.config.disk.as_deref().unwrap()).is_err());
    let mut wrong = f.config.clone();
    wrong.memory_mib += 1;
    assert!(validate_live_intent(&f.path, &wrong, &f.record).is_err());
    let path = f
        .config
        .machine_identity_dir
        .as_ref()
        .unwrap()
        .join("linux-identity-v1.json");
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b" ")
        .unwrap();
    assert!(validate_live_intent(&f.path, &f.config, &f.record).is_err());
    drop(disk);
    drop(lock);
}
