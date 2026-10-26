//! Parse the real CLI without starting a daemon or a VM.
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::Path,
    process::Command,
};

fn cli(action: &str, path: &Path) -> String {
    // An empty home: the daemon must not pick up this user's runtime.json.
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .env("VEYNODEVM_HOME", home.path())
        .args([
            action,
            "--state-dir",
            "/unused",
            "--worker-key-event-log-dir",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn worker_key_event_log_cli_rejects_unsafe_directories() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing");
    let file = temp.path().join("file");
    fs::write(&file, b"unchanged").unwrap();
    let private = temp.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let link = temp.path().join("link");
    symlink(&private, &link).unwrap();
    let wrong_mode = temp.path().join("wrong-mode");
    fs::create_dir(&wrong_mode).unwrap();
    for (path, expected) in [
        (Path::new("relative").to_path_buf(), "must be absolute"),
        (missing.clone(), "must exist"),
        (file.clone(), "must be a directory, not a symlink"),
        (link.clone(), "must be a directory, not a symlink"),
        (link.join(""), "must be a directory, not a symlink"),
        (link.join("."), "must be a directory, not a symlink"),
    ] {
        let error = cli("daemon", &path);
        assert!(error.contains(expected), "{path:?}: {error}");
    }
    for mode in [0o755, 0o750, 0o777, 0o1700] {
        fs::set_permissions(&wrong_mode, fs::Permissions::from_mode(mode)).unwrap();
        let error = cli("daemon", &wrong_mode);
        assert!(error.contains("must have mode 0700"), "{error}");
        assert_eq!(
            fs::metadata(&wrong_mode).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }
    if unsafe { libc::geteuid() } != 0 {
        assert!(cli("daemon", Path::new("/")).contains("must be owned by the current user"));
    }
    assert!(!missing.exists());
    assert_eq!(fs::read(&file).unwrap(), b"unchanged");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn worker_key_event_log_cli_accepts_valid_directory_only_for_daemon() {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    vm_control::validate_worker_key_event_log_dir(temp.path()).unwrap();
    // Reaching the next required option proves parsing/validation succeeded, without binding a socket.
    let error = cli("daemon", temp.path());
    assert!(error.contains("daemon requires --backend"), "{error}");
    for action in ["call", "mcp"] {
        let error = cli(action, temp.path());
        assert!(
            error.contains("--worker-key-event-log-dir is a local daemon option only"),
            "{error}"
        );
    }
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn worker_key_event_log_cli_requires_one_value() {
    for (args, expected) in [
        (
            vec!["daemon", "--worker-key-event-log-dir"],
            "missing worker key-event log directory",
        ),
        (
            vec![
                "daemon",
                "--worker-key-event-log-dir",
                "/a",
                "--worker-key-event-log-dir",
                "/b",
            ],
            "given more than once",
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_vm-control"))
            .env("VEYNODEVM_HOME", home.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(expected), "{error}");
    }
}
