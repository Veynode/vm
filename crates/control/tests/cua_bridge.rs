//! The cua-driver bridge against a fake driver: scopes are enforced before a frame leaves
//! the host, never-exposed tools stay hidden, the phase 0 policies rewrite only what they
//! should, deadlines follow the call, and the driver's results are not turned into success.
use serde_json::{json, Value};
use std::{sync::Mutex, time::Duration};
use vm_control::cua_bridge::{
    self, deadline, endpoint_call, prepare, run, vm_level_tools, Desktop, EndpointCall, Prepared,
    Request, Scopes, Transport,
};

/// Answers every frame with `reply` (its `result`) and records what it was sent.
struct FakeDriver {
    sent: Mutex<Vec<(Value, Duration)>>,
    reply: Value,
}
impl FakeDriver {
    fn new(reply: Value) -> Self {
        Self {
            sent: Mutex::new(vec![]),
            reply,
        }
    }
    fn sent(&self) -> Vec<(Value, Duration)> {
        self.sent.lock().unwrap().clone()
    }
}
impl Transport for FakeDriver {
    fn call(&self, frame: Value, deadline: Duration) -> anyhow::Result<Value> {
        let id = frame["id"].clone();
        self.sent.lock().unwrap().push((frame, deadline));
        Ok(json!({"jsonrpc":"2.0","id":id,"result":self.reply}))
    }
}

const ALONE: Desktop = Desktop {
    person_attached: false,
};
fn call(tool: &str, arguments: Value) -> Request {
    Request::Call {
        tool: tool.into(),
        arguments,
    }
}
fn input() -> Scopes {
    Scopes {
        input: true,
        ..Scopes::default()
    }
}
fn ok() -> Value {
    json!({"content":[{"type":"text","text":"done"}],"structuredContent":{"effect":"confirmed"}})
}
fn exchange(driver: &FakeDriver, request: Request, scopes: Scopes) -> Value {
    run(driver, request, scopes, ALONE, &mut None).unwrap()
}

#[test]
fn scope_refusals_happen_before_the_driver_sees_the_call() {
    let driver = FakeDriver::new(ok());
    for (tool, scopes) in [
        ("click", Scopes::default()),
        ("browser_navigate", input()),
        ("clipboard_read", input()),
        ("run_script", input()),
        ("launch_app", input()),
    ] {
        let reply = exchange(&driver, call(tool, json!({})), scopes);
        assert_eq!(reply["isError"], true, "{tool}");
        assert_eq!(reply["structuredContent"]["code"], "scope_not_granted");
    }
    // A step's tool is checked too, not only run_steps itself.
    let steps = json!({"steps":[{"tool":"click","args":{}},{"run_script":{"script":"1"}}]});
    let reply = exchange(&driver, call("run_steps", steps), input());
    assert_eq!(reply["structuredContent"]["code"], "scope_not_granted");
    assert!(driver.sent().is_empty());
    // Observe needs nothing.
    exchange(
        &driver,
        call("get_window_state", json!({})),
        Scopes::default(),
    );
    assert_eq!(driver.sent().len(), 1);
}

#[test]
fn never_exposed_and_unknown_tools_are_refused_and_not_listed() {
    let driver = FakeDriver::new(ok());
    for tool in cua_bridge::NEVER_EXPOSED
        .iter()
        .chain(&["some_future_tool"])
    {
        let reply = exchange(&driver, call(tool, json!({})), Scopes::all());
        assert_eq!(
            reply["structuredContent"]["code"], "tool_not_exposed",
            "{tool}"
        );
    }
    assert!(driver.sent().is_empty());
    let tools: Vec<Value> = [
        "get_window_state",
        "click",
        "browser_click",
        "clipboard_write",
        "run_script",
        "set_config",
        "install_extension",
        "check_for_update",
        "install_ffmpeg",
        "some_future_tool",
    ]
    .iter()
    .map(|name| json!({"name":name,"inputSchema":{"type":"object"}}))
    .collect();
    let driver = FakeDriver::new(json!({"tools":tools}));
    let mut cache = None;
    let names = |list: &Value| -> Vec<String> {
        list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let all = run(&driver, Request::List, Scopes::all(), ALONE, &mut cache).unwrap();
    assert_eq!(
        names(&all),
        [
            "get_window_state",
            "click",
            "browser_click",
            "clipboard_write",
            "run_script"
        ]
    );
    // The second listing comes from the per-VM cache, filtered for its own scopes.
    let observe = run(&driver, Request::List, Scopes::default(), ALONE, &mut cache).unwrap();
    assert_eq!(names(&observe), ["get_window_state"]);
    assert_eq!(driver.sent().len(), 1);
    assert_eq!(driver.sent()[0].0["method"], "tools/list");
}

#[test]
fn non_ascii_text_goes_foreground_unless_chosen_or_watched() {
    let sent = |arguments: Value, desktop: Desktop| -> Value {
        let driver = FakeDriver::new(ok());
        let reply = run(
            &driver,
            call("type_text", arguments),
            input(),
            desktop,
            &mut None,
        )
        .unwrap();
        let policy = reply["_meta"]["veynode/bridge_policy"].clone();
        let mut frame = driver.sent().remove(0).0;
        frame["policy"] = policy;
        frame
    };
    let frame = sent(json!({"text":"héllo 世界"}), ALONE);
    assert_eq!(frame["params"]["arguments"]["delivery_mode"], "foreground");
    assert_eq!(frame["policy"], json!(["type_text_foreground"]));
    let frame = sent(json!({"text":"plain ascii"}), ALONE);
    assert!(frame["params"]["arguments"].get("delivery_mode").is_none());
    assert!(frame["policy"].is_null());
    let frame = sent(json!({"text":"héllo","delivery_mode":"background"}), ALONE);
    assert_eq!(frame["params"]["arguments"]["delivery_mode"], "background");
    let frame = sent(
        json!({"text":"héllo"}),
        Desktop {
            person_attached: true,
        },
    );
    assert!(frame["params"]["arguments"].get("delivery_mode").is_none());
}

#[test]
fn run_steps_text_and_browser_clicks_get_the_linux_policies() {
    let driver = FakeDriver::new(ok());
    let steps = json!({"steps":[
        {"tool":"type_text","args":{"text":"Zoë"}},
        {"type_text":{"text":"ascii"}},
        {"type_text":{"text":"世界"}},
    ]});
    exchange(&driver, call("run_steps", steps), input());
    let steps = &driver.sent()[0].0["params"]["arguments"]["steps"];
    assert_eq!(steps[0]["args"]["delivery_mode"], "foreground");
    assert!(steps[1]["type_text"].get("delivery_mode").is_none());
    assert_eq!(steps[2]["type_text"]["delivery_mode"], "foreground");
    // A batch-wide delivery_mode is the caller's choice.
    let steps =
        json!({"delivery_mode":"background","steps":[{"tool":"type_text","args":{"text":"Zoë"}}]});
    exchange(&driver, call("run_steps", steps), input());
    assert!(
        driver.sent()[1].0["params"]["arguments"]["steps"][0]["args"]
            .get("delivery_mode")
            .is_none()
    );

    let browser = Scopes {
        browser: true,
        ..Scopes::default()
    };
    exchange(
        &driver,
        call("browser_click", json!({"ref":"p1:2"})),
        browser,
    );
    assert_eq!(
        driver.sent()[2].0["params"]["arguments"]["delivery_mode"],
        "foreground"
    );
    let route = json!({"ref":"p1:2","input_route":"dom_event"});
    exchange(&driver, call("browser_click", route.clone()), browser);
    assert_eq!(driver.sent()[3].0["params"]["arguments"], route);
}

#[test]
fn list_apps_keeps_the_desktop_applications() {
    let app = |name: &str, pid: u64, running: bool, kind: Value, windows: Value| json!({"name":name,"pid":pid,"running":running,"kind":kind,"windows":windows,"bundle_id":null,"launch_path":null,"active":false,"last_used":null});
    let reply = json!({"content":[{"type":"text","text":"Found 5 app(s)"}],"structuredContent":{"apps":[
        app("kthreadd", 2, true, Value::Null, json!([])),
        app("cua-driver", 1051, true, Value::Null, json!([])),
        app("xfdesktop", 973, true, Value::Null, json!([{"window_id":1}])),
        app("Mousepad", 1025, true, json!("desktop"), json!([{"window_id":2}])),
        app("Chromium Web Browser", 0, false, json!("desktop"), json!([])),
    ]}});
    let driver = FakeDriver::new(reply);
    let result = exchange(&driver, call("list_apps", json!({})), Scopes::default());
    let names: Vec<_> = result["structuredContent"]["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["xfdesktop", "Mousepad", "Chromium Web Browser"]);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("2 other Guest processes hidden"), "{text}");
    assert!(text.contains("- Mousepad (pid 1025)"));
    assert!(text.contains("- Chromium Web Browser (not running)"));
    assert!(!text.contains("kthreadd"));
}

#[test]
fn unverifiable_and_refused_results_pass_through_unchanged() {
    for reply in [
        json!({"content":[{"type":"text","text":"Clicked element [13]"}],"structuredContent":{"effect":"unverifiable","route":"accessibility","next_call":{"tool":"get_window_state"}}}),
        json!({"content":[{"type":"text","text":"refused"}],"isError":true,"structuredContent":{"effect":"refused","error":{"code":"browser_input_trust_unavailable"}}}),
        json!({"content":[],"structuredContent":{"results":[{"status":"unknown"}]}}),
    ] {
        let driver = FakeDriver::new(reply.clone());
        let result = exchange(
            &driver,
            call("click", json!({"element_token":"s1:13"})),
            input(),
        );
        assert_eq!(result, reply);
    }
    // A JSON-RPC error from the driver is a failed call, never a result.
    struct Failing(Value);
    impl Transport for Failing {
        fn call(&self, frame: Value, _: Duration) -> anyhow::Result<Value> {
            Ok(json!({"jsonrpc":"2.0","id":frame["id"],"error":self.0}))
        }
    }
    let failed = |error: Value| {
        run(
            &Failing(error),
            call("click", json!({})),
            input(),
            ALONE,
            &mut None,
        )
        .unwrap()
    };
    let result = failed(json!({"code":-32602,"message":"bad pid"}));
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["code"], "driver_error");
    // The guest agent's own failures keep their code (QEMU port errors).
    let result = failed(
        json!({"code":-32000,"message":"deadline exceeded","data":{"veynode_code":"deadline_exceeded"}}),
    );
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["code"], "deadline_exceeded");
}

#[test]
fn deadlines_follow_the_call() {
    let secs = |tool: &str, args: Value| deadline(tool, &args).unwrap().as_secs();
    assert_eq!(secs("click", json!({})), 30);
    assert_eq!(secs("verify_state", json!({"timeout_ms":10000})), 30);
    assert_eq!(secs("verify_state", json!({"timeout_ms":0})), 30);
    assert_eq!(secs("run_script", json!({"timeout_ms":120000})), 130);
    // 5 s observe + 3 × (2 s action + 1 s pause + 3 s target + 4 s expect) + 10 s margin = 45 s.
    let steps = json!({"delay_ms":1000,"observe":{},"steps":[
        {"tool":"click","args":{},"expect":[{"name":"OK","timeout_ms":4000}]},
        {"tool":"click","args":{},"expect":[{"name":"OK","timeout_ms":4000}]},
        {"tool":"click","args":{},"expect":{"name":"OK","timeout_ms":4000}},
    ]});
    assert_eq!(secs("run_steps", steps), 45);
    // Long foreground text gets time to type.
    assert_eq!(secs("type_text", json!({"text":"x".repeat(1000)})), 130);

    // 64 steps each waiting 10 s for their target and 4 × 10 s expects: past the 10 min cap.
    let step = json!({"tool":"click","args":{},"timeout_ms":10000,"wait_for":{"timeout_ms":10000},"expect":[{"timeout_ms":10000},{"timeout_ms":10000},{"timeout_ms":10000},{"timeout_ms":10000}]});
    let huge = json!({"steps":vec![step; 64]});
    assert!(deadline("run_steps", &huge).is_err());
    let driver = FakeDriver::new(ok());
    let reply = exchange(&driver, call("run_steps", huge), input());
    assert_eq!(reply["structuredContent"]["code"], "deadline_exceeds_limit");
    assert!(driver.sent().is_empty());

    // The transport gets the derived deadline.
    exchange(
        &driver,
        call("verify_state", json!({"timeout_ms":10000})),
        input(),
    );
    assert_eq!(driver.sent()[0].1, Duration::from_secs(30));
}

#[test]
fn oversized_requests_are_refused_in_the_drivers_shape() {
    let text = "x".repeat(cua_bridge::MAX_FRAME);
    match prepare(
        call("set_value", json!({"value":text})),
        input(),
        ALONE,
        None,
    ) {
        Prepared::Reply(reply) => {
            assert_eq!(reply["structuredContent"]["code"], "request_too_large")
        }
        Prepared::Send(_) => panic!("a 1 MiB value must not be sent"),
    }
}

#[test]
fn the_per_vm_endpoint_never_reaches_another_vm() {
    let permitted = vm_control::tools_for(&vm_control::McpOptions {
        lifecycle: true,
        ..Default::default()
    });
    let tools = vm_level_tools(&permitted, "c-abc");
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for name in [
        "container_checkpoint",
        "container_rollback",
        "vm_snapshot_list",
    ] {
        assert!(names.contains(&name), "{name}");
    }
    for tool in &tools {
        let properties = &tool["inputSchema"]["properties"];
        assert!(properties.get("container").is_none() && properties.get("id").is_none());
    }
    // A VM named in the arguments is replaced by the endpoint's own.
    match endpoint_call(
        "container_rollback",
        json!({"container":"other","name":"cp","confirm":"other"}),
        "c-abc",
        Scopes::default(),
        &tools,
    ) {
        EndpointCall::Vm(name, args) => {
            assert_eq!(name, "container_rollback");
            assert_eq!(args["container"], "abc");
        }
        other => panic!("{other:?}"),
    }
    match endpoint_call(
        "vm_snapshot_list",
        json!({"id":"other"}),
        "c-abc",
        Scopes::default(),
        &tools,
    ) {
        EndpointCall::Vm(_, args) => assert_eq!(args["id"], "c-abc"),
        other => panic!("{other:?}"),
    }
    match endpoint_call(
        "click",
        json!({"id":"other","pid":1}),
        "c-abc",
        input(),
        &tools,
    ) {
        EndpointCall::Driver(rpc) => {
            assert_eq!(rpc["id"], "c-abc");
            assert_eq!(rpc["tool"], "click");
            assert_eq!(rpc["scopes"], json!(["observe", "input"]));
        }
        other => panic!("{other:?}"),
    }
    // Without lifecycle the checkpoint tools are not offered, and the name goes to the
    // bridge, which refuses it as no driver tool.
    let observe = vm_level_tools(&vm_control::tools_for(&Default::default()), "c-abc");
    assert!(!observe.iter().any(|t| t["name"] == "container_rollback"));
    match endpoint_call(
        "container_rollback",
        json!({}),
        "c-abc",
        Scopes::default(),
        &observe,
    ) {
        EndpointCall::Driver(rpc) => {
            let request = Request::from_args(&rpc).unwrap();
            let Prepared::Reply(reply) = prepare(request, Scopes::default(), ALONE, None) else {
                panic!("must not be sent")
            };
            assert_eq!(reply["structuredContent"]["code"], "tool_not_exposed");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_cua_group_holds_every_curated_tool_and_follows_scopes() {
    let everything = vm_control::tools_for(&vm_control::McpOptions {
        input: true,
        browser: true,
        clipboard: true,
        execute: true,
        ..Default::default()
    });
    let cua: Vec<_> = everything
        .iter()
        .filter(|t| t["name"].as_str().unwrap().starts_with(cua_bridge::PREFIX))
        .collect();
    assert!(cua.len() >= 20, "{}", cua.len());
    for tool in &cua {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(vm_control::tool_groups::group(name), Some("cua"));
        assert!(
            serde_json::to_string(tool).unwrap().len() < 2048,
            "{name} schema stays short"
        );
    }
    let observe = vm_control::tools_for(&Default::default());
    assert!(!observe.iter().any(|t| t["name"] == "cua_click"));
    assert!(!observe.iter().any(|t| t["name"] == "cua_clipboard_read"));
    assert!(observe.iter().any(|t| t["name"] == "cua_get_window_state"));
    let rpc = cua_bridge::curated_call(
        "cua_click",
        json!({"vm":"c-abc","element_token":"s1:2"}),
        input(),
    )
    .unwrap();
    assert_eq!(
        rpc,
        json!({"id":"c-abc","tool":"click","arguments":{"element_token":"s1:2"},"scopes":["observe","input"]})
    );
}
