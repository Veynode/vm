//! The host half of the cua-driver bridge (docs/computer-use.md, phase 1).
//!
//! cua-driver runs in the Guest as an invisible stdio MCP child of the guest agent, which
//! owns its `initialize`. The host sends only `tools/list` and `tools/call` frames. This
//! module decides, before a frame leaves the host:
//! - whether the session's scopes allow the tool (the "Scope mapping" table; unknown and
//!   never-exposed tools are refused, so a newer driver adds nothing on its own);
//! - the per-call deadline;
//! - the bridge policies from phase 0 (non-ASCII text, Linux browser clicks).
//!
//! and, after the reply, filters `list_apps`. The driver's results otherwise pass through
//! unchanged: `effect:"unverifiable"`, `unknown` and refusals are never turned into success.
//! Bridge refusals use the driver's own error shape (`isError` plus `structuredContent.code`).
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

/// The longest call the bridge sends; the Guest transports cap `deadline_ms` at the same value.
pub const MAX_DEADLINE: Duration = Duration::from_secs(600);
/// A call that names no wait of its own (a foreground timeout took 12.5 s in phase 0).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);
/// Added to every derived wait for the driver's own work and the transport.
const MARGIN: Duration = Duration::from_secs(10);
/// Request and reply frame bound of both Guest transports.
pub const MAX_FRAME: usize = 1024 * 1024;
/// Prefix of the curated tools on the main MCP (`cua` tool group).
pub const PREFIX: &str = "cua_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Observe,
    Input,
    Browser,
    Clipboard,
    Execute,
}
impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Input => "input",
            Self::Browser => "browser",
            Self::Clipboard => "clipboard",
            Self::Execute => "execute",
        }
    }
}

/// Never sent to the driver and never listed: they change the driver itself or download code.
pub const NEVER_EXPOSED: [&str; 4] = [
    "install_extension",
    "check_for_update",
    "install_ffmpeg",
    "set_config",
];

/// The scope a driver tool needs, or None for a never-exposed or unknown tool.
/// The first rows are the docs' table; the rest are the driver 0.38 tools the table
/// leaves implicit, placed by what they change.
pub fn scope(tool: &str) -> Option<Scope> {
    if NEVER_EXPOSED.contains(&tool) {
        return None;
    }
    Some(match tool {
        "list_apps"
        | "list_windows"
        | "get_window_state"
        | "get_accessibility_tree"
        | "get_desktop_state"
        | "zoom"
        | "verify_state"
        | "get_browser_state" => Scope::Observe,
        // Reads of the driver's own state and its bookkeeping sessions.
        "get_screen_size"
        | "get_cursor_position"
        | "get_agent_cursor_state"
        | "check_permissions"
        | "health_report"
        | "get_config"
        | "get_guide"
        | "parse_visual_regions"
        | "get_recording_state"
        | "start_session"
        | "escalate_session"
        | "get_session"
        | "list_sessions"
        | "get_session_state"
        | "end_session" => Scope::Observe,
        "click"
        | "double_click"
        | "right_click"
        | "drag"
        | "mouse_button_down"
        | "mouse_drag"
        | "mouse_button_up"
        | "parallel_mouse_drag"
        | "type_text"
        | "press_key"
        | "hotkey"
        | "scroll"
        | "move_cursor"
        | "set_value"
        | "invoke_menu"
        | "set_window_frame"
        | "bring_to_front"
        | "run_steps"
        | "set_agent_cursor_enabled"
        | "set_agent_cursor_motion"
        | "set_agent_cursor_theme" => Scope::Input,
        "browser_prepare"
        | "browser_navigate"
        | "browser_click"
        | "browser_type"
        | "browser_select"
        | "browser_dialog"
        | "browser_set_input_files"
        | "browser_download"
        | "browser_pointer" => Scope::Browser,
        "clipboard_read" | "clipboard_write" => Scope::Clipboard,
        // `page` runs JavaScript in a page; recordings write Guest files and replays act.
        "run_script" | "launch_app" | "kill_app" | "page" | "start_recording"
        | "stop_recording" | "replay_trajectory" => Scope::Execute,
        _ => return None,
    })
}

/// What a session may ask of the driver. Observe is always granted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scopes {
    pub input: bool,
    pub browser: bool,
    pub clipboard: bool,
    pub execute: bool,
}
impl Scopes {
    pub fn all() -> Self {
        Self {
            input: true,
            browser: true,
            clipboard: true,
            execute: true,
        }
    }
    pub fn allows(&self, scope: Scope) -> bool {
        match scope {
            Scope::Observe => true,
            Scope::Input => self.input,
            Scope::Browser => self.browser,
            Scope::Clipboard => self.clipboard,
            Scope::Execute => self.execute,
        }
    }
    /// The wire form sent with `driver_rpc`: the granted scope names.
    pub fn to_value(self) -> Value {
        let mut names = vec!["observe"];
        for (granted, scope) in [
            (self.input, Scope::Input),
            (self.browser, Scope::Browser),
            (self.clipboard, Scope::Clipboard),
            (self.execute, Scope::Execute),
        ] {
            if granted {
                names.push(scope.name());
            }
        }
        json!(names)
    }
    /// Reads the wire form; anything missing is not granted.
    pub fn from_value(value: &Value) -> Self {
        let has = |name: &str| {
            value
                .as_array()
                .is_some_and(|names| names.iter().any(|n| n == name))
        };
        Self {
            input: has("input"),
            browser: has("browser"),
            clipboard: has("clipboard"),
            execute: has("execute"),
        }
    }
}

/// What the bridge knows about the Guest when it prepares a call.
#[derive(Clone, Copy, Debug)]
pub struct Desktop {
    /// A person is watching or driving this desktop (native console or browser session).
    /// Agent-side foreground delivery would take the window from them, so the policies
    /// that choose foreground apply only while this is false. The daemon sets it while a
    /// viewer is open (`control_lease`).
    pub person_attached: bool,
}

/// One request to the driver.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    List,
    Call { tool: String, arguments: Value },
}
impl Request {
    /// The `driver_rpc` daemon arguments: `{"list":true}` or `{"tool","arguments"}`.
    pub fn from_args(args: &Value) -> Result<Self> {
        if args["list"] == true {
            return Ok(Self::List);
        }
        let tool = args["tool"]
            .as_str()
            .context("driver_rpc needs tool or list")?;
        let arguments = args.get("arguments").cloned().unwrap_or(json!({}));
        if !arguments.is_object() {
            bail!("driver_rpc arguments must be an object")
        }
        Ok(Self::Call {
            tool: tool.into(),
            arguments,
        })
    }
}

/// A prepared request: answered here, or a frame to send.
#[derive(Debug)]
pub enum Prepared {
    /// A refusal in the driver's error shape, or the cached tool list.
    Reply(Value),
    Send(Outgoing),
}
#[derive(Debug)]
pub struct Outgoing {
    /// The JSON-RPC 2.0 request frame, without its id (the transport assigns one).
    pub frame: Value,
    pub deadline: Duration,
    pub finish: Finish,
}
/// What to do with the driver's reply.
#[derive(Debug, Clone)]
pub struct Finish {
    pub list: bool,
    tool: String,
    scopes: Scopes,
    policies: Vec<&'static str>,
}

/// A bridge refusal, shaped like the driver's own errors.
pub fn refusal(code: &str, detail: &str) -> Value {
    json!({"content":[{"type":"text","text":format!("refused ({code}): {detail}")}],"isError":true,"structuredContent":{"code":code,"detail":detail,"effect":"refused","ok":false}})
}

/// Checks scope and size, applies the policies and derives the deadline.
/// `cached` is this VM's last `tools/list` result, when there is one.
pub fn prepare(
    request: Request,
    scopes: Scopes,
    desktop: Desktop,
    cached: Option<&Value>,
) -> Prepared {
    let (tool, mut arguments) = match request {
        Request::List => {
            if let Some(list) = cached {
                return Prepared::Reply(json!({"tools":filter_tools(list, scopes)}));
            }
            return Prepared::Send(Outgoing {
                frame: json!({"jsonrpc":"2.0","method":"tools/list","params":{}}),
                deadline: DEFAULT_DEADLINE,
                finish: Finish {
                    list: true,
                    tool: String::new(),
                    scopes,
                    policies: vec![],
                },
            });
        }
        Request::Call { tool, arguments } => (tool, arguments),
    };
    if let Err(reply) = permitted(&tool, &arguments, scopes) {
        return Prepared::Reply(reply);
    }
    let policies = apply_policies(&tool, &mut arguments, desktop);
    let deadline = match deadline(&tool, &arguments) {
        Ok(deadline) => deadline,
        Err(detail) => return Prepared::Reply(refusal("deadline_exceeds_limit", &detail)),
    };
    let frame =
        json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":tool,"arguments":arguments}});
    // Room for the transport's id and the worker command around the frame, whose lines
    // are 1 MiB at most too.
    if serde_json::to_vec(&frame).map_or(usize::MAX, |b| b.len()) > MAX_FRAME - 4096 {
        return Prepared::Reply(refusal(
            "request_too_large",
            "the call exceeds the bridge's 1 MiB frame limit",
        ));
    }
    Prepared::Send(Outgoing {
        frame,
        deadline,
        finish: Finish {
            list: false,
            tool,
            scopes,
            policies,
        },
    })
}

/// Scope check of a tool and, for `run_steps`, of every step's tool.
fn permitted(tool: &str, arguments: &Value, scopes: Scopes) -> std::result::Result<(), Value> {
    let Some(needed) = scope(tool) else {
        return Err(refusal(
            "tool_not_exposed",
            &format!("{tool} is not available through VeynodeVM"),
        ));
    };
    if !scopes.allows(needed) {
        return Err(refusal(
            "scope_not_granted",
            &format!("{tool} needs the {} scope", needed.name()),
        ));
    }
    if tool == "run_steps" {
        for (index, step) in arguments["steps"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            if let Some(inner) = step_tool(step) {
                let allowed = scope(inner).is_some_and(|s| scopes.allows(s));
                if !allowed {
                    return Err(refusal(
                        "scope_not_granted",
                        &format!("run_steps step {}: {inner} is not permitted", index + 1),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// A step's tool: `{tool, args}` or `{<tool>: args}`.
fn step_tool(step: &Value) -> Option<&str> {
    if let Some(tool) = step["tool"].as_str() {
        return Some(tool);
    }
    step.as_object()?
        .keys()
        .map(String::as_str)
        .find(|key| !["args", "expect", "wait_for", "timeout_ms"].contains(key))
}
fn step_args_mut<'a>(step: &'a mut Value, tool: &str) -> Option<&'a mut Map<String, Value>> {
    let key = if step.get("tool").is_some() {
        "args"
    } else {
        tool
    };
    let object = step.as_object_mut()?;
    object
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
}

/// The phase 0 policies. Returns the names of those that changed the call.
fn apply_policies(tool: &str, arguments: &mut Value, desktop: Desktop) -> Vec<&'static str> {
    let mut applied = vec![];
    if desktop.person_attached {
        return applied;
    }
    match tool {
        "type_text" => {
            if let Some(args) = arguments.as_object_mut() {
                if text_policy(args, false) {
                    applied.push("type_text_foreground");
                }
            }
        }
        "browser_click" => {
            // Chromium's trusted background route is refused on Linux by design.
            if let Some(args) = arguments.as_object_mut() {
                if !args.contains_key("delivery_mode") && !args.contains_key("input_route") {
                    args.insert("delivery_mode".into(), json!("foreground"));
                    applied.push("browser_click_foreground");
                }
            }
        }
        "run_steps" => {
            let inherited = arguments.get("delivery_mode").is_some();
            if let Some(steps) = arguments["steps"].as_array_mut() {
                for step in steps {
                    if step_tool(step) != Some("type_text") {
                        continue;
                    }
                    if let Some(args) = step_args_mut(step, "type_text") {
                        if text_policy(args, inherited)
                            && !applied.contains(&"type_text_foreground")
                        {
                            applied.push("type_text_foreground");
                        }
                    }
                }
            }
        }
        _ => {}
    }
    applied
}

/// Non-ASCII text without a chosen delivery goes foreground: the background AT-SPI insert
/// truncates multibyte text (upstream #4754). Foreground rather than `set_value`, because
/// `set_value` replaces the whole value where `type_text` inserts at the caret.
fn text_policy(args: &mut Map<String, Value>, inherited: bool) -> bool {
    let non_ascii = args
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_ascii());
    if non_ascii && !inherited && !args.contains_key("delivery_mode") {
        args.insert("delivery_mode".into(), json!("foreground"));
        return true;
    }
    false
}

fn millis(value: &Value, default: u64) -> u64 {
    value.as_u64().unwrap_or(default)
}

/// The deadline of one call, or why it would exceed `MAX_DEADLINE`.
pub fn deadline(tool: &str, arguments: &Value) -> std::result::Result<Duration, String> {
    // Foreground typing measured about 80 ms per character in phase 0.
    let typing = |args: &Value| {
        args["text"]
            .as_str()
            .map_or(0, |t| t.chars().count() as u64 * 100)
    };
    let wait_ms = match tool {
        "verify_state" => millis(&arguments["timeout_ms"], 5000),
        "run_script" => millis(&arguments["timeout_ms"], 30000),
        "wait_for" => millis(&arguments["timeout_ms"], 5000),
        "type_text" | "set_value" => {
            DEFAULT_DEADLINE.as_millis() as u64 - MARGIN.as_millis() as u64 + typing(arguments)
        }
        "run_steps" => {
            let delay = millis(&arguments["delay_ms"], 0);
            let mut total = if arguments.get("observe").is_some() {
                5000
            } else {
                0
            };
            for step in arguments["steps"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                // The action itself, the pause after it, and how long its target may take to appear.
                total += 2000 + delay + millis(&step["timeout_ms"], 3000);
                if let Some(wait) = step.get("wait_for") {
                    total += millis(&wait["timeout_ms"], 5000);
                }
                let expects = match &step["expect"] {
                    Value::Array(checks) => checks.iter().collect::<Vec<_>>(),
                    Value::Object(_) => vec![&step["expect"]],
                    _ => vec![],
                };
                for check in expects {
                    total += millis(&check["timeout_ms"], 2000);
                }
                let args = step_tool(step).map_or(&Value::Null, |tool| {
                    if step.get("tool").is_some() {
                        &step["args"]
                    } else {
                        &step[tool]
                    }
                });
                total += typing(args);
            }
            total
        }
        _ => return Ok(DEFAULT_DEADLINE),
    };
    let deadline = Duration::from_millis(wait_ms) + MARGIN;
    if deadline > MAX_DEADLINE {
        return Err(format!(
            "{tool} could take {} s, more than the bridge's {} s limit; split it into smaller calls",
            deadline.as_secs(),
            MAX_DEADLINE.as_secs()
        ));
    }
    Ok(deadline.max(DEFAULT_DEADLINE))
}

/// The driver's tools this session may see: never-exposed, unknown and out-of-scope tools removed.
pub fn filter_tools(list: &Value, scopes: Scopes) -> Vec<Value> {
    list["tools"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .and_then(scope)
                .is_some_and(|s| scopes.allows(s))
        })
        .cloned()
        .collect()
}

impl Finish {
    /// Turns the driver's JSON-RPC reply frame into the result: the tool list
    /// (`{"tools"}`, filtered) or the driver's `CallToolResult`, unchanged but for `list_apps`.
    pub fn result(&self, frame: &Value) -> Result<Value> {
        if let Some(error) = frame.get("error") {
            let message = error["message"].as_str().unwrap_or("driver error");
            if self.list {
                bail!("cua-driver tools/list failed: {message}")
            }
            // The guest agent's own failures (deadline_exceeded, driver_exited, unsupported, ...)
            // carry `data.veynode_code`; a driver protocol error has none.
            let code = error["data"]["veynode_code"]
                .as_str()
                .unwrap_or("driver_error");
            return Ok(
                json!({"content":[{"type":"text","text":format!("cua-driver: {message}")}],"isError":true,"structuredContent":{"code":code,"detail":message,"ok":false}}),
            );
        }
        let mut result = frame
            .get("result")
            .cloned()
            .context("cua-driver reply has neither result nor error")?;
        if self.list {
            return Ok(json!({"tools":filter_tools(&result, self.scopes)}));
        }
        if self.tool == "list_apps" {
            filter_apps(&mut result);
        }
        if !self.policies.is_empty() {
            result["_meta"]["veynode/bridge_policy"] = json!(self.policies);
        }
        Ok(result)
    }
}

/// Keeps the desktop's applications: running ones with a window or a desktop entry, and
/// installed ones not running. The driver lists every Guest process (kernel threads, the
/// agent, the driver itself), because it shares the agent's PID namespace.
pub fn filter_apps(result: &mut Value) {
    let Some(apps) = result["structuredContent"]["apps"].as_array_mut() else {
        return;
    };
    let before = apps.len();
    apps.retain(|app| {
        app["running"] == false
            || app["kind"] == "desktop"
            || app["windows"].as_array().is_some_and(|w| !w.is_empty())
    });
    let running = apps.iter().filter(|a| a["running"] == true).count();
    let mut text = format!(
        "Found {} app(s): {running} running, {} installed-not-running ({} other Guest processes hidden).",
        apps.len(),
        apps.len() - running,
        before - apps.len()
    );
    for app in apps.iter() {
        let name = app["name"].as_str().unwrap_or("?");
        match app["pid"].as_u64().filter(|_| app["running"] == true) {
            Some(pid) => text.push_str(&format!("\n- {name} (pid {pid})")),
            None => text.push_str(&format!("\n- {name} (not running)")),
        }
    }
    result["content"] = json!([{"type":"text","text":text}]);
}

/// How a Guest's driver frames travel when the bridge itself waits for the reply. The
/// worker transports (Apple vsock `driver_rpc`, the QEMU `veynode.driver` port) don't use
/// it: the daemon writes their frame under its manager lock and waits on the worker's
/// pending table. The Windows and macOS SSH bridges (phase 2) implement this.
pub trait Transport: Send + Sync {
    /// Sends one JSON-RPC request frame and returns the driver's reply frame within `deadline`.
    fn call(&self, frame: Value, deadline: Duration) -> Result<Value>;
}

/// prepare, exchange and finish over one transport. `cache` is read and filled for `tools/list`.
pub fn run(
    transport: &dyn Transport,
    request: Request,
    scopes: Scopes,
    desktop: Desktop,
    cache: &mut Option<Value>,
) -> Result<Value> {
    match prepare(request, scopes, desktop, cache.as_ref()) {
        Prepared::Reply(reply) => Ok(reply),
        Prepared::Send(outgoing) => {
            let mut frame = outgoing.frame;
            frame["id"] = json!(1);
            let reply = transport.call(frame, outgoing.deadline)?;
            if outgoing.finish.list {
                if let Some(result) = reply.get("result") {
                    *cache = Some(result.clone());
                }
            }
            outgoing.finish.result(&reply)
        }
    }
}

/// The curated tools of the main MCP's `cua` group: the driver's everyday tools with
/// short schemas and a `vm` argument. Other arguments pass through to the driver, which
/// validates them; the per-VM endpoint serves the driver's full schemas.
pub fn curated_tools(scopes: Scopes) -> Vec<Value> {
    let window = json!({"pid":{"type":"integer"},"window_id":{"type":"integer"}});
    let target = json!({"pid":{"type":"integer"},"window_id":{"type":"integer"},"element_token":{"type":"string","description":"Row [N] of get_window_state is <snapshot_id>:N"},"x":{"type":"number"},"y":{"type":"number"},"delivery_mode":{"type":"string","enum":["background","foreground"]}});
    let browser = json!({"session":{"type":"string"},"target_id":{"type":"string"},"tab_id":{"type":"string"}});
    let rows: Vec<(&str, &str, Value, &[&str])> = vec![
        ("list_apps", "Desktop applications (running, with a window or desktop entry, and installed).", json!({}), &[]),
        ("list_windows", "Windows with pid, window_id, title, bounds and z-order.", json!({"app":{"type":"string"},"pid":{"type":"integer"},"on_screen_only":{"type":"boolean"}}), &[]),
        ("get_window_state", "A window's accessibility tree as compact markdown rows with element tokens; since:\"latest\" returns only what changed; query narrows it.", merge(&window, json!({"query":{"type":"string"},"since":{"type":["string","null"]},"include_screenshot":{"type":"boolean"},"max_elements":{"type":"integer"}})), &["pid","window_id"]),
        ("get_desktop_state", "Screenshot of the whole desktop plus its windows.", json!({"max_image_dimension":{"type":"integer"}}), &[]),
        ("zoom", "A screenshot of one region of a window.", merge(&window, json!({"x1":{"type":"number"},"y1":{"type":"number"},"x2":{"type":"number"},"y2":{"type":"number"}})), &["pid","window_id","x1","y1","x2","y2"]),
        ("verify_state", "Check up to 8 predicates on one window, waiting up to timeout_ms (max 10000). satisfied, unsatisfied or unknown; unknown is not success.", merge(&window, json!({"expect":{"type":"array","items":{"type":"object"},"minItems":1,"maxItems":8},"timeout_ms":{"type":"integer","maximum":10000}})), &["pid","window_id","expect"]),
        ("get_browser_state", "A Chromium page bound to its window: DOM refs for the browser_* tools.", merge(&window, json!({"snapshot_format":{"type":"string"},"query":{"type":"string"}})), &[]),
        ("click", "Click an element by token (accessibility) or a point in a window. effect:\"unverifiable\" means read again before retrying.", merge(&target, json!({"count":{"type":"integer"},"button":{"type":"string"}})), &[]),
        ("type_text", "Type text into an element or the focused field. Non-ASCII text goes foreground unless delivery_mode is given.", merge(&target, json!({"text":{"type":"string"}})), &["text"]),
        ("press_key", "Press one key, optionally with modifiers.", merge(&target, json!({"key":{"type":"string"},"modifiers":{"type":"array","items":{"type":"string"}}})), &["key"]),
        ("hotkey", "Press a key combination such as [\"ctrl\",\"s\"].", merge(&target, json!({"keys":{"type":"array","items":{"type":"string"}}})), &["keys"]),
        ("scroll", "Scroll an element or a point.", merge(&target, json!({"direction":{"type":"string","enum":["up","down","left","right"]},"amount":{"type":"integer"}})), &["direction"]),
        ("drag", "Drag from one point to another in a window.", merge(&window, json!({"from_x":{"type":"number"},"from_y":{"type":"number"},"target":{"type":"object"}})), &[]),
        ("set_value", "Replace an element's whole value.", merge(&window, json!({"element_token":{"type":"string"},"value":{"type":"string"}})), &["element_token","value"]),
        ("invoke_menu", "Open a menu path such as [\"File\",\"New\"].", merge(&window, json!({"path":{"type":"array","items":{"type":"string"}}})), &["pid","path"]),
        ("set_window_frame", "Move or resize a window.", merge(&window, json!({"x":{"type":"number"},"y":{"type":"number"},"width":{"type":"number"},"height":{"type":"number"}})), &["pid","window_id"]),
        ("run_steps", "Run up to 64 action steps ({tool, args, expect?, wait_for?}) in one call, stopping at the first failure; observe:{} returns what changed.", merge(&window, json!({"steps":{"type":"array","items":{"type":"object"},"minItems":1,"maxItems":64},"observe":{"type":"object"},"delay_ms":{"type":"integer","maximum":2000},"delivery_mode":{"type":"string","enum":["background","foreground"]}})), &["steps"]),
        ("browser_prepare", "Make Chromium drivable. Attaching to an existing profile needs a grant the bridge does not give; allow_launch with profile {mode:\"isolated_new\"} launches a driver-owned browser that ends with the session.", merge(&window, json!({"allow_launch":{"type":"boolean"},"profile":{"type":"object","properties":{"mode":{"type":"string"}}},"strategy":{"type":"object"},"session":{"type":"string"}})), &[]),
        ("browser_navigate", "Open a URL in a prepared tab.", merge(&browser, json!({"url":{"type":"string"}})), &["url"]),
        ("browser_click", "Click a DOM ref; on Linux it goes foreground unless delivery_mode or input_route is given.", merge(&browser, json!({"ref":{"type":"string"},"delivery_mode":{"type":"string","enum":["background","foreground"]},"input_route":{"type":"string"}})), &[]),
        ("browser_type", "Type into a DOM ref (exact for any text).", merge(&browser, json!({"ref":{"type":"string"},"text":{"type":"string"},"replace":{"type":"boolean"}})), &["text"]),
        ("clipboard_read", "Read the Guest clipboard.", json!({}), &[]),
        ("clipboard_write", "Write text to the Guest clipboard.", json!({"text":{"type":"string"}}), &["text"]),
        ("launch_app", "Start an application by name, launch path (Linux, Windows) or bundle id (macOS).", json!({"name":{"type":"string"},"launch_path":{"type":"string"},"bundle_id":{"type":"string"},"urls":{"type":"array","items":{"type":"string"}}}), &[]),
    ];
    rows.into_iter()
        .filter(|(tool, ..)| scope(tool).is_some_and(|s| scopes.allows(s)))
        .map(|(tool, about, properties, required)| {
            let mut properties = properties;
            properties["vm"] = json!({"type":"string","description":"VM id (container_list vm_id)"});
            let mut required: Vec<&str> = required.to_vec();
            required.insert(0, "vm");
            json!({"name":format!("{PREFIX}{tool}"),"description":format!("cua-driver {tool} in a VM desktop (Linux, Windows or macOS container): {about}"),"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":true}})
        })
        .collect()
}
fn merge(base: &Value, extra: Value) -> Value {
    let mut merged = base.clone();
    for (key, value) in extra.as_object().unwrap() {
        merged[key] = value.clone();
    }
    merged
}

/// Whether a `driver_rpc` outcome means this Guest has no driver to ask (none in the
/// image, a Guest that cannot run one, or one that will not start), so the host may
/// answer an observation itself. The guest agent reports these as tool errors with
/// `structuredContent.code`; the daemon and the SSH links as errors.
pub fn helperless(outcome: &Result<Value>) -> bool {
    const CODES: [&str; 3] = ["unsupported", "driver_unavailable", "driver_exited"];
    match outcome {
        Ok(result) => {
            result["isError"] == true
                && result["structuredContent"]["code"]
                    .as_str()
                    .is_some_and(|c| CODES.contains(&c))
        }
        Err(error) => {
            let text = error.to_string();
            CODES.iter().any(|code| text.starts_with(code))
        }
    }
}

/// The `driver_rpc` daemon arguments of a main-MCP `cua_*` call.
pub fn curated_call(name: &str, mut args: Value, scopes: Scopes) -> Result<Value> {
    let tool = name.strip_prefix(PREFIX).context("not a cua tool")?;
    let vm = args
        .as_object_mut()
        .and_then(|a| a.remove("vm"))
        .context("missing vm")?;
    Ok(json!({"id":vm,"tool":tool,"arguments":args,"scopes":scopes.to_value()}))
}

/// The VM-level tools the per-VM endpoint serves beside the driver's, with the VM bound.
const VM_LEVEL: [&str; 7] = [
    "container_checkpoint",
    "container_checkpoint_list",
    "container_rollback",
    "vm_snapshot_list",
    "vm_snapshot_status",
    "vm_snapshot_create",
    "vm_snapshot_restore",
];

/// The argument that names the VM in a VM-level tool, and its value for `vm`.
/// Container tools exist only for containers (VM id `c-<container id>`).
fn binding(tool: &str, vm: &str) -> Option<(&'static str, String)> {
    if !VM_LEVEL.contains(&tool) {
        return None;
    }
    if tool.starts_with("container_") {
        vm.strip_prefix("c-").map(|c| ("container", c.to_string()))
    } else {
        Some(("id", vm.to_string()))
    }
}

/// `permitted` (the main MCP's tools for this session) narrowed to the VM-level tools of
/// `vm`, without the argument that names the VM.
pub fn vm_level_tools(permitted: &[Value], vm: &str) -> Vec<Value> {
    permitted
        .iter()
        .filter_map(|tool| {
            let (key, value) = binding(tool["name"].as_str()?, vm)?;
            let mut tool = tool.clone();
            let schema = &mut tool["inputSchema"];
            if let Some(properties) = schema["properties"].as_object_mut() {
                properties.remove(key);
            }
            if let Some(required) = schema["required"].as_array_mut() {
                required.retain(|r| r != key);
            }
            if tool["name"] == "container_rollback" {
                let about = tool["description"].as_str().unwrap_or_default();
                tool["description"] = json!(format!(
                    "{about} On this endpoint the container is {value:?}."
                ));
            }
            Some(tool)
        })
        .collect()
}

/// How the per-VM endpoint answers one `tools/call`.
#[derive(Debug, PartialEq)]
pub enum EndpointCall {
    /// A VM-level tool, with the VM's argument filled in.
    Vm(String, Value),
    /// The `driver_rpc` daemon arguments; the bridge refuses tools out of scope.
    Driver(Value),
}
/// Routes a per-VM endpoint call; `vm_tools` is `vm_level_tools` of the session. Any VM
/// named in the arguments is replaced by the endpoint's own, so the endpoint never reaches
/// another VM. Every other name goes to the bridge, which refuses what is not a permitted
/// driver tool before the driver sees it.
pub fn endpoint_call(
    name: &str,
    mut args: Value,
    vm: &str,
    scopes: Scopes,
    vm_tools: &[Value],
) -> EndpointCall {
    if let Some((key, value)) =
        binding(name, vm).filter(|_| vm_tools.iter().any(|t| t["name"] == name))
    {
        args[key] = json!(value);
        return EndpointCall::Vm(name.into(), args);
    }
    EndpointCall::Driver(json!({"id":vm,"tool":name,"arguments":args,"scopes":scopes.to_value()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_scope_row_maps() {
        for tool in [
            "list_apps",
            "get_accessibility_tree",
            "browser_navigate",
            "clipboard_write",
            "run_script",
        ] {
            assert!(scope(tool).is_some(), "{tool}");
        }
        for tool in NEVER_EXPOSED {
            assert_eq!(scope(tool), None);
        }
        assert_eq!(scope("some_future_tool"), None);
    }

    #[test]
    fn helperless_means_no_driver_not_a_failed_call() {
        let tool_error =
            |code: &str| Ok(json!({"isError":true,"structuredContent":{"code":code,"detail":"x"}}));
        // As seen live: the Guest agent reports a missing binary as driver_unavailable.
        assert!(helperless(&tool_error("driver_unavailable")));
        assert!(helperless(&tool_error("driver_exited")));
        assert!(helperless(&Err(anyhow::anyhow!(
            "unsupported: no cua-driver port"
        ))));
        assert!(!helperless(&tool_error("stale_element_token")));
        assert!(!helperless(&Ok(json!({"isError":false}))));
        assert!(!helperless(&Err(anyhow::anyhow!("VM not found"))));
    }

    #[test]
    fn curated_tools_are_mapped_and_need_vm() {
        let all = curated_tools(Scopes::all());
        assert!(all.len() >= 20);
        for tool in &all {
            let name = tool["name"].as_str().unwrap();
            assert!(
                scope(name.strip_prefix(PREFIX).unwrap()).is_some(),
                "{name}"
            );
            assert_eq!(tool["inputSchema"]["required"][0], "vm");
        }
        let observe = curated_tools(Scopes::default());
        assert!(observe.iter().all(|t| {
            scope(t["name"].as_str().unwrap().strip_prefix(PREFIX).unwrap()) == Some(Scope::Observe)
        }));
    }
}
