use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};
struct Daemon {
    child: Child,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
/// Each test daemon gets its own VEYNODEVM_HOME so a developer or user
/// runtime.json (QEMU/Venus registration) never leaks into its behaviour.
fn daemon(path: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .env("VEYNODEVM_HOME", path.parent().unwrap())
        .args(["daemon", "--state-dir"])
        .arg(path)
        .args(["--backend", "/usr/bin/false"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if path.join("control.sock").exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Daemon { child }
}
#[test]
fn registry_protocol_and_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let mut d = daemon(&dir);
    assert_eq!(
        std::fs::metadata(dir.join("control.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        vm_control::call(&dir, "list", json!({})).unwrap(),
        json!([])
    );
    assert!(vm_control::call(
        &dir,
        "create",
        json!({"id":"bad","guest":"windows","cpus":2,"memory_mib":1024})
    )
    .is_err());
    let kernel = tmp.path().join("kernel");
    std::fs::write(&kernel, b"not bootable; registry-only test").unwrap();
    let cfg =
        json!({"id":"registry-test","guest":"linux","cpus":2,"memory_mib":1024,"kernel":kernel});
    assert!(vm_control::call(&dir, "create", cfg.clone()).is_ok());
    assert!(vm_control::call(&dir, "create", cfg).is_err());
    assert_eq!(
        std::fs::metadata(dir.join("registry.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        vm_control::call(&dir, "status", json!({"id":"registry-test"})).unwrap()["state"],
        "stopped"
    );
    let owner = "a".repeat(64);
    let acquired = vm_control::call(
        &dir,
        "acquire_control",
        json!({"id":"registry-test","owner":owner}),
    )
    .unwrap();
    let epoch = acquired["epoch"].as_u64().unwrap();
    assert!(vm_control::call(
        &dir,
        "acquire_control",
        json!({"id":"registry-test","owner":"b".repeat(64)})
    )
    .is_err());
    vm_control::call(&dir, "human_takeover", json!({"id":"registry-test"})).unwrap();
    let rejected = vm_control::call(
        &dir,
        "exec",
        json!({"id":"registry-test","owner":owner,"epoch":epoch,"argv":["/bin/true"]}),
    )
    .unwrap_err();
    assert!(rejected.to_string().contains("revoked"));
    let rejected_start = vm_control::call(
        &dir,
        "exec_start",
        json!({"id":"registry-test","owner":owner,"epoch":epoch,"argv":["/bin/true"]}),
    )
    .unwrap_err();
    assert!(rejected_start.to_string().contains("revoked"));
    for operation in ["exec_poll", "exec_cancel"] {
        let rejected = vm_control::call(
            &dir,
            operation,
            json!({"id":"registry-test","owner":owner,"epoch":epoch,"job_id":"a".repeat(32)}),
        )
        .unwrap_err();
        assert!(rejected.to_string().contains("not owned"));
    }

    assert!(vm_control::call(
        &dir,
        "acquire_control",
        json!({"id":"registry-test","owner":owner})
    )
    .is_err());
    vm_control::call(&dir, "human_release", json!({"id":"registry-test"})).unwrap();
    let next = vm_control::call(
        &dir,
        "acquire_control",
        json!({"id":"registry-test","owner":owner}),
    )
    .unwrap();
    assert!(next["epoch"].as_u64().unwrap() > epoch);
    let duplicate = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .env("VEYNODEVM_HOME", dir.parent().unwrap())
        .args(["daemon", "--state-dir"])
        .arg(&dir)
        .args(["--backend", "/usr/bin/false"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    d.child.kill().unwrap();
    d.child.wait().unwrap();
    std::fs::remove_file(dir.join("control.sock")).unwrap();
    let _next = daemon(&dir);
    assert_eq!(
        vm_control::call(&dir, "list", json!({}))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn stdio_actual_client_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let _d = daemon(&dir);
    let mut child = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .env("VEYNODEVM_HOME", dir.parent().unwrap())
        .args(["mcp", "--state-dir"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    fn exchange(input: &mut impl Write, output: &mut impl BufRead, request: Value) -> Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    assert!(exchange(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":0,"method":"tools/list"})
    )
    .get("error")
    .is_some());
    let result = exchange(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}}),
    );
    assert_eq!(result["result"]["protocolVersion"], vm_control::VERSION);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    assert_eq!(
        exchange(
            &mut input,
            &mut output,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vm_list","arguments":{}}})
        )["result"]["isError"],
        false
    );
    assert!(exchange(&mut input,&mut output,json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vm_start","arguments":{"id":"x"}}})).get("error").is_some());
    assert!(exchange(&mut input,&mut output,json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"vm_list","arguments":{"unexpected":1}}})).get("error").is_some());
    let tools = exchange(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":5,"method":"tools/list"}),
    );
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "vm_windows"));
    let kernel = tmp.path().join("observe-kernel");
    std::fs::write(&kernel, b"registry only").unwrap();
    vm_control::call(
        &dir,
        "create",
        json!({"id":"observe-linux","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
    )
    .unwrap();
    let unsupported = exchange(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"vm_windows","arguments":{"id":"observe-linux"}}}),
    );
    assert_eq!(unsupported["result"]["isError"], true);
    assert!(unsupported["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("unsupported"));
    assert!(exchange(&mut input,&mut output,json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"vm_windows","arguments":{"id":"observe-linux","include_titles":true}}})).get("error").is_some());
    drop(input);
    assert!(child.wait().unwrap().success());
    assert!(vm_control::call(&dir, "list", json!({})).is_ok());
}

#[test]
fn stdio_graphics_profile_schema_and_fail_closed_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let _daemon = daemon(&dir);
    let mut child = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .env("VEYNODEVM_HOME", dir.parent().unwrap())
        .args(["mcp", "--allow-lifecycle", "--state-dir"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let mut owned = Daemon { child };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    fn exchange(
        input: &mut impl Write,
        rx: &std::sync::mpsc::Receiver<std::io::Result<String>>,
        request: Value,
    ) -> Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap()).unwrap()
    }
    assert!(exchange(&mut input, &rx, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"graphics-test","version":"1"}}}))["result"].is_object());
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();

    let tools = exchange(
        &mut input,
        &rx,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let schema = &tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "vm_create")
        .unwrap()["inputSchema"];
    assert_eq!(
        schema["properties"]["qemu_graphics_profile"]["enum"],
        json!(["virgl", "venus-moltenvk-experimental"])
    );
    assert_eq!(
        schema["properties"]["qemu_graphics_profile"]["default"],
        "virgl"
    );
    assert!(schema["properties"].get("qemu_guest_exec").is_none());
    assert_eq!(schema["additionalProperties"], false);
    let kernel = tmp.path().join("kernel");
    std::fs::write(&kernel, b"registry only, never booted").unwrap();
    let args = json!({"id":"venus","backend":"qemu-hvf","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel,"qemu_graphics_profile":"venus-moltenvk-experimental"});
    let denied = exchange(
        &mut input,
        &rx,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vm_create","arguments":args}}),
    );
    assert!(
        denied.get("error").is_none(),
        "schema rejected the authorized enum: {denied}"
    );
    assert_eq!(denied["result"]["isError"], true);
    assert!(denied["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("not registered"));
    for (i, value) in [json!("unknown"), json!(1), json!(null)]
        .into_iter()
        .enumerate()
    {
        let mut invalid = args.clone();
        invalid["qemu_graphics_profile"] = value;
        let denied = exchange(
            &mut input,
            &rx,
            json!({"jsonrpc":"2.0","id":10+i,"method":"tools/call","params":{"name":"vm_create","arguments":invalid}}),
        );
        assert_eq!(denied["result"]["isError"], true);
    }
    for key in ["qemu_guest_exec", "venus_runtime", "render_server"] {
        let mut invalid = args.clone();
        invalid[key] = json!(false);
        let denied = exchange(
            &mut input,
            &rx,
            json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"vm_create","arguments":invalid}}),
        );
        assert_eq!(denied["error"]["code"], -32602);
    }
    assert_eq!(
        vm_control::call(&dir, "list", json!({})).unwrap(),
        json!([])
    );
    drop(input);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = owned.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "MCP EOF timeout");
        thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
}
