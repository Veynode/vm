use super::*;
use std::{
    fs,
    io::Write,
    os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt},
};

struct Fixture {
    _temp: tempfile::TempDir,
    state: PathBuf,
    store: SnapshotStore,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().canonicalize().unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let store = SnapshotStore::open(&state).unwrap();
        Self {
            _temp: temp,
            state,
            store,
        }
    }
}
fn compatible(disk: bool) -> Compatibility {
    Compatibility {
        backend: "qemu_hvf".into(),
        backend_version: "10.0.12".into(),
        backend_sha256: "1".repeat(64),
        machine: "virt-10.0".into(),
        architecture: "aarch64".into(),
        cpu_model: "host".into(),
        cpus: 1,
        memory_mib: 256,
        config_sha256: "2".repeat(64),
        devices_sha256: "3".repeat(64),
        disk_bytes: disk.then_some(512),
    }
}
fn proof(capture: &Capture<'_>, full: bool) -> CaptureProof {
    // Synthetic backend evidence exercises storage validation only. These unit
    // bytes are not a QEMU migration stream or an actual full-state result.
    CaptureProof {
        capture_id: capture.capture_id().into(),
        backend_receipt_sha256: "4".repeat(64),
        source_worker_instance: full.then(|| "5".repeat(64)),
        backend_completed: true,
        source_quiesced: true,
        ram_cpu_devices_saved: full,
        disk_in_same_quiesced_interval: true,
        protects_restore_of: None,
    }
}
fn bytes(capture: &Capture<'_>, role: Role, value: &[u8]) {
    let mut file = capture.create_artifact(role).unwrap();
    file.write_all(value).unwrap();
    file.sync_all().unwrap();
}
fn rollback(vm: &mut VmSnapshots, name: &str) -> Metadata {
    let capture = vm
        .begin(name, Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    bytes(&capture, Role::Disk, &[7; 512]);
    let receipt = proof(&capture, false);
    capture.commit(receipt).unwrap()
}
#[test]
fn atomic_named_disk_capture_list_verify_delete_is_not_full_state() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let meta = rollback(&mut vm, "before-upgrade");
    assert_eq!(meta.kind, Kind::DiskRollback);
    assert!(!meta.proof.ram_cpu_devices_saved && !meta.restore_verified);
    assert_eq!(vm.list().unwrap().len(), 1);
    assert_eq!(
        vm.verify("before-upgrade", &compatible(true))
            .unwrap()
            .artifacts[0]
            .bytes,
        512
    );
    assert!(vm
        .begin("before-upgrade", Kind::DiskRollback, compatible(true), 512)
        .is_err());
    vm.delete("before-upgrade").unwrap();
    assert!(vm.list().unwrap().is_empty());
}
#[test]
fn abandoned_and_incomplete_full_capture_remain_hidden_and_preserved() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let capture = vm
        .begin("full", Kind::FullState, compatible(true), 512 * 1024 * 1024)
        .unwrap();
    let path = capture.artifact_path(Role::Disk).unwrap();
    bytes(&capture, Role::Disk, &[7; 512]);
    let receipt = proof(&capture, true);
    assert!(capture.commit(receipt).is_err());
    assert!(path.exists() && vm.list().unwrap().is_empty());
    assert!(fs::read_dir(path.parent().unwrap())
        .unwrap()
        .next()
        .is_some());
}
#[test]
fn plain_copy_and_unquiesced_or_foreign_receipts_cannot_commit_full_state() {
    for variant in 0..4 {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        let capture = vm
            .begin("full", Kind::FullState, compatible(true), 512 * 1024 * 1024)
            .unwrap();
        bytes(&capture, Role::Disk, &[7; 512]);
        bytes(
            &capture,
            Role::MachineState,
            b"synthetic CPU fixture; not a real migration stream",
        );
        let mut receipt = proof(&capture, true);
        match variant {
            0 => receipt.ram_cpu_devices_saved = false,
            1 => receipt.source_quiesced = false,
            2 => receipt.disk_in_same_quiesced_interval = false,
            _ => receipt.capture_id = "0".repeat(32),
        }
        assert!(capture.commit(receipt).is_err());
        assert!(vm.list().unwrap().is_empty());
    }
}
#[test]
fn full_state_and_suspend_are_distinct_and_never_imply_restore_validation() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    for (name, kind) in [("full", Kind::FullState), ("sleep", Kind::Suspend)] {
        let capture = vm
            .begin(name, kind, compatible(false), 512 * 1024 * 1024)
            .unwrap();
        bytes(
            &capture,
            Role::MachineState,
            b"synthetic state projection only",
        );
        let receipt = proof(&capture, true);
        let metadata = capture.commit(receipt).unwrap();
        assert_eq!(metadata.kind, kind);
        assert!(!metadata.restore_verified);
    }
    assert_eq!(vm.list().unwrap().len(), 2);
}
#[test]
fn tamper_and_backend_config_device_version_changes_refuse_restore_inputs() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    rollback(&mut vm, "s1");
    for variant in 0..4 {
        let mut next = compatible(true);
        match variant {
            0 => next.backend_version = "10.0.13".into(),
            1 => next.config_sha256 = "6".repeat(64),
            2 => next.devices_sha256 = "7".repeat(64),
            _ => next.cpus = 2,
        }
        assert!(vm.verify("s1", &next).is_err());
    }
    fs::write(f.state.join("snapshots/linux1/s1/disk.raw"), [8; 512]).unwrap();
    assert!(vm.verify("s1", &compatible(true)).is_err());
}
#[test]
fn namespace_and_symlink_edges_never_traverse_or_delete_user_files() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let outside = f.state.join("important.raw");
    fs::write(&outside, [9; 512]).unwrap();
    for bad in ["../important.raw", "/tmp/other", ".", "..", "", "a/b"] {
        assert!(vm
            .begin(bad, Kind::DiskRollback, compatible(true), 512)
            .is_err());
        assert!(vm.delete(bad).is_err());
    }
    let capture = vm
        .begin("linked", Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    let path = capture.artifact_path(Role::Disk).unwrap();
    symlink(&outside, &path).unwrap();
    let receipt = proof(&capture, false);
    assert!(capture.commit(receipt).is_err());
    assert_eq!(fs::read(&outside).unwrap(), vec![9; 512]);
    let external = f.state.join("external");
    fs::create_dir(&external).unwrap();
    symlink(&external, f.state.join("snapshots/linux2")).unwrap();
    assert!(f.store.lock_vm("linux2").is_err());
}
#[test]
fn exclusive_vm_lock_and_foreign_manifest_prevent_cross_vm_deletion() {
    let f = Fixture::new();
    let mut first = f.store.lock_vm("linux1").unwrap();
    assert!(f.store.lock_vm("linux1").is_err());
    rollback(&mut first, "s1");
    let manifest = f.state.join("snapshots/linux1/s1/manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    value["vm"] = serde_json::json!("linux2");
    fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(first.delete("s1").is_err());
    assert!(f.state.join("snapshots/linux1/s1/disk.raw").exists());
    drop(first);
    assert!(f.store.lock_vm("linux1").is_ok());
}
#[test]
fn diskless_restore_still_requires_retirement_and_holds_snapshot_lock() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let capture = vm
        .begin(
            "full",
            Kind::FullState,
            compatible(false),
            512 * 1024 * 1024,
        )
        .unwrap();
    bytes(
        &capture,
        Role::MachineState,
        b"synthetic state projection only",
    );
    let receipt = proof(&capture, true);
    capture.commit(receipt).unwrap();
    assert!(vm
        .prepare_restore(
            "full",
            &compatible(false),
            None,
            RestoreTarget {
                worker_retired: false,
                disk: None
            }
        )
        .is_err());
    let plan = vm
        .prepare_restore(
            "full",
            &compatible(false),
            None,
            RestoreTarget {
                worker_retired: true,
                disk: None,
            },
        )
        .unwrap();
    assert!(plan.machine_state.is_some() && plan.disk.is_none() && !plan.metadata.restore_verified);
    assert!(f.store.lock_vm("linux1").is_err());
    drop(plan);
    vm.delete("full").unwrap();
}
#[test]
fn protection_binds_current_bytes_exact_restore_and_vm_without_disk_overwrite() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let source = rollback(&mut vm, "source");
    let capture = vm
        .begin("protect", Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    bytes(&capture, Role::Disk, &[7; 512]);
    let mut receipt = proof(&capture, false);
    receipt.protects_restore_of = Some(source.proof.capture_id.clone());
    let protection = capture.commit(receipt).unwrap();
    let current = File::open(f.state.join("snapshots/linux1/source/disk.raw")).unwrap();
    let hash = storage::hash(&current).unwrap();
    check_protection(&source, &protection, &compatible(true), hash.clone()).unwrap();
    let mut wrong = protection.clone();
    wrong.vm = "linux2".into();
    assert!(check_protection(&source, &wrong, &compatible(true), hash.clone()).is_err());
    wrong = protection.clone();
    wrong.proof.protects_restore_of = Some("0".repeat(32));
    assert!(check_protection(&source, &wrong, &compatible(true), hash.clone()).is_err());
    assert!(check_protection(
        &source,
        &protection,
        &compatible(true),
        (512, "0".repeat(64))
    )
    .is_err());
    assert!(vm
        .prepare_restore(
            "source",
            &compatible(true),
            Some("protect"),
            RestoreTarget {
                worker_retired: true,
                disk: None
            }
        )
        .is_err());
}
#[test]
fn reservation_and_unknown_entries_fail_before_destructive_actions() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    assert!(vm
        .begin("tiny", Kind::FullState, compatible(true), 512)
        .is_err());
    assert!(vm
        .begin("overflow", Kind::DiskRollback, compatible(true), u64::MAX)
        .is_err());
    rollback(&mut vm, "s1");
    let extra = f.state.join("snapshots/linux1/s1/user-file");
    fs::write(&extra, b"preserve").unwrap();
    assert!(vm.delete("s1").is_err());
    assert_eq!(fs::read(extra).unwrap(), b"preserve");
}

#[test]
fn supported_storage_identities_do_not_allow_cross_backend_restore() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let mut apple = compatible(false);
    apple.backend = "apple_virtualization".into();
    apple.backend_version = "macOS-27.0.1-26A113".into();
    apple.machine = "VZGenericPlatformConfiguration:pinned-machine-identity".into();
    let capture = vm
        .begin("apple", Kind::FullState, apple.clone(), 512 * 1024 * 1024)
        .unwrap();
    bytes(
        &capture,
        Role::MachineState,
        b"synthetic Apple storage fixture, not VZ state",
    );
    let receipt = proof(&capture, true);
    capture.commit(receipt).unwrap();
    assert!(vm.verify("apple", &apple).is_ok());
    assert!(vm.verify("apple", &compatible(false)).is_err());
    let mut other = apple;
    other.backend = "unknown_backend".into();
    assert!(vm.verify("apple", &other).is_err());
}

#[test]
fn replaced_lock_or_owner_is_detected_before_future_mutation() {
    for replace_lock in [false, true] {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        rollback(&mut vm, "preserve");
        let directory = f.state.join("snapshots/linux1");
        if replace_lock {
            fs::rename(directory.join(".lock"), directory.join(".old-lock")).unwrap();
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(directory.join(".lock"))
                .unwrap();
            drop(file);
        } else {
            fs::write(
                directory.join(".owner.json"),
                br#"{"schema":1,"vm":"other"}"#,
            )
            .unwrap();
        }
        assert!(vm.list().is_err());
        assert!(vm
            .begin("new", Kind::DiskRollback, compatible(true), 512)
            .is_err());
        assert!(vm.delete("preserve").is_err());
        assert_eq!(
            fs::read(directory.join("preserve/disk.raw")).unwrap(),
            vec![7; 512]
        );
        assert!(!directory.join("new").exists());
    }
}

#[test]
fn lock_replacement_during_capture_cannot_publish_a_complete_snapshot() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let capture = vm
        .begin("capture", Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    bytes(&capture, Role::Disk, &[7; 512]);
    let pending = capture.artifact_path(Role::Disk).unwrap();
    let directory = f.state.join("snapshots/linux1");
    fs::rename(directory.join(".lock"), directory.join(".retired-lock")).unwrap();
    let receipt = proof(&capture, false);
    assert!(capture.commit(receipt).is_err());
    assert!(!directory.join("capture").exists());
    assert_eq!(fs::read(pending).unwrap(), vec![7; 512]);
}

#[test]
fn foreign_name_collision_at_commit_is_never_overwritten() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let capture = vm
        .begin("capture", Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    bytes(&capture, Role::Disk, &[7; 512]);
    let pending = capture.artifact_path(Role::Disk).unwrap();
    let final_dir = f.state.join("snapshots/linux1/capture");
    fs::create_dir(&final_dir).unwrap();
    fs::write(final_dir.join("keep-me"), b"foreign existing data").unwrap();
    let receipt = proof(&capture, false);
    assert!(capture.commit(receipt).is_err());
    assert_eq!(
        fs::read(final_dir.join("keep-me")).unwrap(),
        b"foreign existing data"
    );
    assert!(!final_dir.join("disk.raw").exists());
    assert_eq!(fs::read(pending).unwrap(), vec![7; 512]);
}

#[test]
fn snapshot_directory_replacement_is_not_followed_for_deletion_or_capture() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    rollback(&mut vm, "s1");
    let original = f.state.join("snapshots/linux1");
    let moved = f.state.join("snapshots/retained-linux1");
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(original.join("keep-me"), b"replacement directory").unwrap();
    assert!(vm.list().is_err());
    assert!(vm.delete("s1").is_err());
    assert!(vm
        .begin("new", Kind::DiskRollback, compatible(true), 512)
        .is_err());
    assert_eq!(fs::read(moved.join("s1/disk.raw")).unwrap(), vec![7; 512]);
    assert_eq!(
        fs::read(original.join("keep-me")).unwrap(),
        b"replacement directory"
    );
}

#[test]
fn hard_links_and_non_private_store_or_artifacts_are_rejected() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let capture = vm
        .begin("hardlink", Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    bytes(&capture, Role::Disk, &[7; 512]);
    let pending = capture.artifact_path(Role::Disk).unwrap();
    let outside = f.state.join("outside.raw");
    fs::hard_link(&pending, &outside).unwrap();
    let receipt = proof(&capture, false);
    assert!(capture.commit(receipt).is_err());
    assert_eq!(fs::read(outside).unwrap(), vec![7; 512]);
    let capture = vm
        .begin("public", Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    bytes(&capture, Role::Disk, &[7; 512]);
    fs::set_permissions(
        capture.artifact_path(Role::Disk).unwrap(),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let receipt = proof(&capture, false);
    assert!(capture.commit(receipt).is_err());
    assert!(vm.list().unwrap().is_empty());
    fs::set_permissions(&f.state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(SnapshotStore::open(&f.state).is_err());
}

#[test]
fn space_admission_has_real_available_space_check_and_no_visible_side_effects() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let error = match vm.begin(
        "no-space",
        Kind::DiskRollback,
        compatible(true),
        u64::MAX - 64 * 1024 * 1024,
    ) {
        Ok(_) => panic!("filesystem reported u64::MAX available bytes"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("insufficient snapshot space"),
        "{error:#}"
    );
    assert!(vm.list().unwrap().is_empty());
    assert!(fs::read_dir(f.state.join("snapshots/linux1"))
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pending-")));
}

#[test]
fn delete_only_explicit_snapshot_and_leave_other_vm_and_raw_file_intact() {
    let f = Fixture::new();
    let mut one = f.store.lock_vm("linux1").unwrap();
    let mut two = f.store.lock_vm("linux2").unwrap();
    rollback(&mut one, "same-name");
    rollback(&mut one, "keep");
    rollback(&mut two, "same-name");
    let original = f.state.join("original.raw");
    fs::write(&original, [42; 512]).unwrap();
    one.delete("same-name").unwrap();
    assert_eq!(
        one.list()
            .unwrap()
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        ["keep"]
    );
    assert_eq!(two.list().unwrap().len(), 1);
    assert_eq!(fs::read(original).unwrap(), vec![42; 512]);
    assert!(one.delete("same-name").is_err());
}

#[test]
fn oversized_metadata_or_truncated_artifact_never_yields_restore_plan() {
    for oversized in [false, true] {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        rollback(&mut vm, "s1");
        let directory = f.state.join("snapshots/linux1/s1");
        if oversized {
            fs::write(directory.join("manifest.json"), vec![b' '; 65537]).unwrap();
        } else {
            fs::write(directory.join("disk.raw"), [7; 511]).unwrap();
        }
        assert!(vm.verify("s1", &compatible(true)).is_err());
        assert!(vm
            .prepare_restore(
                "s1",
                &compatible(true),
                None,
                RestoreTarget {
                    worker_retired: true,
                    disk: None
                }
            )
            .is_err());
        assert!(directory.exists());
    }
}

#[test]
fn qmp_output_descriptor_is_writeonly_and_never_truncates_an_existing_artifact() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    let capture = vm
        .begin(
            "state",
            Kind::FullState,
            compatible(false),
            512 * 1024 * 1024,
        )
        .unwrap();
    let mut output = capture
        .create_artifact_writeonly(Role::MachineState)
        .unwrap();
    let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::O_ACCMODE, libc::O_WRONLY);
    output
        .write_all(b"synthetic writeonly output, not a migration stream")
        .unwrap();
    output.sync_all().unwrap();
    assert!(capture
        .create_artifact_writeonly(Role::MachineState)
        .is_err());
    assert!(capture.create_artifact(Role::MachineState).is_err());
    drop(output);
    let receipt = proof(&capture, true);
    capture.commit(receipt).unwrap();
    assert_eq!(
        vm.verify("state", &compatible(false))
            .unwrap()
            .artifacts
            .len(),
        1
    );
}

#[cfg(target_os = "macos")]
#[test]
fn apfs_disk_clone_uses_pinned_fd_and_preserves_independent_original_and_capture() {
    use std::os::unix::fs::{FileExt, MetadataExt};
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    // This is a tiny offline storage fixture, not a real VM or full snapshot.
    // The production caller additionally retains its existing DiskGuard.
    let source_path = f.state.join("source.raw");
    fs::write(&source_path, [7; 512]).unwrap();
    let source = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&source_path)
        .unwrap();
    let retained = f.state.join("retained-source.raw");
    fs::rename(&source_path, &retained).unwrap();
    fs::write(&source_path, [99; 512]).unwrap();
    let capture = vm
        .begin("clone", Kind::DiskRollback, compatible(true), 512)
        .unwrap();
    let cloned = capture.clone_disk(&source).unwrap();
    assert_ne!(
        (
            source.metadata().unwrap().dev(),
            source.metadata().unwrap().ino()
        ),
        (
            cloned.metadata().unwrap().dev(),
            cloned.metadata().unwrap().ino()
        )
    );
    let mut actual = [0; 512];
    cloned.read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(actual, [7; 512]);
    assert!(capture.clone_disk(&source).is_err());
    source.write_all_at(&[8; 512], 0).unwrap();
    source.sync_all().unwrap();
    cloned.read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(actual, [7; 512]);
    let receipt = proof(&capture, false);
    capture.commit(receipt).unwrap();
    vm.verify("clone", &compatible(true)).unwrap();
    vm.delete("clone").unwrap();
    assert_eq!(fs::read(retained).unwrap(), vec![8; 512]);
    assert_eq!(fs::read(source_path).unwrap(), vec![99; 512]);
}

#[test]
fn backend_receipt_artifact_mismatch_never_publishes_snapshot() {
    for wrong in 0..3 {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        let capture = vm
            .begin("mismatch", Kind::DiskRollback, compatible(true), 512)
            .unwrap();
        bytes(&capture, Role::Disk, &[7; 512]);
        let pending = capture.artifact_path(Role::Disk).unwrap();
        let receipt = proof(&capture, false);
        let mut expected = vec![Artifact {
            role: Role::Disk,
            bytes: 512,
            sha256: format!("{:x}", Sha256::digest([7; 512])),
        }];
        match wrong {
            0 => expected[0].sha256 = "0".repeat(64),
            1 => expected[0].bytes = 511,
            _ => expected.push(expected[0].clone()),
        }
        assert!(capture.commit_expected(receipt, &expected).is_err());
        assert!(vm.list().unwrap().is_empty());
        assert!(pending.exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn acl_grants_behind_private_mode_bits_block_store_traversal_verify_and_delete() {
    for target in 0..8 {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        rollback(&mut vm, "keep");
        let path = match target {
            0 => f.state.clone(),
            1 => f.state.join("snapshots"),
            2 => f.state.join("snapshots/linux1"),
            3 => f.state.join("snapshots/linux1/keep"),
            4 => f.state.join("snapshots/linux1/keep/disk.raw"),
            5 => f.state.join("snapshots/linux1/keep/manifest.json"),
            6 => f.state.join("snapshots/linux1/.lock"),
            _ => f.state.join("snapshots/linux1/.owner.json"),
        };
        let acl = storage::acl_test_support::Entry::add(&path);
        let error = match vm.verify("keep", &compatible(true)) {
            Ok(_) => panic!("ACL-bearing snapshot input accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("extended ACL"), "{error:#}");
        assert!(vm.delete("keep").is_err());
        if target == 0 {
            assert!(SnapshotStore::open(&f.state).is_err());
        }
        acl.assert_unchanged();
        assert_eq!(
            fs::read(f.state.join("snapshots/linux1/keep/disk.raw")).unwrap(),
            vec![7; 512]
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn acl_on_saved_machine_state_or_clone_source_is_never_published_or_stripped() {
    for machine_state in [false, true] {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        if machine_state {
            let capture = vm
                .begin(
                    "state",
                    Kind::FullState,
                    compatible(false),
                    512 * 1024 * 1024,
                )
                .unwrap();
            bytes(
                &capture,
                Role::MachineState,
                b"synthetic CPU fixture, not real VM state",
            );
            let path = capture.artifact_path(Role::MachineState).unwrap();
            let acl = storage::acl_test_support::Entry::add(&path);
            let receipt = proof(&capture, true);
            let error = capture.commit(receipt).unwrap_err();
            assert!(format!("{error:#}").contains("extended ACL"), "{error:#}");
            assert!(vm.list().unwrap().is_empty());
            acl.assert_unchanged();
        } else {
            let source = f.state.join("source.raw");
            fs::write(&source, [8; 512]).unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
            let acl = storage::acl_test_support::Entry::add(&source);
            let capture = vm
                .begin("clone", Kind::DiskRollback, compatible(true), 512)
                .unwrap();
            let error = capture
                .clone_disk(&File::open(&source).unwrap())
                .unwrap_err();
            assert!(format!("{error:#}").contains("extended ACL"), "{error:#}");
            assert!(!capture.artifact_path(Role::Disk).unwrap().exists());
            acl.assert_unchanged();
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn acl_bearing_operation_root_directory_or_receipt_refuses_reload_without_repairing_acl() {
    for target in 0..3 {
        let f = Fixture::new();
        let ops = crate::snapshot_control::Operations::open(&f.state).unwrap();
        let op = ops.begin("linux1", "before", "create", None).unwrap();
        let root = f.state.join("snapshot-operations");
        let path = match target {
            0 => root,
            1 => root.join(op.id()),
            _ => root.join(op.id()).join("0000.json"),
        };
        let acl = storage::acl_test_support::Entry::add(&path);
        let error = match crate::snapshot_control::Operations::open(&f.state) {
            Ok(_) => panic!("ACL-bearing operation accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("extended ACL"), "{error:#}");
        acl.assert_unchanged();
        assert!(ops.active_vm("linux1"));
    }
}

#[test]
fn interrupted_named_delete_can_retry_without_touching_other_snapshots() {
    let f = Fixture::new();
    let mut vm = f.store.lock_vm("linux1").unwrap();
    rollback(&mut vm, "victim");
    rollback(&mut vm, "keep");
    let parent = f.state.join("snapshots/linux1");
    fs::set_permissions(parent.join("victim"), fs::Permissions::from_mode(0o500)).unwrap();
    assert!(vm.delete("victim").is_err());
    let tomb = fs::read_dir(&parent)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(".deleting-")
        })
        .unwrap();
    fs::set_permissions(&tomb, fs::Permissions::from_mode(0o700)).unwrap();
    // Reopen storage to prove this is durable, not in-memory retry ownership.
    drop(vm);
    let store = SnapshotStore::open(&f.state).unwrap();
    let mut vm = store.lock_vm("linux1").unwrap();
    vm.delete("victim").unwrap();
    assert!(!tomb.exists());
    assert_eq!(
        vm.list()
            .unwrap()
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        ["keep"]
    );
}
#[test]
fn interrupted_delete_reserves_name_and_finishes_partial_or_empty_tomb() {
    for empty in [false, true] {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        rollback(&mut vm, "victim");
        let parent = f.state.join("snapshots/linux1");
        let tomb = parent.join(".deleting-victim");
        fs::rename(parent.join("victim"), &tomb).unwrap();
        fs::remove_file(tomb.join("disk.raw")).unwrap();
        if empty {
            fs::remove_file(tomb.join("manifest.json")).unwrap();
        }
        assert!(vm
            .begin("victim", Kind::DiskRollback, compatible(true), 512)
            .is_err());
        vm.delete("victim").unwrap();
        assert!(!tomb.exists());
        rollback(&mut vm, "victim");
        assert_eq!(vm.list().unwrap().len(), 1);
    }
}

#[test]
fn resumed_delete_refuses_foreign_extra_alias_or_ambiguous_contents() {
    for fault in [
        "foreign",
        "extra",
        "symlink",
        "ambiguous",
        "manifest_missing",
    ] {
        let f = Fixture::new();
        let mut vm = f.store.lock_vm("linux1").unwrap();
        rollback(&mut vm, "victim");
        rollback(&mut vm, "keep");
        let parent = f.state.join("snapshots/linux1");
        let tomb = parent.join(".deleting-victim");
        fs::rename(parent.join("victim"), &tomb).unwrap();
        let outside = f.state.join("original.raw");
        fs::write(&outside, [42; 512]).unwrap();
        match fault {
            "foreign" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(tomb.join("manifest.json")).unwrap()).unwrap();
                value["vm"] = serde_json::json!("linux2");
                fs::write(
                    tomb.join("manifest.json"),
                    serde_json::to_vec(&value).unwrap(),
                )
                .unwrap();
            }
            "extra" => {
                fs::write(tomb.join("unknown"), b"preserve").unwrap();
            }
            "symlink" => {
                fs::remove_file(tomb.join("disk.raw")).unwrap();
                symlink(&outside, tomb.join("disk.raw")).unwrap();
            }
            "ambiguous" => {
                fs::create_dir(parent.join("victim")).unwrap();
            }
            "manifest_missing" => {
                fs::remove_file(tomb.join("manifest.json")).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(vm.delete("victim").is_err(), "{fault}");
        assert!(tomb.join("disk.raw").symlink_metadata().is_ok());
        assert!(parent.join("keep/disk.raw").exists());
        assert_eq!(fs::read(&outside).unwrap(), vec![42; 512]);
    }
}
