use super::*;

#[test]
fn worker_key_event_log_argv_order_and_backend_gating() {
    let config = Path::new("/state/vm.json");
    let dir = Path::new("/private/test logs");
    let instance = "1234abcd".to_owned() + &"0".repeat(56);
    let (args, path) = worker_start_args(
        Backend::AppleVirtualization,
        config,
        Some(dir),
        "mac",
        123,
        &instance,
    );
    let expected = dir.join("key-events-mac-123-1234abcd.jsonl");
    assert_eq!(path, Some(expected.clone()));
    assert_eq!(
        args,
        vec![
            OsString::from("run"),
            OsString::from("--config"),
            config.into(),
            OsString::from("--log-key-events"),
            expected.into_os_string(),
        ]
    );
    let (args, path) = worker_start_args(
        Backend::AppleVirtualization,
        config,
        None,
        "mac",
        123,
        &instance,
    );
    assert_eq!(
        args,
        vec![
            OsString::from("run"),
            OsString::from("--config"),
            config.into()
        ]
    );
    assert_eq!(path, None);
    for dir in [None, Some(dir)] {
        let (args, path) =
            worker_start_args(Backend::QemuHvf, config, dir, "linux", 123, &instance);
        assert_eq!(args, vec![OsString::from("--config"), config.into()]);
        assert_eq!(path, None);
    }
}

#[test]
fn worker_key_event_log_names_differ_for_starts_in_the_same_second() {
    let path = |instance: &str, seconds| {
        worker_start_args(
            Backend::AppleVirtualization,
            Path::new("/config"),
            Some(Path::new("/logs")),
            "mac",
            seconds,
            instance,
        )
        .1
        .unwrap()
    };
    assert_ne!(path(&"a".repeat(64), 123), path(&"b".repeat(64), 123));
    assert_ne!(path(&"a".repeat(64), 123), path(&"a".repeat(64), 124));
}

// Only disposable shell fixtures are started, never a VM. Reap them even on assertion failure.
struct Fixture(Manager);
impl Drop for Fixture {
    fn drop(&mut self) {
        for runtime in self.0.running.values_mut() {
            let _ = runtime.force_stop();
        }
    }
}

#[test]
fn worker_key_event_log_status_tracks_launch_exit_restart_and_default_off() {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let backend = temp.path().join("worker");
    fs::write(
        &backend,
        b"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$3.argv\"\nread -r done\n",
    )
    .unwrap();
    fs::set_permissions(&backend, fs::Permissions::from_mode(0o700)).unwrap();
    let kernel = temp.path().join("kernel");
    fs::write(&kernel, b"fixture, not a kernel").unwrap();
    let mut fixture = Fixture(
        Manager::load(
            crate::test_private_state(temp.path()),
            backend,
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap(),
    );
    let manager = &mut fixture.0;
    for (id, backend) in [("apple", "apple-virtualization"), ("qemu", "qemu-hvf")] {
        manager.configs.insert(
            id.into(),
            serde_json::from_value(json!({
                "id":id,"backend":backend,"guest":"linux","kernel":kernel,"cpus":1,"memory_mib":512
            }))
            .unwrap(),
        );
    }
    manager.worker_key_event_log_dir = Some(temp.path().into());
    assert_eq!(
        manager.view("apple").unwrap()["key_event_log"],
        json!({"enabled":false})
    );
    assert_eq!(
        manager.view("qemu").unwrap()["key_event_log"],
        json!({"enabled":false})
    );
    let start = |manager: &mut Manager| {
        manager
            .dispatch(
                json!({"method":"start","arguments":{"id":"apple"}}),
                &mut None,
            )
            .unwrap()
    };
    let first = start(manager);
    let path = first["key_event_log"]["path"].as_str().unwrap();
    assert_eq!(first["key_event_log"], json!({"enabled":true,"path":path}));
    let list = manager
        .dispatch(json!({"method":"list","arguments":{}}), &mut None)
        .unwrap();
    assert_eq!(list[0]["key_event_log"], first["key_event_log"]);
    let status = manager
        .dispatch(
            json!({"method":"status","arguments":{"id":"apple"}}),
            &mut None,
        )
        .unwrap();
    assert_eq!(status["key_event_log"], first["key_event_log"]);
    let expected = format!(
        "run\n--config\n{}\n--log-key-events\n{path}\n",
        manager.dir.join("apple.json").display()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while fs::read_to_string(temp.path().join("apple.json.argv"))
        .ok()
        .as_deref()
        != Some(&expected)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture did not receive expected argv"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // Let this fixture exit itself, then exercise status's real exit observation.
    writeln!(manager.running.get_mut("apple").unwrap().stdin, "done").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let status = manager.view("apple").unwrap();
        if status["state"] == "stopped" {
            assert_eq!(status["key_event_log"], json!({"enabled":false}));
            assert!(status["key_event_log"].get("path").is_none());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "fixture did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    let second = start(manager);
    assert_ne!(
        second["key_event_log"]["path"],
        first["key_event_log"]["path"]
    );
    manager
        .running
        .get_mut("apple")
        .unwrap()
        .force_stop()
        .unwrap();
    manager.worker_key_event_log_dir = None;
    assert_eq!(start(manager)["key_event_log"], json!({"enabled":false}));
    assert!(!manager.worker_key_event_logs.contains_key("apple"));
}

fn private_dir(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn key_event_log_dir_refuses_a_replaceable_ancestor_and_accepts_it_when_sticky() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("shared");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
    let logs = parent.join("logs");
    private_dir(&logs);
    // Group-writable and not sticky: another account in the group could rename `logs` and plant a symlink.
    let error = validate_worker_key_event_log_dir(&logs)
        .unwrap_err()
        .to_string();
    assert!(error.contains("group/other-writable ancestor"), "{error}");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o1775)).unwrap();
    validate_worker_key_event_log_dir(&logs).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    validate_worker_key_event_log_dir(&logs).unwrap();
}

#[test]
fn key_event_log_dir_is_resolved_to_the_canonical_path() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
    let logs = real.join("logs");
    private_dir(&logs);
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // A symlinked ancestor is allowed to be named by the operator, but the worker only ever gets the real path.
    let canonical = validate_worker_key_event_log_dir(&link.join("logs")).unwrap();
    assert_eq!(canonical, fs::canonicalize(&logs).unwrap());
    assert!(!canonical.starts_with(&link));
}

#[cfg(target_os = "macos")]
#[test]
fn key_event_log_dir_with_an_extended_acl_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    private_dir(&logs);
    validate_worker_key_event_log_dir(&logs).unwrap();
    let status = std::process::Command::new("/bin/chmod")
        .args([
            "+a",
            "everyone allow list,search,add_file,file_inherit",
            logs.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "chmod +a is needed to set up this test");
    let error = validate_worker_key_event_log_dir(&logs)
        .unwrap_err()
        .to_string();
    assert!(error.contains("extended ACL"), "{error}");
}
