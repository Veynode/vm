//! Pushed MCP events through the real stdio adapter and the real TLS endpoint,
//! against a synthetic daemon socket (tests/mcp_events.py). No VM or Guest.
fn run(transport: &str) -> serde_json::Value {
    let mut command = std::process::Command::new(python());
    command
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mcp_events.py"))
        .arg(env!("CARGO_BIN_EXE_vm-control"))
        .arg(transport);
    if transport == "https" {
        command.arg(env!("CARGO_BIN_EXE_vm-mcp-http"));
    }
    let output = command
        .output()
        .expect("Python 3 (with OpenSSL for TLS 1.3 on https) required");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["passed"], true);
    println!("{}", String::from_utf8_lossy(&output.stdout));
    evidence
}

#[test]
fn stdio_events_resources_and_progress() {
    let evidence = run("stdio");
    assert!(evidence["checks"].as_array().unwrap().len() >= 24);
}

#[test]
fn https_events_are_streamed_filtered_and_bounded() {
    let evidence = run("https");
    assert!(evidence["checks"].as_array().unwrap().len() >= 30);
}

// TLS 1.3 needs a modern OpenSSL; the macOS system python3 links LibreSSL 2.8.
fn python() -> String {
    std::env::var("VEYNODE_PYTHON").unwrap_or_else(|_| "python3".into())
}

/// The real daemon publishes a worker's start and exit. The worker is
/// /usr/bin/false: it exits non-zero at once, which is a crash.
#[test]
fn daemon_publishes_worker_start_and_crash_once() {
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _daemon = Daemon(
        std::process::Command::new(env!("CARGO_BIN_EXE_vm-control"))
            .env("VEYNODEVM_HOME", tmp.path())
            .args(["daemon", "--state-dir"])
            .arg(&dir)
            .args(["--backend", "/usr/bin/false"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dir.join("control.sock").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let kernel = tmp.path().join("kernel");
    std::fs::write(&kernel, b"not bootable; event test").unwrap();
    vm_control::call(
        &dir,
        "create",
        json!({"id":"events","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
    )
    .unwrap();
    let cursor = vm_control::call(&dir, "events", json!({})).unwrap()["next"]
        .as_u64()
        .unwrap();
    vm_control::call(&dir, "start", json!({"id":"events"})).unwrap();
    let mut kinds = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !kinds.contains(&"crashed".to_string()) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
        // status observes the exit too; it must not publish it twice.
        vm_control::call(&dir, "status", json!({"id":"events"})).unwrap();
        let reply = vm_control::call(&dir, "events", json!({"after":cursor - 1})).unwrap();
        assert_eq!(reply["lost"], false);
        kinds = reply["events"]
            .as_array()
            .unwrap()
            .iter()
            .inspect(|e| {
                assert_eq!(e["vm"], "events");
                assert!(e.get("pid").is_none());
            })
            .map(|e| e["event"].as_str().unwrap().to_string())
            .collect();
    }
    assert_eq!(kinds, ["started", "crashed"]);
    let tail = vm_control::call(&dir, "log_read", json!({"id":"events","stream":"serial"}));
    assert!(tail.unwrap_err().to_string().contains("no serial log"));
}
