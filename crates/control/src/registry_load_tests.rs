//! Registry load judges every entry with the creation checks (decision 0025 item 3) and refuses a registry schema
//! newer than it knows (`registry.schema`). Nothing here starts a VM: the only `start` sent is one the daemon must
//! refuse before it looks at the backend.
use super::*;

fn call(manager: &mut Manager, method: &str, arguments: Value) -> Result<Value> {
    manager.dispatch(json!({"method":method,"arguments":arguments}), &mut None)
}

#[test]
fn entries_failing_the_creation_checks_are_listed_with_the_reason_refused_by_start_and_kept() {
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"registry only, never started").unwrap();
    let state = temp.path().join("state");
    fs::create_dir(&state).unwrap();
    let registry = json!({
        "good":{"backend":"apple-virtualization","id":"good","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel,"window":false},
        "gone":{"backend":"apple-virtualization","id":"gone","guest":"linux","cpus":1,"memory_mib":512,"kernel":temp.path().join("absent"),"window":false},
        "cpus":{"backend":"apple-virtualization","id":"cpus","guest":"linux","cpus":0,"memory_mib":512,"kernel":kernel,"window":false},
        "guest":{"backend":"apple-virtualization","id":"guest","guest":"freebsd","cpus":1,"memory_mib":512,"window":false},
        // Valid in itself, but this daemon has no QEMU registration: `create` refuses it for that reason too.
        "qemu":{"backend":"qemu-hvf","id":"qemu","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel,"window":false},
    });
    let bytes = serde_json::to_vec_pretty(&registry).unwrap();
    fs::write(state.join("registry.json"), &bytes).unwrap();
    let mut manager = Manager::load(
        crate::test_private_state(&state),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    manager.validate_registry();
    let listed = call(&mut manager, "list", json!({})).unwrap();
    let listed: BTreeMap<&str, &Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|vm| (vm["config"]["id"].as_str().unwrap(), vm))
        .collect();
    assert_eq!(listed.len(), 5);
    assert_eq!(listed["good"]["state"], "stopped");
    assert!(listed["good"].get("invalid_reason").is_none());
    for (id, expected) in [
        ("gone", "input files must exist at absolute paths"),
        ("cpus", "invalid CPU or memory allocation"),
        ("guest", "unsupported guest"),
        ("qemu", "QEMU/HVF backend not registered"),
    ] {
        let vm = listed[id];
        assert_eq!(vm["state"], "invalid", "{id}");
        let reason = vm["invalid_reason"].as_str().unwrap();
        assert!(reason.contains(expected), "{id}: {reason}");
        // The definition is shown as registered, not altered.
        assert_eq!(vm["config"]["cpus"], registry[id]["cpus"], "{id}");
        // The reason is the one `create` gives for the same definition under a free id.
        let mut again = registry[id].clone();
        again["id"] = json!(format!("{id}-again"));
        assert_eq!(
            format!("{:#}", call(&mut manager, "create", again).unwrap_err()),
            reason,
            "{id}"
        );
        let refusal = format!(
            "{:#}",
            call(&mut manager, "start", json!({"id":id})).unwrap_err()
        );
        assert!(
            refusal.contains("failed validation at registry load and cannot be started"),
            "{id}: {refusal}"
        );
        assert!(refusal.contains(reason), "{id}: {refusal}");
        assert_eq!(
            call(&mut manager, "status", json!({"id":id})).unwrap()["state"],
            "invalid",
            "{id}"
        );
    }
    assert!(manager.running.is_empty());
    // Loading, judging, listing and the refused calls wrote nothing.
    assert_eq!(fs::read(state.join("registry.json")).unwrap(), bytes);
    // The daemon and the other VMs go on: a new definition is accepted, and the write keeps every failing entry.
    let created = call(
        &mut manager,
        "create",
        json!({"id":"new","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
    )
    .unwrap();
    assert_eq!(created["state"], "stopped");
    let written: Value =
        serde_json::from_slice(&fs::read(state.join("registry.json")).unwrap()).unwrap();
    assert_eq!(written.as_object().unwrap().len(), 6);
    for id in ["good", "gone", "cpus", "guest", "qemu"] {
        assert_eq!(written[id], registry[id], "{id}");
    }
    assert_eq!(
        call(&mut manager, "status", json!({"id":"gone"})).unwrap()["state"],
        "invalid"
    );
}

#[test]
fn a_registry_of_valid_entries_reports_none_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"registry only, never started").unwrap();
    let mut manager = Manager::load(
        crate::test_private_state(temp.path()),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    call(
        &mut manager,
        "create",
        json!({"id":"a","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
    )
    .unwrap();
    manager.validate_registry();
    assert!(manager.invalid.is_empty());
    assert_eq!(
        call(&mut manager, "status", json!({"id":"a"})).unwrap()["state"],
        "stopped"
    );
}

#[test]
fn registry_schema_sidecar_is_optional_accepted_at_version_1_and_refused_above_or_malformed() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"registry only, never started").unwrap();
    let state = temp.path().join("state");
    fs::create_dir(&state).unwrap();
    let load = || {
        Manager::load(
            crate::test_private_state(&state),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
    };
    let sidecar = state.join("registry.schema");
    // Absent is version 1, and a write at version 1 does not create the file.
    let mut manager = load().unwrap();
    call(
        &mut manager,
        "create",
        json!({"id":"a","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
    )
    .unwrap();
    drop(manager);
    assert!(!sidecar.exists());
    let registry = fs::read(state.join("registry.json")).unwrap();
    for accepted in [&b"1"[..], b"1\n"] {
        fs::write(&sidecar, accepted).unwrap();
        let mut manager = load().unwrap();
        assert_eq!(
            call(&mut manager, "status", json!({"id":"a"})).unwrap()["state"],
            "stopped"
        );
        assert_eq!(fs::read(&sidecar).unwrap(), accepted);
        assert_eq!(fs::read(state.join("registry.json")).unwrap(), registry);
    }
    for (found, bytes) in [
        ("2", &b"2\n"[..]),
        ("7", b"7"),
        ("18446744073709551615", b"18446744073709551615\n"),
    ] {
        fs::write(&sidecar, bytes).unwrap();
        let error = format!("{:#}", load().err().unwrap());
        assert_eq!(
            error,
            format!("registry schema version {found} is newer than this daemon supports (1); daemon not started, nothing rewritten")
        );
        assert_eq!(fs::read(&sidecar).unwrap(), bytes);
        assert_eq!(fs::read(state.join("registry.json")).unwrap(), registry);
    }
    for bytes in [
        &b""[..],
        b"\n",
        b"0",
        b"01",
        b"+2",
        b"-1",
        b" 2",
        b"2 ",
        b"2\n\n",
        b"2\r\n",
        b"1.0",
        b"two",
        b"\xff",
        b"18446744073709551616",
        b"{\"schema_version\":2}",
    ] {
        fs::write(&sidecar, bytes).unwrap();
        let error = format!("{:#}", load().err().unwrap());
        assert!(
            error.contains("registry.schema is malformed"),
            "{bytes:?}: {error}"
        );
        assert!(
            error.contains("this daemon supports 1"),
            "{bytes:?}: {error}"
        );
        assert_eq!(fs::read(&sidecar).unwrap(), bytes);
        assert_eq!(fs::read(state.join("registry.json")).unwrap(), registry);
    }
    // Not a regular file: never followed, never read as a version.
    fs::remove_file(&sidecar).unwrap();
    fs::write(temp.path().join("one"), b"1\n").unwrap();
    symlink(temp.path().join("one"), &sidecar).unwrap();
    assert!(format!("{:#}", load().err().unwrap()).contains("malformed"));
    fs::remove_file(&sidecar).unwrap();
    fs::create_dir(&sidecar).unwrap();
    assert!(format!("{:#}", load().err().unwrap()).contains("malformed"));
    fs::remove_dir(&sidecar).unwrap();
    load().unwrap();
}

fn load_registry(dir: &Path) -> Result<Manager> {
    Manager::load(
        crate::test_private_state(dir),
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
}

#[test]
fn registry_reserved_ids_and_malformed_ids_are_invalid_entries_and_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"never started").unwrap();
    let ids = ["registry", "Registry", "../bad", "", "bad.id"];
    let mut registry = serde_json::Map::new();
    for id in ids.into_iter().chain(["good"]) {
        registry.insert(
            id.into(),
            json!({"id":id,"guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
        );
    }
    let bytes = serde_json::to_vec(&registry).unwrap();
    fs::write(temp.path().join("registry.json"), &bytes).unwrap();
    let mut manager = load_registry(temp.path()).unwrap();
    manager.validate_registry();
    let listed = call(&mut manager, "list", json!({})).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), ids.len() + 1);
    for id in ids {
        assert!(validate_new_vm_id(id).is_err(), "{id}");
        let vm = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|vm| vm["config"]["id"] == id)
            .unwrap();
        assert_eq!(vm["state"], "invalid", "{id}");
        let reason = vm["invalid_reason"].as_str().unwrap();
        let error = call(&mut manager, "create", registry[id].clone()).unwrap_err();
        assert_eq!(format!("{error:#}"), reason);
        assert!(call(&mut manager, "start", json!({"id":id})).is_err());
    }
    assert_eq!(
        call(&mut manager, "status", json!({"id":"good"})).unwrap()["state"],
        "stopped"
    );
    assert!(manager.running.is_empty());
    assert_eq!(fs::read(temp.path().join("registry.json")).unwrap(), bytes);
    call(
        &mut manager,
        "create",
        json!({"id":"new","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
    )
    .unwrap();
    let written: BTreeMap<String, Config> =
        serde_json::from_slice(&fs::read(temp.path().join("registry.json")).unwrap()).unwrap();
    for (id, config) in registry {
        assert_eq!(
            serde_json::to_value(&written[&id]).unwrap(),
            serde_json::to_value(serde_json::from_value::<Config>(config).unwrap()).unwrap()
        );
    }
}

#[test]
fn registry_config_writer_refuses_protected_paths_with_typed_error() {
    let temp = tempfile::tempdir().unwrap();
    let config: Config =
        serde_json::from_value(json!({"id":"registry","guest":"linux","cpus":1,"memory_mib":512}))
            .unwrap();
    for name in ["registry.json", "REGISTRY.json"] {
        let path = temp.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"must remain untouched").unwrap();
        let error = write_vm_config(temp.path(), &path, &config).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RegistryError>(),
            Some(RegistryError::ProtectedConfigPath(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"must remain untouched");
    }
    let path = temp.path().join("good.json");
    write_vm_config(temp.path(), &path, &config).unwrap();
    let mut expected = serde_json::to_vec_pretty(&config).unwrap();
    expected.push(b'\n');
    assert_eq!(fs::read(path).unwrap(), expected);
}

#[test]
fn registry_duplicate_keys_fail_the_strict_read_and_are_left_out_at_daemon_start() {
    let temp = tempfile::tempdir().unwrap();
    // Escaped spellings are the same decoded member name. The first definition is invalid at creation.
    for second in [r#""vm""#, r#""\u0076m""#] {
        let bytes = format!(
            r#"{{"vm":{{"id":"vm","guest":"linux","cpus":0,"memory_mib":512}},{second}:{{"id":"vm","guest":"linux","cpus":1,"memory_mib":512}},"ok":{{"id":"ok","guest":"linux","cpus":1,"memory_mib":512}}}}"#
        );
        let path = temp.path().join("registry.json");
        fs::write(&path, &bytes).unwrap();
        let error = read_registry(&path).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<RegistryError>(), Some(RegistryError::DuplicateKey(key)) if key == "vm"),
            "{error:#}"
        );
        // Neither definition of the repeated key is served; the rest is, and the file stays as it was.
        let mut manager = load_registry(temp.path()).unwrap();
        let listed = call(&mut manager, "list", json!({})).unwrap();
        let ids: Vec<_> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|vm| vm["config"]["id"].clone())
            .collect();
        assert_eq!(ids, [json!("ok")]);
        assert!(
            manager.notices[0].contains("\"vm\""),
            "{:?}",
            manager.notices
        );
        assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
    }
}

#[test]
fn a_damaged_registry_is_copied_aside_and_its_valid_entries_are_served() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("registry.json");
    let bytes = br#"{"good":{"id":"good","guest":"linux","cpus":1,"memory_mib":512},
        "other":{"id":"elsewhere","guest":"linux","cpus":1,"memory_mib":512},
        "broken":{"id":"broken","cpus":"two"}}"#;
    fs::write(&path, bytes).unwrap();
    let backups = || {
        fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".bad"))
            .collect::<Vec<_>>()
    };
    let mut manager = load_registry(temp.path()).unwrap();
    let listed = call(&mut manager, "list", json!({})).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["config"]["id"], "good");
    let notice = &manager.notices[0];
    assert!(
        notice.contains("1 VMs kept")
            && notice.contains("\"other\"")
            && notice.contains("\"broken\""),
        "{notice}"
    );
    let saved = backups();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(fs::read(temp.path().join(&saved[0])).unwrap(), bytes);
    // Started again on the same file: one copy still.
    load_registry(temp.path()).unwrap();
    assert_eq!(backups(), saved);
    assert_eq!(fs::read(&path).unwrap(), bytes);

    // Not JSON at all: the daemon starts with no VMs.
    fs::write(&path, b"{\"good\": {").unwrap();
    let mut manager = load_registry(temp.path()).unwrap();
    assert_eq!(call(&mut manager, "list", json!({})).unwrap(), json!([]));
    assert_eq!(backups().len(), 2);
}

#[test]
fn registry_read_is_bounded_and_refuses_nonregular_files_without_blocking() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("registry.json");
    let error =
        read_state_file(File::open("/dev/zero").unwrap(), Path::new("/dev/zero"), 16).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<StateFileError>(),
        Some(StateFileError::NotRegular(_))
    ));
    let limit = 16 * MAX_MESSAGE;
    let mut bytes = vec![b' '; limit];
    bytes[..2].copy_from_slice(b"{}");
    fs::write(&path, &bytes).unwrap();
    load_registry(temp.path()).unwrap();
    bytes.push(b' ');
    fs::write(&path, &bytes).unwrap();
    assert!(load_registry(temp.path())
        .err()
        .unwrap()
        .to_string()
        .contains("size limit"));
    assert_eq!(fs::metadata(&path).unwrap().len(), bytes.len() as u64);
    fs::remove_file(&path).unwrap();
    symlink("/dev/zero", &path).unwrap();
    // A timeout fails the test instead of hanging the harness if a read regresses to fs::read.
    let dir = temp.path().to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        tx.send(load_registry(&dir).err().map(|e| e.to_string()))
            .unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap().is_some());
    assert!(fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn registry_schema_fifo_is_refused_promptly_and_descriptor_is_pinned() {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("registry.schema");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let dir = temp.path().to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        tx.send(check_registry_schema(&dir).map_err(|e| e.to_string()))
            .unwrap();
    });
    assert!(rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap_err()
        .contains("malformed"));
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"1\n").unwrap();
    let file = open_state_file(&path).unwrap().unwrap();
    assert_ne!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    assert_ne!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    fs::rename(&path, temp.path().join("original")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &path).unwrap();
    assert_eq!(read_state_file(file, &path, 21).unwrap(), b"1\n");
    assert!(check_registry_schema(temp.path()).is_err());
}
