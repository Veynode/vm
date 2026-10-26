//! `driver_rpc` through `Manager::dispatch` with a fake worker: what each backend's worker
//! is sent, how its reply is unwrapped, and what never reaches it.
use super::*;

struct Fixture {
    manager: Manager,
    output: BufReader<std::process::ChildStdout>,
    _directory: tempfile::TempDir,
}
impl Fixture {
    /// One running VM per entry; its "worker" is `cat`, so the fixture reads back each command.
    fn new(vms: &[(&str, &str, &str)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = Manager::load(
            test_private_state(directory.path()),
            "/unused".into(),
            MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
        )
        .unwrap();
        let mut output = None;
        for (id, guest, backend) in vms {
            manager.configs.insert(
                (*id).into(),
                serde_json::from_value(
                    json!({"id":id,"guest":guest,"backend":backend,"cpus":1,"memory_mib":512}),
                )
                .unwrap(),
            );
            let mut child = Command::new("/bin/cat")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            output.get_or_insert(child.stdout.take().unwrap());
            manager.running.insert(
                (*id).into(),
                Running {
                    bridge: Arc::new(Mutex::new(bridge::State::new().unwrap())),
                    child,
                    stdin,
                    pending: Arc::new(Mutex::new(BTreeMap::new())),
                    assigned_memory_mib: 512,
                    expected_exit: false,
                    execution_quiescing: false,
                    guest_async_timeout: Arc::new(Mutex::new(GuestAsyncTimeout::default())),
                    exit_status: None,
                    disk_identity: None,
                    state: Arc::new(Mutex::new(json!({"state":"running"}))),
                    next: 1,
                },
            );
        }
        Self {
            manager,
            output: BufReader::new(output.unwrap()),
            _directory: directory,
        }
    }
    /// Dispatches; returns the immediate reply or the worker command and a completion.
    fn dispatch(&mut self, args: Value) -> (Value, Option<Pending>) {
        let mut completion = None;
        let value = self
            .manager
            .dispatch(
                json!({"method":"driver_rpc","arguments":args}),
                &mut completion,
            )
            .unwrap();
        match completion {
            None => (value, None),
            Some(Completion::Backend(pending)) => {
                let mut line = String::new();
                self.output.read_line(&mut line).unwrap();
                (serde_json::from_str(&line).unwrap(), Some(pending))
            }
            Some(_) => panic!("driver_rpc completes through the worker"),
        }
    }
    fn answer(&mut self, vm: &str, pending: Pending, reply: Value) -> Result<Value> {
        self.manager.running[vm]
            .pending
            .lock()
            .unwrap()
            .remove(&pending.number)
            .unwrap()
            .send(reply)
            .unwrap();
        pending.wait()
    }
}

fn unverifiable() -> Value {
    json!({"content":[{"type":"text","text":"Clicked"}],"structuredContent":{"effect":"unverifiable","route":"accessibility"}})
}

#[test]
fn apple_virtualization_sends_through_the_guest_agent() {
    let mut fixture = Fixture::new(&[("lnx", "linux", "apple-virtualization")]);
    let (wire, pending) = fixture.dispatch(json!({"id":"lnx","tool":"click","arguments":{"element_token":"s1:3"},"scopes":["observe","input"]}));
    let pending = pending.unwrap();
    assert_eq!(wire["method"], "guest_request");
    let request = &wire["request"];
    assert_eq!(request["method"], "driver_rpc");
    assert_eq!(request["arguments"]["deadline_ms"], 30000);
    let frame = &request["arguments"]["frame"];
    assert_eq!(
        frame,
        &json!({"jsonrpc":"2.0","id":wire["id"],"method":"tools/call","params":{"name":"click","arguments":{"element_token":"s1:3"}}})
    );
    assert_eq!(pending.timeout, Duration::from_secs(35));
    let reply = json!({"id":wire["id"],"result":{"id":request["id"],"result":{"frame":{"jsonrpc":"2.0","id":wire["id"],"result":unverifiable()}}}});
    assert_eq!(
        fixture.answer("lnx", pending, reply).unwrap(),
        unverifiable()
    );
}

#[test]
fn qemu_sends_on_the_driver_port_and_caches_the_tool_list() {
    let mut fixture = Fixture::new(&[("qm", "linux", "qemu-hvf")]);
    let (wire, pending) = fixture.dispatch(json!({"id":"qm","list":true,"scopes":["observe"]}));
    assert_eq!(wire["method"], "driver_rpc");
    assert_eq!(wire["frame"]["method"], "tools/list");
    let list =
        json!({"tools":[{"name":"click"},{"name":"get_window_state"},{"name":"set_config"}]});
    let reply =
        json!({"id":wire["id"],"result":{"frame":{"jsonrpc":"2.0","id":wire["id"],"result":list}}});
    let tools = fixture.answer("qm", pending.unwrap(), reply).unwrap();
    assert_eq!(tools, json!({"tools":[{"name":"get_window_state"}]}));
    // Served from the cache, without the worker, and filtered for the caller's scopes.
    let (cached, pending) =
        fixture.dispatch(json!({"id":"qm","list":true,"scopes":["observe","input"]}));
    assert!(pending.is_none());
    assert_eq!(
        cached,
        json!({"tools":[{"name":"click"},{"name":"get_window_state"}]})
    );
}

#[test]
fn refusals_never_reach_the_worker_and_guest_errors_are_errors() {
    let mut fixture = Fixture::new(&[("lnx", "linux", "apple-virtualization")]);
    for args in [
        json!({"id":"lnx","tool":"click","arguments":{}}),
        json!({"id":"lnx","tool":"set_config","arguments":{},"scopes":["observe","input","browser","clipboard","execute"]}),
    ] {
        let (reply, pending) = fixture.dispatch(args);
        assert!(pending.is_none());
        assert_eq!(reply["isError"], true);
    }
    assert!(fixture.manager.running["lnx"]
        .pending
        .lock()
        .unwrap()
        .is_empty());
    let (wire, pending) =
        fixture.dispatch(json!({"id":"lnx","tool":"list_windows","arguments":{}}));
    let reply = json!({"id":wire["id"],"result":{"id":wire["request"]["id"],"error":{"code":"unsupported","message":"unsupported: cua-driver not in this image"}}});
    let error = fixture.answer("lnx", pending.unwrap(), reply).unwrap_err();
    assert!(
        error.to_string().contains("cua-driver not in this image"),
        "{error}"
    );
}

#[test]
fn an_attached_console_keeps_background_delivery() {
    let mut fixture = Fixture::new(&[("qm", "linux", "qemu-hvf")]);
    let text = json!({"id":"qm","tool":"type_text","arguments":{"text":"Zoë"},"scopes":["observe","input"]});
    let (wire, _pending) = fixture.dispatch(text.clone());
    assert_eq!(
        wire["frame"]["params"]["arguments"]["delivery_mode"],
        "foreground"
    );
    fixture
        .manager
        .native_connections
        .insert("qm".into(), ("owner".into(), "worker".into()));
    let (wire, _pending) = fixture.dispatch(text);
    assert!(wire["frame"]["params"]["arguments"]
        .get("delivery_mode")
        .is_none());
}

#[test]
fn windows_and_macos_guests_go_over_a_driver_link() {
    let mut fixture = Fixture::new(&[
        ("win", "windows", "qemu-hvf"),
        ("mac", "macos", "apple-virtualization"),
    ]);
    for vm in ["win", "mac"] {
        let mut completion = None;
        let result = fixture.manager.dispatch(
            json!({"method":"driver_rpc","arguments":{"id":vm,"tool":"list_apps"}}),
            &mut completion,
        );
        match result {
            Ok(_) => assert!(matches!(completion, Some(crate::Completion::Driver { .. }))),
            // A fixture Windows machine has no SSH key to reach; never "unsupported".
            Err(error) => assert!(!error.to_string().starts_with("unsupported"), "{error}"),
        }
    }
}

#[test]
fn a_person_in_control_holds_back_host_and_driver_input() {
    let mut fixture = Fixture::new(&[("vm", "linux", "apple-virtualization")]);
    let now = std::time::Instant::now();
    fixture.manager.leases.viewer("vm", "window", true, now);
    fixture.manager.leases.human_input("vm", now);
    let mut completion = None;
    let error = fixture
        .manager
        .dispatch(
            json!({"method":"input_click","arguments":{"id":"vm","x":1,"y":1}}),
            &mut completion,
        )
        .unwrap_err();
    assert!(
        error.to_string().starts_with("person_in_control: "),
        "{error}"
    );
    let refused = fixture
        .manager
        .dispatch(
            json!({"method":"driver_rpc","arguments":{"id":"vm","tool":"click","arguments":{"pid":1,"x":1,"y":1},"scopes":{"input":true}}}),
            &mut completion,
        )
        .unwrap();
    assert_eq!(refused["structuredContent"]["code"], "person_in_control");
    assert!(completion.is_none(), "nothing reached the worker");
    let status = fixture
        .manager
        .dispatch(
            json!({"method":"control_lease","arguments":{"id":"vm"}}),
            &mut completion,
        )
        .unwrap();
    assert_eq!(status["holder"], "person");
    // Closing the window hands control back at once.
    fixture.manager.leases.viewer("vm", "window", false, now);
    assert!(fixture.manager.leases.acquire("vm", now).is_ok());
}
