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
    // Time-bounded: four daemons start at once here, and a loaded host can take over a second.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !path.join("control.sock").exists() && std::time::Instant::now() < deadline {
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
    // The VM is stopped, so commands fail before reaching a guest.
    for operation in ["exec", "exec_start"] {
        assert!(vm_control::call(
            &dir,
            operation,
            json!({"id":"registry-test","argv":["/bin/true"]}),
        )
        .is_err());
    }
    for operation in ["exec_poll", "exec_cancel"] {
        let rejected = vm_control::call(
            &dir,
            operation,
            json!({"id":"registry-test","job_id":"a".repeat(32)}),
        )
        .unwrap_err();
        assert!(rejected.to_string().contains("unknown"));
    }
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
    let windows_tool = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "vm_windows")
        .unwrap();
    assert!(windows_tool["description"]
        .as_str()
        .unwrap()
        .contains("Linux and Windows Guests return unsupported"));
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
    assert_eq!(
        unsupported["result"]["content"][0]["text"],
        "unsupported: window enumeration requires enabled macOS Guest observation service"
    );
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
        json!(["virgl", "venus-moltenvk-experimental", "virtio-2d"])
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

const PRODUCT_CAPABILITIES: [&str; 7] = [
    "clipboard",
    "accessibility_metadata",
    "application_tools",
    "unicode_text_input",
    "retina_input_mapping",
    "multi_display_input_mapping",
    "window_metadata",
];
/// Every product capability is present with an explicit status; an unsupported one says why, and nothing is omitted.
fn assert_explicit(entries: &Value, supported: &[&str]) {
    let object = entries.as_object().unwrap();
    assert_eq!(object.len(), PRODUCT_CAPABILITIES.len(), "{entries}");
    for name in PRODUCT_CAPABILITIES {
        let entry = &entries[name];
        if supported.contains(&name) {
            assert_eq!(entry["status"], "supported", "{name}: {entry}");
            assert!(entry.get("reason").is_none(), "{name}: {entry}");
            assert!(
                entry["requirements"].as_str().unwrap().len() > 20,
                "{name}: {entry}"
            );
        } else {
            assert_eq!(entry["status"], "unsupported", "{name}: {entry}");
            let reason = entry["reason"].as_str().unwrap();
            assert!(reason.len() > 20 && reason.len() < 260, "{name}: {reason}");
        }
    }
}
#[test]
fn capabilities_state_each_unimplemented_product_capability_per_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let _d = daemon(&dir);
    let report = vm_control::call(&dir, "capabilities", json!({})).unwrap();
    for (backend, guest) in [
        ("apple-virtualization", "macos"),
        ("apple-virtualization", "linux"),
        ("qemu-hvf", "linux"),
        ("qemu-hvf", "macos"),
        ("qemu-hvf", "windows"),
    ] {
        let cells = &report["backends"][backend]["control_features"][guest];
        for name in [
            "snapshot",
            "balloon",
            "gpu_3d",
            "clipboard",
            "application_launch_list",
            "accessibility_tree",
            "window_list",
        ] {
            assert!(vm_control::FEATURE_STATUSES.contains(&cells[name]["status"].as_str().unwrap()));
            assert!(!cells[name]["reason"].as_str().unwrap().is_empty());
        }
        // The desktop tools serve Apple Virtualization desktop containers, macOS containers and Windows machines.
        let desktop = (backend == "apple-virtualization") != (guest == "windows");
        assert_eq!(
            cells["accessibility_tree"]["status"],
            if desktop {
                "implemented_unverified"
            } else {
                "unsupported"
            }
        );
    }

    // The response shape that existed before the report is unchanged.
    assert_eq!(report["backend"], "apple-virtualization");
    assert_eq!(report["guests"], json!(["linux", "macos"]));
    assert_eq!(report["windows"], false);
    assert_eq!(report["remote_desktop"], false);
    assert!(report["window_enumeration"].is_string());
    assert!(report["resource_admission"]["vm_memory_budget_mib"].is_u64());
    let apple = &report["backends"]["apple-virtualization"];
    assert_eq!(apple["guest_os"], json!(["linux", "macos"]));
    assert_eq!(apple["lifecycle"], true);
    assert_eq!(apple["window_enumeration"], true);
    let qemu = &report["backends"]["qemu-hvf"];
    assert_eq!(qemu["registered"], false);
    assert_eq!(
        qemu["capabilities"]["guest_os"],
        json!(["linux", "windows"])
    );
    assert_eq!(qemu["capabilities"]["input"], false);
    assert_eq!(qemu["capabilities"]["window_enumeration"], false);

    // macOS on Apple Virtualization: window metadata, the pixel-to-point mapping and the container desktop tools
    // have code; the rest does not.
    let mac = &apple["product_capabilities"]["macos"];
    assert_explicit(
        mac,
        &[
            "accessibility_metadata",
            "application_tools",
            "retina_input_mapping",
            "window_metadata",
        ],
    );
    assert!(mac["retina_input_mapping"]["evidence"]
        .as_str()
        .unwrap()
        .contains("no live Guest input acceptance"));
    assert!(mac["window_metadata"]["requirements"]
        .as_str()
        .unwrap()
        .contains("no titles"));
    assert!(mac["accessibility_metadata"]["requirements"]
        .as_str()
        .unwrap()
        .contains("vm-macos-desktop helper over SSH"));
    assert!(mac["multi_display_input_mapping"]["reason"]
        .as_str()
        .unwrap()
        .contains("main display only"));
    assert!(mac["unicode_text_input"]["reason"]
        .as_str()
        .unwrap()
        .contains("vm_input_text is refused"));
    // Linux desktop containers on Apple Virtualization have the window, accessibility and application tools;
    // Windows implements none. Linux on QEMU has Unicode text input only, and only under the stated requirements.
    let apple_linux = &apple["product_capabilities"]["linux"];
    assert_explicit(
        apple_linux,
        &[
            "accessibility_metadata",
            "application_tools",
            "window_metadata",
        ],
    );
    assert!(apple_linux["unicode_text_input"]["reason"]
        .as_str()
        .unwrap()
        .contains("requires a Linux Guest on QEMU"));
    for name in [
        "accessibility_metadata",
        "application_tools",
        "window_metadata",
    ] {
        assert!(apple_linux[name]["requirements"]
            .as_str()
            .unwrap()
            .contains("Linux desktop containers (--desktop)"));
    }
    let qemu_linux = &qemu["product_capabilities"]["linux"];
    assert_explicit(qemu_linux, &["unicode_text_input"]);
    let text_requirements = qemu_linux["unicode_text_input"]["requirements"]
        .as_str()
        .unwrap();
    for requirement in [
        "vm_input_text",
        "qemu_guest_exec",
        "--allow-text-input",
        "X11",
        "no Wayland",
        "explicit unsupported error",
    ] {
        assert!(text_requirements.contains(requirement), "{requirement}");
    }
    let text_evidence = qemu_linux["unicode_text_input"]["evidence"]
        .as_str()
        .unwrap();
    for limit in [
        "real Xorg n=1",
        "record not retained in this tree",
        "host path offline-tested only",
        "application acceptance is not observed",
    ] {
        assert!(text_evidence.contains(limit), "{limit}");
    }
    assert!(qemu_linux["retina_input_mapping"]["reason"]
        .as_str()
        .unwrap()
        .contains("no display scale factor"));
    assert!(qemu_linux["window_metadata"]["reason"]
        .as_str()
        .unwrap()
        .contains("refused for Linux Guests"));
    assert!(qemu_linux["application_tools"]["reason"]
        .as_str()
        .unwrap()
        .contains("no application launch or list tool"));
    for linux in [apple_linux, qemu_linux] {
        assert!(linux["clipboard"]["reason"]
            .as_str()
            .unwrap()
            .contains("no clipboard channel"));
    }
    let windows = &qemu["product_capabilities"]["windows"];
    assert_explicit(
        windows,
        &[
            "window_metadata",
            "accessibility_metadata",
            "application_tools",
        ],
    );
    assert!(windows["clipboard"]["reason"]
        .as_str()
        .unwrap()
        .contains("not implemented for Windows guests"));
    assert_eq!(apple["product_capabilities"].as_object().unwrap().len(), 2);
    // Known backend limits are stated for the macOS Guest only: no Linux entry, nothing under QEMU.
    assert_eq!(apple["known_limits"].as_object().unwrap().len(), 1);
    assert!(qemu.get("known_limits").is_none());
    assert!(qemu["capabilities"].get("known_limits").is_none());
    let limits = apple["known_limits"]["macos"].as_array().unwrap();
    assert_eq!(limits.len(), 2);
    assert_eq!(
        limits[0]["code"],
        "macos_guest_metal_large_compute_pipelines"
    );
    assert_eq!(
        limits[0]["decision"],
        "docs/decisions/0023-macos-guest-metal-compute-limit.md"
    );
    assert!(limits[0]["description"]
        .as_str()
        .unwrap()
        .contains("Cycles rendering on the Guest's Metal device is not available"));
    assert_eq!(limits[1]["code"], "macos_guest_opengl_not_accelerated");
    assert_eq!(
        limits[1]["decision"],
        "docs/decisions/0017-macos-opengl-capability-gap.md"
    );
    assert!(limits[1]["description"]
        .as_str()
        .unwrap()
        .contains("no accelerated OpenGL (CGL) renderer"));
    assert!(limits[1]["description"]
        .as_str()
        .unwrap()
        .contains("Apple Software Renderer"));
    for limit in limits {
        assert_eq!(
            limit["classification"],
            "backend limit of Apple's paravirtual graphics, not of this product's implementation"
        );
        assert!(limit["measured_on"]
            .as_str()
            .unwrap()
            .contains("macOS 26.6.2 Guest"));
    }
    assert_eq!(qemu["product_capabilities"].as_object().unwrap().len(), 2);

    // The report agrees with what the daemon actually does: a Windows Guest is refused, a Linux window list is refused,
    // and a registered VM states the same entries in its own status.
    assert!(vm_control::call(
        &dir,
        "create",
        json!({"id":"win","guest":"windows","cpus":2,"memory_mib":4096})
    )
    .is_err());
    let kernel = tmp.path().join("kernel");
    std::fs::write(&kernel, b"registry only, never booted").unwrap();
    let status = vm_control::call(
        &dir,
        "create",
        json!({"id":"cap-linux","guest":"linux","cpus":1,"memory_mib":512,"kernel":kernel}),
    )
    .unwrap();
    assert_eq!(status["capabilities"]["product_capabilities"], *apple_linux);
    assert!(status["capabilities"].get("known_limits").is_none());
    assert_eq!(status["capabilities"]["window_enumeration"], false);
    assert!(vm_control::call(&dir, "windows", json!({"id":"cap-linux"}))
        .unwrap_err()
        .to_string()
        .contains("unsupported"));
    // Unicode text input is refused, by name and before any worker is consulted, for every VM that is not a Linux
    // Guest on QEMU with qemu_guest_exec. (QEMU without qemu_guest_exec needs a registered QEMU runtime and is
    // asserted in the daemon unit test text_input_is_gated_by_vm_kind_and_never_echoes_text.)
    let disk = tmp.path().join("disk");
    std::fs::write(&disk, b"registry only, never booted").unwrap();
    let identity = tmp.path().join("identity");
    std::fs::create_dir(&identity).unwrap();
    let mac_status = vm_control::call(
        &dir,
        "create",
        json!({"id":"cap-mac","guest":"macos","cpus":2,"memory_mib":4096,"disk":disk,"machine_identity_dir":identity}),
    )
    .unwrap();
    assert_eq!(mac_status["capabilities"]["product_capabilities"], *mac);
    assert_eq!(
        mac_status["capabilities"]["known_limits"],
        apple["known_limits"]["macos"]
    );
    const TEXT_REFUSAL: &str = "unsupported: Unicode text input requires a Linux Guest on QEMU registered with qemu_guest_exec and a Guest service started with --allow-text-input";
    let typed = "never-typed \u{4f60}\u{597d}";
    for id in ["cap-linux", "cap-mac"] {
        for method in ["input_text", "input_text_step"] {
            let error = vm_control::call(
                &dir,
                method,
                json!({"id":id,"frame_id":"f".repeat(36),"width":1280,"height":800,"source_generation":1,"text":typed,"owner":"a".repeat(64),"epoch":1}),
            )
            .unwrap_err()
            .to_string();
            assert_eq!(error, TEXT_REFUSAL, "{id} {method}");
        }
    }

    // The MCP tool returns the same report, and offers no tool for any capability reported as unsupported everywhere.
    let mut child = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .args([
            "mcp",
            "--allow-lifecycle",
            "--allow-execute",
            "--allow-files",
            "--allow-input",
            "--state-dir",
        ])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let _mcp = Daemon { child };
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"capability-test","version":"1"}}})).unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert!(serde_json::from_str::<Value>(&line).unwrap()["result"].is_object());
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let mut exchange = |request: Value| -> Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let tools = exchange(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"vm_capabilities") && names.contains(&"vm_input_key"));
    // Exactly one tool types text, and it is the one product_capabilities names for Linux on QEMU
    // (vm_screen_text reads text from the screen).
    assert_eq!(
        names
            .iter()
            .filter(
                |name| (name.contains("text") && name.starts_with("vm_input"))
                    || name.contains("type")
            )
            .collect::<Vec<_>>(),
        [&"vm_input_text"]
    );
    assert!(text_requirements.contains("vm_input_text"));
    for name in &names {
        for absent in ["clipboard", "paste", "ax_"] {
            assert!(
                !name.contains(absent),
                "{name} exists: update product_capabilities before reporting {absent} as unsupported"
            );
        }
    }
    // The application and accessibility tools exist, and the report names them for Apple Virtualization.
    let apple = &report["backends"]["apple-virtualization"]["product_capabilities"];
    for (tool, capability) in [
        ("vm_desktop_apps", "application_tools"),
        ("vm_accessibility_tree", "accessibility_metadata"),
    ] {
        assert!(names.contains(&tool), "{tool}");
        for guest in ["linux", "macos"] {
            let requirements = apple[guest][capability]["requirements"].as_str().unwrap();
            assert!(
                requirements.contains(tool),
                "{guest} {capability}: {requirements}"
            );
        }
    }
    // Through MCP the refusal is the same explicit error, and the text is not echoed.
    for id in ["cap-linux", "cap-mac"] {
        let reply = exchange(
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"vm_input_text","arguments":{"id":id,"frame_id":"f".repeat(36),"width":1280,"height":800,"source_generation":1,"text":typed}}}),
        );
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        let message = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(message.contains(TEXT_REFUSAL), "{message}");
        assert!(!reply.to_string().contains("never-typed"), "{reply}");
    }
    // The internal per-chunk method is not an MCP tool.
    let reply = exchange(
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"vm_input_text_step","arguments":{"id":"cap-linux"}}}),
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown"),
        "{reply}"
    );
    let reply = exchange(
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vm_capabilities","arguments":{}}}),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let through_mcp: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(through_mcp["backends"], report["backends"]);
}
#[test]
fn stdio_tool_groups_and_progressive_disclosure() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let _d = daemon(&dir);
    let rejected = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .args(["mcp", "--tools", "core,shell", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let child = Command::new(env!("CARGO_BIN_EXE_vm-control"))
        .env("VEYNODEVM_HOME", dir.parent().unwrap())
        .args(["mcp", "--tools", "core", "--state-dir"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut mcp = Daemon { child };
    let mut input = mcp.child.stdin.take().unwrap();
    let mut output = BufReader::new(mcp.child.stdout.take().unwrap());
    let mut send = |request: Value| writeln!(input, "{request}").unwrap();
    let mut read = || {
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    send(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"groups-test","version":"1"}}}),
    );
    assert_eq!(
        read()["result"]["capabilities"]["tools"]["listChanged"],
        true
    );
    send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let names = |reply: &Value| -> Vec<String> {
        reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let listed = read();
    let core = names(&listed);
    assert!(core.contains(&"vm_list".into()) && core.contains(&"vm_tools_enable".into()));
    assert!(!core.contains(&"vm_screenshot".into()));
    let enable = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "vm_tools_enable")
        .unwrap();
    // Observe-only: groups that need a permission this session lacks are not offered;
    // cua holds observe tools (cua_get_window_state, ...), so it is.
    assert_eq!(
        enable["inputSchema"]["properties"]["groups"]["items"]["enum"],
        json!(["screen", "desktop", "snapshots", "cua"])
    );
    // A permitted but hidden tool is refused with a pointer to the enable tool.
    send(
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vm_screenshot","arguments":{"id":"x"}}}),
    );
    assert!(read()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("vm_tools_enable"));
    for groups in [json!(["input"]), json!(["exec"]), json!([])] {
        send(
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"vm_tools_enable","arguments":{"groups":groups}}}),
        );
        assert!(read().get("error").is_some(), "{groups}");
    }
    send(
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"vm_tools_enable","arguments":{"groups":["screen"]}}}),
    );
    let enabled = read();
    assert_eq!(enabled["id"], 5);
    let result: Value =
        serde_json::from_str(enabled["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["enabled"], json!(["screen"]));
    assert!(result["added_tools"]
        .as_array()
        .unwrap()
        .contains(&json!("vm_screenshot")));
    assert_eq!(
        read(),
        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})
    );
    send(json!({"jsonrpc":"2.0","id":6,"method":"tools/list"}));
    let widened = names(&read());
    assert!(
        widened.contains(&"vm_screenshot".into()) && widened.contains(&"vm_tools_enable".into())
    );
    // Still observe-only: enabling groups never adds a tool the permissions withhold.
    assert!(!widened
        .iter()
        .any(|n| n == "vm_start" || n.starts_with("vm_input_")));
}
