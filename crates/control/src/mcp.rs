//! The MCP stdio server over the daemon's socket.
use super::*;

pub use veynodevm_mcp::VERSION;
pub fn tools_list(lifecycle: bool, execute: bool) -> Value {
    tools_with_files(lifecycle, execute, false)
}
pub(crate) fn tools_with_files(lifecycle: bool, execute: bool, files: bool) -> Value {
    tools_with_input(lifecycle, execute, files, false)
}
pub(crate) fn tools_with_input(lifecycle: bool, execute: bool, files: bool, input: bool) -> Value {
    let id = json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false});
    let mut tools = vec![
        json!({"name":"vm_list","description":"List registered VMs and actual backend process state.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"vm_screenshot","description":"Capture the same console: by default at full source resolution (QEMU PNG, Apple Virtualization Linux desktop PNG (JPEG when large) or macOS Guest JPEG). max_width/max_height scale it down, region crops it and format/quality re-encode it on the host; the reply then gives image_scale and image_region (input x = image_region.x + image x / image_scale) and, for a whole-screen image, image_point_pixel_scale to pass with coordinate_space points. Returns actual MIME, frame id, dimensions (always the full frame's, for input), scale and capture timing. veynodevm macOS containers are captured through the Guest's Screen Sharing server (no Guest permission grants); other macOS Guests need the opted-in Guest service with existing Screen Recording permission, never requested automatically.","inputSchema":{"type":"object","properties":agent_view::screenshot_properties(),"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_screen_wait","description":"Wait until the screen changes, then optionally until it is still for settle_ms (e.g. after a click, until redrawing ends). Pass the frame_id you acted on so an earlier change counts. Returns changed, settled, timing and, on Apple Virtualization Linux desktops, changed regions in screenshot pixels (region limits the wait there); QEMU and macOS Guests detect whole-screen changes. Take a new screenshot before input.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"frame_id":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":screen_wait::MAX_TIMEOUT_MS,"default":10000},"settle_ms":{"type":"integer","minimum":0,"maximum":screen_wait::MAX_SETTLE_MS,"default":0},"region":{"type":"object","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["x","y","width","height"],"additionalProperties":false}},"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_screen_text","description":"Read screen text without an image: a screenshot plus host text recognition (Apple Vision), on every screenshot backend with nothing installed in the Guest. Returns lines in reading order with boxes and centers in screenshot pixels, plus frame_id, source_generation, width and height for the input tools. find returns each case-insensitive match with its box. fast trades accuracy for speed; languages are BCP 47 codes (default automatic).","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"find":{"type":"string","minLength":1,"maxLength":256},"fast":{"type":"boolean","default":false},"languages":{"type":"array","items":{"type":"string"},"maxItems":8}},"required":["id"],"additionalProperties":false}}),
        json!({"name":"vm_windows","description":"Read macOS Guest on-screen window metadata (--allow-observe, existing Screen Recording permission). No titles, focus, stacking order, Accessibility tree or input authority. Desktop-global points; not atomic with a screenshot. Linux and Windows Guests return unsupported, as do other Guest/backend combinations.","inputSchema":id}),
        json!({"name":"vm_guest_info","description":"Query the real Guest service: Apple host-only vsock or QEMU private virtio-serial (default observer-only; explicit v2 permits rootless cgroup execution). Reports actual capabilities; fails if unavailable.","inputSchema":id}),
        json!({"name":"vm_status","description":"Get VM configuration and process state; running does not assert guest health.","inputSchema":id}),
        json!({"name":"vm_capabilities","description":"Report implemented operations, explicitly unsupported integrations, and known backend limits of macOS Guests (known_limits).","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"vm_snapshot_list","description":"List named snapshot metadata for one VM. Capture integrity is separate from a successful restore. Hidden incomplete captures are never reported as complete.","inputSchema":id}),
        json!({"name":"vm_snapshot_status","description":"Query one snapshot operation by its returned operation_id. A timeout or client disconnect does not cancel an operation; inspect its terminal state before retrying.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"operation_id":{"type":"string"}},"required":["id","operation_id"],"additionalProperties":false}}),
    ];
    // After the screen tools, so observe-only listings keep their order.
    tools.splice(4..4, desktop_tools::tools(input));
    if lifecycle {
        tools.push(json!({"name":"vm_snapshot_create","description":"Create a named full-state Linux snapshot using the explicitly supported backend/device configuration. Returns an operation_id to poll. Never falls back to a disk-only clone. Unsupported configurations or unresolved Guest work are refused.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"kind":{"type":"string","enum":["full_state"]}},"required":["id","name","kind"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_restore","description":"Restore one named full-state Linux snapshot into a stopped VM. Requires explicit confirmation equal to the VM id, verifies compatibility/integrity, preserves the current disk, and loads a fresh private disk generation without cold booting. Returns an operation_id; poll its state before retrying.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"confirm":{"type":"string"}},"required":["id","name","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_delete","description":"Delete exactly one named snapshot. Requires explicit confirmation equal to its name. Does not delete the VM, original disk or other snapshots.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"confirm":{"type":"string"}},"required":["id","name","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_reconcile","description":"Reconcile an interrupted create or restore only with its durable retirement proof and no live disk or persistent-identity owner. Create requires an admission-time witness; restore locks both disk generations. Confirmation equals VM id. Preserves history and artifacts; never claims capture or restore success.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"operation_id":{"type":"string"},"confirm":{"type":"string"}},"required":["id","operation_id","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"vm_snapshot_resume_source","description":"Recover the same live Apple Linux source after an unsuccessful capture, without restoring a snapshot or changing disks. Requires confirmation equal to VM id, the original owned worker and a returned capture channel. Busy or unknown outcomes stay fenced; poll snapshot status before retrying. Successful recovery preserves the failed capture history.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"operation_id":{"type":"string"},"confirm":{"type":"string"}},"required":["id","operation_id","confirm"],"additionalProperties":false}}));
        for (name, description) in [
            ("vm_start", "Start a VM in its own native backend process."),
            (
                "vm_shutdown",
                "Request normal shutdown via backend power button (default) or opt-in installed guest service (mode guest). Request acceptance is not completion; poll status. Never implicitly force-stops.",
            ),
            ("vm_show_window", "Show native VM console; QEMU input requires a verified v2 helper and its bound private controller channel, and flows only while the window has focus."),
            (
                "vm_close_window",
                "Close native console while keeping VM running.",
            ),
            (
                "vm_sleep",
                "Free the VM's resources now, when you are done with it for a while: an Apple Linux VM with a fixed MAC and machine identifier is hibernated (memory saved to disk, worker stopped), any other is paused. Any later call on the VM wakes it first, so no explicit wake is needed. Refused while Guest jobs run.",
            ),
            (
                "vm_keep_awake",
                "Keep the VM from its idle pause or hibernation for seconds (0 ends it early, max 86400), e.g. while a build or server you started in the background runs; wakes the VM if it sleeps.",
            ),
        ] {
            let schema=if name=="vm_shutdown" {json!({"type":"object","properties":{"id":{"type":"string"},"mode":{"type":"string","enum":["backend","guest"],"default":"backend"}},"required":["id"],"additionalProperties":false})}else if name=="vm_keep_awake" {json!({"type":"object","properties":{"id":{"type":"string"},"seconds":{"type":"integer","minimum":0,"maximum":86400}},"required":["id","seconds"],"additionalProperties":false})}else{id.clone()};
            tools.push(json!({"name":name,"description":description,"inputSchema":schema}));
        }
        tools.push(json!({"name":"vm_force_stop","description":"Kill VM process. Can lose guest data. Requires explicit confirmation equal to VM id.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"confirm":{"type":"string"}},"required":["id","confirm"],"additionalProperties":false}}));
        let mut properties = json!({"backend":{"type":"string","enum":["apple-virtualization","qemu-hvf"],"default":"apple-virtualization"},"id":{"type":"string"},"guest":{"type":"string","enum":["linux","macos"]},"cpus":{"type":"integer","minimum":1,"maximum":128},"memory_mib":{"type":"integer","minimum":256,"maximum":1048576},"window":{"type":"boolean"}});
        properties["qemu_graphics_profile"] = json!({"type":"string","enum":["virgl","venus-moltenvk-experimental","virtio-2d"],"default":"virgl","description":"Linux QEMU graphics profile. virtio-2d is an explicit unaccelerated console, not a 3D fallback. Experimental Venus requires a trusted runtime pre-registered by the local daemon operator; this option grants no binary paths, environment overrides or Guest execution capability."});
        for p in ["width", "height"] {
            properties[p] = json!({"type":"integer","minimum":320,"maximum":8192,"description":"QEMU requires both initial width and height, even values, max3840x2160; Guest actual mode may differ. No dynamic resizing."});
        }
        for p in [
            "kernel",
            "initrd",
            "command_line",
            "disk",
            "restore_image",
            "machine_identity_dir",
            "serial_log",
            "shared_dir",
            "efi_vars",
            "cdrom",
        ] {
            properties[p] = json!({"type":"string"});
        }
        properties["boot"] = json!({"type":"string","enum":["kernel","uefi"],"default":"kernel","description":"uefi boots the Linux distribution's own bootloader from disk or cdrom (installer ISO) and requires efi_vars, a per-VM firmware variable store created on first start."});
        properties["qemu_monitor"] = json!({"type":"boolean","description":"QEMU only: expose a private QMP monitor socket in the runtime state for installer automation."});
        properties["audio"] = json!({"type":"boolean","description":"Guest sound output, played by the attached desktop viewer, and microphone input from it (Linux and Windows; a macOS Guest has sound output only, through its Guest service)."});
        tools.push(json!({"name":"vm_create","description":"Register a Linux direct-boot, Linux UEFI (distribution ISO/installed disk) or installed macOS VM configuration; does not create storage.","inputSchema":{"type":"object","properties":properties,"required":["id","guest","cpus","memory_mib"],"additionalProperties":false}}));
    }
    if execute {
        tools.push(json!({"name":"vm_exec","description":"Execute argv directly inside the VM through its guest service. No implicit shell. Output bounded by guest service; timeout enforced in guest.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128},"timeout_ms":{"type":"integer","minimum":1,"maximum":30000}},"required":["id","argv"],"additionalProperties":false}}));
    }
    if execute {
        tools.push(json!({"name":"vm_exec_start","description":"Start a bounded async guest command; returns a job id to poll. Unknown or reconnecting Guests keep a 30000 ms host limit; only opted-in QEMU Linux exec-v2 allows up to 120000 ms once vm_guest_info reports its bound no-reconnect async policy. A longer unsupported request is rejected before Guest dispatch. The guest kills the command's process group at timeout_ms (default 5000), on cancel, or when this MCP session ends.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128},"timeout_ms":{"type":"integer","minimum":1,"maximum":ASYNC_EXEC_MAX_TIMEOUT_MS}},"required":["id","argv"],"additionalProperties":false}}));
        for (name,description) in [("vm_exec_poll","Poll a job started on this VM. Inspect terminal result, exit code, cancelled and timed_out."),("vm_exec_cancel","Kill a guest job's process group. Cancellation is not rollback; poll to verify the actual terminal state.")] {tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":{"id":{"type":"string"},"job_id":{"type":"string","pattern":"^[0-9a-f]{32}$"}},"required":["id","job_id"],"additionalProperties":false}}));}
    }
    if input {
        for (name, extra, required) in [
            (
                "input_click",
                json!({"x":{"type":"integer"},"y":{"type":"integer"},"button":{"type":"string","enum":["left","middle","right"]}}),
                vec!["x", "y"],
            ),
            (
                "input_drag",
                json!({"x":{"type":"integer"},"y":{"type":"integer"},"end_x":{"type":"integer"},"end_y":{"type":"integer"},"button":{"type":"string","enum":["left","middle","right"]}}),
                vec!["x", "y", "end_x", "end_y"],
            ),
            (
                "input_scroll",
                json!({"x":{"type":"integer"},"y":{"type":"integer"},"direction":{"type":"string","enum":["up","down","left","right"]},"ticks":{"type":"integer","minimum":1,"maximum":20}}),
                vec!["x", "y", "direction", "ticks"],
            ),
            (
                "input_key",
                json!({"keys":{"type":"array","items":{"type":"string","enum":input::key_names()},"minItems":1,"maxItems":8},"hold_ms":{"type":"integer","minimum":0,"maximum":input::MAX_HOLD_MS,"description":"Hold the keys this long before release (default a 20 ms tap); games that steer or accelerate while a key is down need a hold."}}),
                vec!["keys"],
            ),
            ("input_text", input::text_tool_properties(), vec!["text"]),
        ] {
            let mut properties = extra;
            for k in ["id", "frame_id"] {
                properties[k] = json!({"type":"string"});
            }
            for k in ["width", "height", "source_generation"] {
                properties[k] = json!({"type":"integer"});
            }
            properties["observe"] = agent_view::observe_schema();
            let mut required = required;
            required.extend(["id", "frame_id", "width", "height", "source_generation"]);
            tools.push(json!({"name":format!("vm_{name}"),"description":if name == "input_text" { "Type Unicode text into the focused application of a Linux Guest on QEMU through the Guest service (XTEST with temporary keycodes), not physical key codes. Needs a screenshot younger than 5 s (or of an unchanged screen), qemu_guest_exec and a Guest service started with --allow-text-input, else fails as unsupported. A failure reports how many characters were delivered. The Guest X server acknowledges key events; application acceptance is not observed." } else { "Bounded physical input to a QEMU console, Apple Virtualization Linux desktop or opted-in macOS Guest. Needs a screenshot of this VM with matching generation and dimensions, younger than 5 s or of a screen unchanged since (digest or console draw sequence). Pass observe to get the result and a fresh frame for the next input in one call. Dispatch does not prove app acceptance; no Unicode/layout translation; vm_guest_info lists supported keys." },"inputSchema":input::coordinate_tool_schema(name, json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}))}));
        }
        tools.push(agent_view::batch_tool());
        tools.push(json!({"name":"vm_control_lease","description":"Who controls this VM's input. A person typing or clicking in an open console window holds it, and agent input (vm_input_*, window and accessibility actions, cua_* input) is refused with person_in_control until they have been quiet for quiet_ms or close the window; their input always wins. action status (default) reports holder (person, agent or none), viewers and agent_may_act_in_ms; release gives the agent's lease back until its next input.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"action":{"type":"string","enum":["status","release"]}},"required":["id"],"additionalProperties":false}}));
    }
    if files {
        for (name, description, extra, required) in [
            ("file_upload_begin", "Begin <=8MiB upload of a single filename in the enabled Guest files root. Does not overwrite. Linux uses a dedicated files account; macOS uses an opt-in private root with the service UID, not isolation from same-user programs. Query vm_guest_info for availability and file_policy.", json!({"path":{"type":"string"},"size":{"type":"integer","minimum":0,"maximum":8388608},"sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"}}),vec!["path","size","sha256"]),
            ("file_upload_write", "Append one base64 chunk <=32KiB at the exact sequential offset.",json!({"token":{"type":"string"},"offset":{"type":"integer","minimum":0},"data":{"type":"string","maxLength":43692}}),vec!["token","offset","data"]),
            ("file_upload_commit", "Verify size and SHA256 then atomically publish without replacing existing files.",json!({"token":{"type":"string"}}),vec!["token"]),
            ("file_download_begin", "Open an owned single-link regular file in the configured files directory, max8MiB.",json!({"path":{"type":"string"}}),vec!["path"]),
            ("file_download_read", "Read next <=32KiB base64 chunk; verify complete download against SHA256. Final chunk closes transfer.",json!({"token":{"type":"string"},"offset":{"type":"integer","minimum":0}}),vec!["token","offset"]),
            ("file_cancel", "Cancel this session's transfer and remove any unfinished upload. Published files are not deleted.",json!({"token":{"type":"string"}}),vec!["token"]),
        ] {
            let mut properties=extra;properties["id"]=json!({"type":"string"});let mut required=required;required.push("id");
            tools.push(json!({"name":format!("vm_{name}"),"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}));
        }
    }
    tools.extend(container_tools::tools(lifecycle, execute));
    mcp_events::extend_tools(&mut tools);
    json!({"tools":tools})
}
pub(crate) fn rpc_error(id: Value, code: i64, message: impl ToString) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.to_string()}})
}
pub(crate) struct SessionCleanup {
    pub(crate) state: Arc<SessionCleanupState>,
}
pub(crate) struct SessionCleanupState {
    pub(crate) dir: PathBuf,
    pub(crate) owner: String,
    pub(crate) stopped: AtomicBool,
    // The first cleanup owns the daemon's response. A second call must not
    // replace a failed result with an empty, apparently successful response.
    pub(crate) outcome: Mutex<Option<bool>>,
}
impl SessionCleanupState {
    pub(crate) fn finish(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let mut outcome = self.outcome.lock().unwrap();
        let clean = *outcome.get_or_insert_with(|| cleanup_session(&self.dir, &self.owner).is_ok());
        if !clean {
            bail!("session cleanup unconfirmed; a Guest cleanup request failed")
        }
        Ok(())
    }
}
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        if self.state.finish().is_err() {
            eprintln!("vm-control: session cleanup unconfirmed");
        }
    }
}
pub(crate) fn cleanup_session(dir: &Path, owner: &str) -> Result<()> {
    let result = call(dir, "session_end", json!({"owner":owner}))?;
    validate_session_cleanup(&result)
}
pub(crate) fn validate_session_cleanup(result: &Value) -> Result<()> {
    if result["closed"] != true {
        bail!("session cleanup unconfirmed")
    }
    for key in ["job_cancellation", "file_cancellation"] {
        let cancellation = &result[key];
        let attempted = cancellation["attempted"]
            .as_u64()
            .context("missing cleanup count")?;
        let outcomes = cancellation["outcomes"]
            .as_array()
            .context("missing cleanup outcomes")?;
        if cancellation["unconfirmed_requests"].as_u64() != Some(0)
            || outcomes.len() as u64 != attempted
            || outcomes.iter().any(|outcome| {
                outcome.get("error").is_some()
                    || outcome.get("result").is_none()
                    || outcome["result"].get("error").is_some()
                    || if key == "file_cancellation" {
                        outcome["result"]["result"]["cancelled_transfers"]
                            .as_u64()
                            .is_none()
                    } else {
                        outcome["result"]["result"]["cancel_requested"]
                            .as_bool()
                            .is_none()
                    }
            })
        {
            bail!("Guest cleanup request failed or unconfirmed")
        }
    }
    // A valid cancel_requested reply is not evidence that a job has reached its
    // terminal state. This receipt covers the cleanup RPC outcomes.
    Ok(())
}

pub fn mcp(dir: &Path, lifecycle: bool, execute: bool, files: bool) -> Result<()> {
    mcp_with_input(dir, lifecycle, execute, files, false)
}
pub fn mcp_with_input(
    dir: &Path,
    lifecycle: bool,
    execute: bool,
    files: bool,
    input: bool,
) -> Result<()> {
    mcp_with_tools(
        dir,
        lifecycle,
        execute,
        files,
        input,
        tool_groups::default(),
    )
}
pub fn mcp_with_tools(
    dir: &Path,
    lifecycle: bool,
    execute: bool,
    files: bool,
    input: bool,
    groups: BTreeSet<&'static str>,
) -> Result<()> {
    let options = McpOptions {
        lifecycle,
        execute,
        files,
        input,
        ..McpOptions::default()
    };
    mcp_serve(dir, &options, groups)
}
/// What one stdio MCP session may call (`--allow-*`), and the VM a per-VM endpoint serves.
#[derive(Clone, Debug, Default)]
pub struct McpOptions {
    pub lifecycle: bool,
    pub execute: bool,
    pub files: bool,
    pub input: bool,
    /// cua-driver `browser_*` tools that act on pages.
    pub browser: bool,
    /// cua-driver clipboard tools.
    pub clipboard: bool,
    /// `--vm ID`: serve that VM's cua-driver tools under their own names, plus its
    /// checkpoint and snapshot tools, instead of the VM tools.
    pub vm: Option<String>,
}
impl McpOptions {
    pub fn scopes(&self) -> cua_bridge::Scopes {
        cua_bridge::Scopes {
            input: self.input,
            browser: self.browser,
            clipboard: self.clipboard,
            execute: self.execute,
        }
    }
}
/// Every tool these permissions grant on the main MCP: the VM tools and the `cua` group.
pub fn tools_for(options: &McpOptions) -> Vec<Value> {
    let mut tools = tools_with_input(
        options.lifecycle,
        options.execute,
        options.files,
        options.input,
    )["tools"]
        .as_array()
        .unwrap()
        .clone();
    tools.extend(cua_bridge::curated_tools(options.scopes()));
    tools
}
/// The per-VM endpoint's tools: the driver's own (scope-filtered by the bridge) and the
/// VM-level ones. A driver that cannot answer (VM stopped, image without cua-driver)
/// leaves the VM-level tools.
pub(crate) fn endpoint_tools(dir: &Path, vm: &str, options: &McpOptions) -> Vec<Value> {
    let mut tools = call(
        dir,
        "driver_rpc",
        json!({"id":vm,"list":true,"scopes":options.scopes().to_value()}),
    )
    .ok()
    .and_then(|list| list["tools"].as_array().cloned())
    .unwrap_or_default();
    tools.extend(cua_bridge::vm_level_tools(&tools_for(options), vm));
    tools
}
/// A VM tool's result as MCP text content.
pub(crate) fn text_content(result: Result<Value>) -> Result<Value> {
    Ok(match result {
        Ok(v) => {
            json!({"content":[{"type":"text","text":serde_json::to_string(&v)?}],"isError":false})
        }
        Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
    })
}
/// A driver result is already MCP content; a failed exchange becomes an error result.
pub(crate) fn driver_content(result: Result<Value>) -> Value {
    result.unwrap_or_else(
        |e| json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
    )
}
/// The stdio MCP server. `groups` narrows the listed tools within the permissions;
/// `vm_tools_enable` widens it again up to them. With `options.vm` it is that VM's
/// endpoint instead: no groups, resources or VM tools beyond the VM-level ones.
pub fn mcp_serve(dir: &Path, options: &McpOptions, groups: BTreeSet<&'static str>) -> Result<()> {
    let stdin = std::io::stdin();
    mcp_session(
        dir,
        options,
        groups,
        stdin.lock(),
        std::io::stdout(),
        |wait| await_peer_disconnect(&[libc::STDIN_FILENO, libc::STDOUT_FILENO], wait),
    )
}

/// One MCP session over `reader` and `sink`: the stdio server's, or the
/// daemon's `mcp.serve` stream. `peer_gone` waits up to its argument for the
/// client to go away and says whether it has, so a blocked tool does not
/// defer the session's cleanup.
pub fn mcp_session(
    dir: &Path,
    options: &McpOptions,
    mut groups: BTreeSet<&'static str>,
    mut reader: impl BufRead,
    sink: impl Write + Send + 'static,
    peer_gone: impl Fn(Duration) -> bool + Send + 'static,
) -> Result<()> {
    let permitted = || tools_for(options);
    let endpoint = options.vm.as_deref();
    if let Some(vm) = endpoint {
        validate_id(vm)?;
    }
    let scopes = options.scopes();
    let execute = options.execute;
    let list_changed = endpoint.is_none() && !tool_groups::hidden(&permitted(), &groups).is_empty();
    let out = mcp_events::Out::new(sink);
    let mut random = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let owner = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let disconnected = Arc::new(AtomicBool::new(false));
    let cleanup = SessionCleanup {
        state: Arc::new(SessionCleanupState {
            dir: dir.to_path_buf(),
            owner: owner.clone(),
            stopped: AtomicBool::new(false),
            outcome: Mutex::new(None),
        }),
    };
    let monitor_cleanup = cleanup.state.clone();
    let monitor_disconnected = disconnected.clone();
    thread::spawn(move || {
        while !monitor_cleanup.stopped.load(Ordering::Acquire) {
            if peer_gone(PEER_MONITOR_WAIT) {
                monitor_disconnected.store(true, Ordering::Release);
                let _ = monitor_cleanup.finish();
                break;
            }
        }
    });
    let events = mcp_events::Session::new(dir, out.clone(), execute);
    let protocol_result = (|| -> Result<()> {
        let mut agent = agent_view::Session::default();
        let mut initialized = false;
        let mut ready = false;
        while let Some(bytes) = read_frame(&mut reader)? {
            // A blocked synchronous tool must not defer cleanup until the next stdin read.
            // Also discard queued requests after a disconnected transport was observed.
            if disconnected.load(Ordering::Acquire) {
                break;
            }
            let request: Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    out.send(&rpc_error(Value::Null, -32700, "Parse error"))?;
                    continue;
                }
            };
            let id = request.get("id").cloned();
            if request["jsonrpc"] != "2.0"
                || !request["method"].is_string()
                || id
                    .as_ref()
                    .is_some_and(|v| !v.is_string() && !v.is_i64() && !v.is_u64())
            {
                out.send(&rpc_error(Value::Null, -32600, "Invalid request"))?;
                continue;
            }
            let method = request["method"].as_str().unwrap();
            if id.is_none() {
                if method == "notifications/initialized" && initialized {
                    ready = true;
                }
                // Disconnection/cancellation does not terminate independently running VM operations.
                continue;
            }
            let id = id.unwrap();
            let progress = if ready && method == "tools/call" {
                mcp_events::Progress::start(
                    &out,
                    &request,
                    request["params"]["name"].as_str().unwrap_or("tool"),
                )
            } else {
                None
            };
            let response = if method == "initialize" {
                if initialized {
                    rpc_error(id, -32600, "Already initialized")
                } else if !request["params"]["protocolVersion"].is_string()
                    || !request["params"]["capabilities"].is_object()
                    || !request["params"]["clientInfo"]["name"].is_string()
                    || !request["params"]["clientInfo"]["version"].is_string()
                {
                    rpc_error(
                        id,
                        -32602,
                        "Initialization requires protocolVersion, capabilities and clientInfo",
                    )
                } else {
                    initialized = true;
                    if let Some(vm) = endpoint {
                        let reply = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"veynode-vm-cua","version":env!("CARGO_PKG_VERSION")},"instructions":format!("cua-driver in the Linux Guest desktop of VM {vm}, with its own tool names and schemas, limited to this session's scopes (observe by default; input, browser, clipboard and execute are granted separately), plus this VM's checkpoint and snapshot tools. install_extension, check_for_update, install_ffmpeg and set_config are never available. `effect:\"unverifiable\"` and `unknown` are not success. A Guest image without cua-driver lists only the VM-level tools.")}});
                        out.send(&reply)?;
                        continue;
                    }
                    let mut reply = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":list_changed}},"serverInfo":{"name":"veynode-vm","version":env!("CARGO_PKG_VERSION")},"instructions":"Local trusted-user daemon. Observe is default; server --allow-lifecycle, --allow-execute, --allow-files and --allow-input each add their own tools, and --tools lists only some tool groups (vm_tools_enable, when listed, adds the rest). Commands need enabled Linux Guest async jobs; macOS legacy execution is not exposed by vm_exec, but container_exec runs commands in veynodevm macOS containers over SSH. Commands run under a guest timeout; the guest kills a command's process group when it times out, is cancelled or this MCP session ends. File transfer needs an enabled Guest files root: a dedicated Linux files account, or macOS --allow-files --files-root with a private directory owned by the service UID, not isolated from other same-UID programs. Query vm_guest_info for actual capabilities, file_policy, macOS permission availability and supported physical keys; Guest denial is an error, not permission to use another path. Screenshots need QEMU capture, an Apple Virtualization Linux desktop container (--desktop; captured and driven by the guest agent with no Guest setup), a veynodevm macOS container (Guest Screen Sharing; no Guest permission grants) or macOS Guest --allow-observe with existing Screen Recording permission. On Linux desktop and macOS containers the desktop and accessibility tools read windows, applications and the accessibility tree without a screenshot; acting on them needs input permission. Physical input needs one of those screenshot backends (macOS Guests also --allow-input with existing Accessibility permission) and a matching screenshot younger than 5 seconds or of an unchanged screen; pass observe on input and accessibility actions to get what changed and a fresh frame in the same call. Accessibility trees default to compact lines with short refs (eN) valid for this session. Physical keys carry no Unicode text or keyboard-layout translation. Backend input acknowledgement does not prove application acceptance. VM start returns before guest readiness."}});
                    mcp_events::initialize(&mut reply);
                    reply
                }
            } else if method == "ping" {
                json!({"jsonrpc":"2.0","id":id,"result":{}})
            } else if !ready {
                rpc_error(
                    id,
                    -32000,
                    "Initialize and send notifications/initialized first",
                )
            } else if let Some(vm) = endpoint {
                match method {
                    "tools/list" => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":endpoint_tools(dir, vm, options)}})
                    }
                    "tools/call" => {
                        let name = request["params"]["name"].as_str().unwrap_or("");
                        let args = request["params"]
                            .get("arguments")
                            .cloned()
                            .unwrap_or(json!({}));
                        if !args.is_object() {
                            rpc_error(id, -32602, "Invalid tool arguments")
                        } else {
                            let vm_tools = cua_bridge::vm_level_tools(&permitted(), vm);
                            let content = match cua_bridge::endpoint_call(
                                name, args, vm, scopes, &vm_tools,
                            ) {
                                cua_bridge::EndpointCall::Vm(name, args) => {
                                    text_content(if name.starts_with("container_") {
                                        container_tools::call(dir, &name, &args)
                                    } else {
                                        call(dir, name.strip_prefix("vm_").unwrap(), args)
                                    })?
                                }
                                cua_bridge::EndpointCall::Driver(rpc) => {
                                    driver_content(call(dir, "driver_rpc", rpc))
                                }
                            };
                            json!({"jsonrpc":"2.0","id":id,"result":content})
                        }
                    }
                    _ => rpc_error(id, -32601, "Method not found"),
                }
            } else if let Some(result) = events.handle(method, &request["params"]) {
                match result {
                    Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                    Err(error) => rpc_error(id, -32002, error),
                }
            } else if method == "tools/list" {
                json!({"jsonrpc":"2.0","id":id,"result":{"tools":tool_groups::select(permitted(), &groups)}})
            } else if method == "tools/call" {
                let name = request["params"]["name"].as_str().unwrap_or("");
                let listed = tool_groups::select(permitted(), &groups);
                let known = listed.iter().find(|t| t["name"] == name);
                if known.is_none() {
                    let message = if permitted().iter().any(|t| t["name"] == name) {
                        "Tool is in a group this session has not enabled; call vm_tools_enable"
                    } else {
                        "Tool unknown or permission not granted"
                    };
                    rpc_error(id, -32602, message)
                } else {
                    let mut args = request["params"]
                        .get("arguments")
                        .cloned()
                        .unwrap_or(json!({}));
                    let schema = known.map_or(Value::Null, |t| t["inputSchema"].clone());
                    // cua tools list their main arguments only; the driver validates the rest.
                    let open = schema["additionalProperties"] == true;
                    let valid = args.as_object().is_some_and(|a| {
                        open || a.keys().all(|k| schema["properties"].get(k).is_some())
                    }) && schema["required"]
                        .as_array()
                        .is_none_or(|r| r.iter().all(|k| args.get(k.as_str().unwrap()).is_some()));
                    if !valid {
                        rpc_error(id, -32602, "Invalid tool arguments")
                    } else if name == tool_groups::ENABLE_TOOL {
                        let offered = tool_groups::hidden(&permitted(), &groups);
                        let asked: Option<BTreeSet<&'static str>> = args["groups"]
                            .as_array()
                            .and_then(|a| {
                                a.iter()
                                    .map(|g| g.as_str().and_then(|g| offered.get(g).copied()))
                                    .collect()
                            })
                            .filter(|asked: &BTreeSet<_>| !asked.is_empty());
                        match asked {
                            None => rpc_error(id, -32602, "Invalid tool arguments"),
                            Some(asked) => {
                                let before = tool_groups::select(permitted(), &groups);
                                groups.extend(asked.iter());
                                let added: Vec<_> = tool_groups::select(permitted(), &groups)
                                    .into_iter()
                                    .filter(|t| !before.iter().any(|b| b["name"] == t["name"]))
                                    .map(|t| t["name"].clone())
                                    .collect();
                                let result = json!({"enabled":asked,"added_tools":added});
                                // The reply goes first; the notification follows as this
                                // request's last frame, so the client refetches the list.
                                out.send(
                                    &json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":result.to_string()}],"isError":false}}),
                                )?;
                                json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})
                            }
                        }
                    } else {
                        // The session owner lets session_end cancel this session's
                        // jobs and file transfers when the client goes away.
                        if name.starts_with("vm_file_")
                            || ["vm_exec", "vm_exec_start"].contains(&name)
                        {
                            args["owner"] = json!(owner);
                        }
                        let result = if agent_view::handles(name) {
                            agent.call(&mut agent_view::Live(dir), name, args)
                        } else if name.starts_with(cua_bridge::PREFIX) {
                            let tool = name.strip_prefix(cua_bridge::PREFIX).unwrap();
                            let vm = args["vm"].as_str().unwrap_or_default().to_owned();
                            match agent.ocr_token_call(&vm, tool, &args) {
                                Some(answer) => Ok(answer),
                                None => {
                                    let outcome = cua_bridge::curated_call(name, args, scopes)
                                        .and_then(|rpc| call(dir, "driver_rpc", rpc));
                                    // No driver in this Guest: the host reads the screen.
                                    if tool == "get_window_state"
                                        && cua_bridge::helperless(&outcome)
                                    {
                                        match agent
                                            .ocr_window_state(&mut agent_view::Live(dir), &vm)
                                        {
                                            Ok(answer) => Ok(answer),
                                            Err(ocr) => Ok(driver_content(outcome.map(|mut r| {
                                                r["_meta"]["veynode/host_ocr_error"] =
                                                    json!(ocr.to_string());
                                                r
                                            }))),
                                        }
                                    } else if tool == "verify_state"
                                        && cua_bridge::helperless(&outcome)
                                    {
                                        // Nothing in the Guest can check a predicate: unknown, never an error.
                                        let structured = json!({"status":"unknown","reason":"driver_unavailable","hint":"This Guest has no cua-driver; read the screen with cua_get_window_state (host OCR) or vm_screen_text and compare yourself."});
                                        Ok(
                                            json!({"content":[{"type":"text","text":structured.to_string()}],"structuredContent":structured,"isError":false}),
                                        )
                                    } else {
                                        Ok(driver_content(outcome))
                                    }
                                }
                            }
                        } else if name.starts_with("container_") || name.starts_with("image_") {
                            container_tools::call(dir, name, &args)
                        } else if name == "vm_exec_poll" && args.get("wait_ms").is_some() {
                            mcp_events::exec_poll_wait(dir, args)
                        } else {
                            call(dir, name.strip_prefix("vm_").unwrap(), args)
                        };
                        let content = match result {
                            // Already MCP content.
                            Ok(v)
                                if agent_view::handles(name)
                                    || name.starts_with(cua_bridge::PREFIX) =>
                            {
                                v
                            }
                            Ok(v) => {
                                json!({"content":[{"type":"text","text":serde_json::to_string(&v)?}],"isError":false})
                            }
                            Err(e) => {
                                json!({"content":[{"type":"text","text":e.to_string()}],"isError":true})
                            }
                        };
                        json!({"jsonrpc":"2.0","id":id,"result":content})
                    }
                }
            } else {
                rpc_error(id, -32601, "Method not found")
            };
            if let Some(progress) = progress {
                progress.finish();
            }
            out.send(&response)?;
        }
        Ok(())
    })();
    drop(events);
    let cleanup_result = cleanup.state.finish();
    protocol_result.and(cleanup_result)
}
