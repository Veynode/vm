//! MCP `container_*` and `image_*` tools.
//!
//! Containers and images are the daemon's, so these tools are API 1 calls
//! (`container.*`, `checkpoint.*`, `image.*`) to the daemon of this state
//! directory. Host bind mounts are deliberately not exposed: an MCP client
//! must not be able to map arbitrary host directories.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, process::Command, time::Duration};
use veynodevm_api::{Client, ErrorCode, Event};

const OUTPUT_LIMIT: usize = 64 * 1024;

pub fn tools(lifecycle: bool, execute: bool) -> Vec<Value> {
    let container = json!({"type":"object","properties":{"container":{"type":"string","description":"Container name or id (prefix)"}},"required":["container"],"additionalProperties":false});
    let mut tools = vec![
        json!({"name":"container_list","description":"List micro-VM containers (all states) with image, status, published ports, GPU profile and the backing vm_id usable with vm_* tools (screenshot, exec, input).","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"container_inspect","description":"Container record, current state, guest IP, exit code, GPU device report and backing VM status.","inputSchema":container}),
        json!({"name":"container_logs","description":"Container output so far (console between start and exit markers; for macOS, the output of the run COMMAND), last 64 KiB.","inputSchema":container}),
        json!({"name":"container_checkpoint_list","description":"List a Linux container's checkpoints (name, time, whether it was running and synced).","inputSchema":container}),
        json!({"name":"image_list","description":"List local images: linux/arm64 OCI images and Tart-format macOS images.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ];
    if execute {
        tools.push(json!({"name":"container_exec","description":"Run a command in a running container and return exit_code/stdout/stderr. Linux: bounded Guest job (30 s). macOS: over SSH with the image login, waiting for the Guest to boot first.","inputSchema":{"type":"object","properties":{
            "container":{"type":"string"},
            "command":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":128}
        },"required":["container","command"],"additionalProperties":false}}));
    }
    if lifecycle {
        tools.push(json!({"name":"image_pull","description":"Pull an image from a registry (anonymous): linux/arm64 OCI images get a read-only root; Tart macOS images (e.g. ghcr.io/cirruslabs/macos-tahoe-vanilla, ~25 GB download, ~40 GB disk) become a macOS VM base.","inputSchema":{"type":"object","properties":{"image":{"type":"string"}},"required":["image"],"additionalProperties":false}}));
        tools.push(json!({"name":"container_run","description":"Create and start a detached container as its own micro-VM. Without gpus/desktop it runs on Apple Virtualization; gpus selects QEMU/HVF with virgl (GL) or experimental Venus (Vulkan via MoltenVK). A macOS image runs as a macOS VM with Apple's paravirtualized Metal GPU (omit command and gpus; use container_exec, and the vm_* screenshot and input tools on its vm_id through Guest Screen Sharing). Host bind mounts are not available through MCP.","inputSchema":{"type":"object","properties":{
            "image":{"type":"string"},
            "command":{"type":"array","items":{"type":"string"},"maxItems":128},
            "name":{"type":"string","pattern":"^[A-Za-z0-9_.-]{1,64}$"},
            "env":{"type":"array","items":{"type":"string"},"maxItems":64},
            "ports":{"type":"array","items":{"type":"string","pattern":"^(127\\.0\\.0\\.1:)?[0-9]{1,5}:[0-9]{1,5}(/(tcp|udp))?$"},"maxItems":32,"description":"HOST:GUEST, loopback only"},
            "cpus":{"type":"integer","minimum":1,"maximum":16},
            "memory_mib":{"type":"integer","minimum":256,"maximum":65536},
            "gpus":{"type":"string","enum":["virgl","venus"]},
            "desktop":{"type":"boolean"}
        },"required":["image"],"additionalProperties":false}}));
        let checkpoint = json!({"type":"object","properties":{"container":{"type":"string"},"name":{"type":"string","pattern":"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$"},"memory":{"type":"boolean","description":"Also save RAM/CPU/device state when the backend can (default true)"}},"required":["container","name"],"additionalProperties":false});
        tools.push(json!({"name":"container_checkpoint","description":"Checkpoint a Linux container before a risky step. Its writable disk layer is an instant APFS clone (no data copied; blocks diverge only as the container writes). On Apple Virtualization a running container also gets its memory, CPU and device state saved in the same brief pause (kind warm; the state file is about the size of the Guest's used memory), so rollback resumes running processes. QEMU/GPU containers and memory:false get a crash-consistent disk-only checkpoint (kind disk; warm_unavailable says why). Returns create_ms.","inputSchema":checkpoint}));
        tools.push(json!({"name":"container_rollback","description":"Roll a Linux container back to a checkpoint: hard-stops it (its current state is discarded) and swaps in a clone of the checkpoint's disk. If it was running it runs again: from the checkpoint's saved memory when there is one (path warm: processes continue, the clock is re-synced), otherwise by booting (path cold: the entrypoint runs again); a failed warm restore falls back to cold and reports warm_error. Requires confirm equal to the container given. Returns path and timings.","inputSchema":{"type":"object","properties":{"container":{"type":"string"},"name":{"type":"string"},"confirm":{"type":"string"},"start":{"type":"boolean","description":"Run afterwards (default: as before the rollback)"},"memory":{"type":"boolean","description":"Resume saved memory when the checkpoint has it (default true); false boots cold"}},"required":["container","name","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"container_checkpoint_delete","description":"Delete one checkpoint of a container. Requires confirm equal to the checkpoint name.","inputSchema":{"type":"object","properties":{"container":{"type":"string"},"name":{"type":"string"},"confirm":{"type":"string"}},"required":["container","name","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"container_fork","description":"Fork a Linux container into count new containers that start from the same disk state (a checkpoint, or the current layer synced and cloned once), to try approaches in parallel. Each fork is an APFS clone with a fresh container id, hostname, machine-id, session token, MAC address and new free host ports for the source's published ports. Forks always cold-boot, also from a warm checkpoint: Apple Virtualization restores saved memory only with the source's MAC address and machine identifier, which every fork would share. Refused before creating anything if count x memory exceeds the daemon's remaining memory budget. Returns the forks (name, vm_id for vm_* tools, ports) and a group id for container_fork_rm.","inputSchema":{"type":"object","properties":{
            "container":{"type":"string"},
            "count":{"type":"integer","minimum":1,"maximum":16},
            "checkpoint":{"type":"string","pattern":"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$"},
            "start":{"type":"boolean","description":"Boot the forks and wait until their entrypoints start (default true)"}
        },"required":["container","count"],"additionalProperties":false}}));
        tools.push(json!({"name":"container_fork_rm","description":"Stop and delete forks with their disks: every fork of one container_fork call (its group id) or every fork of a source container. Requires confirm equal to the selector. Never removes the source.","inputSchema":{"type":"object","properties":{"selector":{"type":"string","description":"Fork group id or source container"},"confirm":{"type":"string"}},"required":["selector","confirm"],"additionalProperties":false}}));
        tools.push(json!({"name":"container_stop","description":"Stop a container (guest power-off, then hard stop after the timeout).","inputSchema":container}));
        tools.push(json!({"name":"container_rm","description":"Remove a stopped container and its writable layer. Requires confirm equal to the container name or id given.","inputSchema":{"type":"object","properties":{"container":{"type":"string"},"confirm":{"type":"string"}},"required":["container","confirm"],"additionalProperties":false}}));
    }
    tools
}

fn client(dir: &Path) -> Client {
    Client::at_socket(
        dir.join("control.sock"),
        concat!("vm-control-mcp/", env!("CARGO_PKG_VERSION")),
    )
}

fn api(dir: &Path, method: &str, args: Value, timeout: Duration) -> Result<Value> {
    client(dir).call_within(method, args, timeout)
}

/// A streaming method's answer, with its output (the last 64 KiB of each
/// channel) as `stdout` and `stderr`.
fn collect(dir: &Path, method: &str, args: Value) -> Result<(Value, String, String)> {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = client(dir).stream(method, args)?.finish(|event| {
        match event {
            Event::Stdout(data) => out.extend_from_slice(&data),
            Event::Stderr(data) => err.extend_from_slice(&data),
            _ => {}
        }
        Ok(())
    })?;
    Ok((result, tail(&out), tail(&err)))
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let start = text.len().saturating_sub(OUTPUT_LIMIT);
    let start = (start..text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    text[start..].to_owned()
}

/// `vm-control` with the home of `dir` (a daemon state directory is
/// `<home>/state`).
fn core(dir: &Path) -> Result<Command> {
    let mut command = Command::new(crate::services::core_exe()?);
    if dir.file_name().is_some_and(|n| n == "state") {
        command.env("VEYNODEVM_HOME", dir.parent().unwrap());
    }
    Ok(command)
}

fn string(args: &Value, key: &str) -> Result<String> {
    let value = args[key]
        .as_str()
        .with_context(|| format!("{key} must be a string"))?;
    if value.is_empty() || value.starts_with('-') || value.len() > 512 {
        bail!("invalid {key}")
    }
    Ok(value.to_owned())
}

fn strings(args: &Value, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None => Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .with_context(|| format!("{key} must be an array"))?
            .iter()
            .map(|s| {
                s.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("{key} items must be strings"))
            })
            .collect(),
    }
}

/// The cua-driver link of a macOS container: the driver runs in the Guest
/// over SSH on the stdio of `vm-control __driver`.
pub fn macos_driver(dir: &Path, vm: &str) -> crate::driver_link::Link {
    let (dir, vm) = (dir.to_path_buf(), vm.to_owned());
    crate::driver_link::Link::new(Box::new(move || {
        let mut command = core(&dir)?;
        command.args(["__driver", &vm]);
        Ok(command)
    }))
}

/// A desktop or accessibility method of a macOS container, run by its
/// helper in the Guest over SSH (`vm-control __desktop`), in its own process
/// so a stuck Guest costs the caller only the timeout.
pub fn desktop(dir: &Path, vm: &str, method: &str, arguments: &Value) -> Result<Value> {
    let mut command = core(dir)?;
    command
        .args(["__desktop", vm, method, &arguments.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{method} on {vm} timed out")
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "{}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .trim_start_matches("vm-control: ")
        )
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

pub fn call(dir: &Path, name: &str, args: &Value) -> Result<Value> {
    let quick = Duration::from_secs(30);
    let long = Duration::from_secs(600);
    match name {
        "container_list" => api(dir, "container.ls", json!({"all":true}), quick),
        "image_list" => api(dir, "image.ls", json!({}), quick),
        "container_inspect" => {
            let name = string(args, "container")?;
            match api(dir, "container.inspect", json!({"name":name}), quick) {
                Err(e) if veynodevm_api::code_of(&e) == ErrorCode::NotFound => {
                    api(dir, "image.inspect", json!({"reference":name}), quick)
                }
                other => other,
            }
        }
        "container_logs" => {
            let (result, out, err) = collect(
                dir,
                "container.logs",
                json!({"name":string(args, "container")?}),
            )?;
            Ok(json!({"output":format!("{out}{err}"),"exit_code":result["exitCode"]}))
        }
        "container_stop" => api(
            dir,
            "container.stop",
            json!({"name":string(args, "container")?}),
            Duration::from_secs(60),
        ),
        "container_rm" => {
            let container = string(args, "container")?;
            if args["confirm"].as_str() != Some(container.as_str()) {
                bail!("container_rm requires confirm equal to container")
            }
            api(
                dir,
                "container.rm",
                json!({"name":container}),
                Duration::from_secs(60),
            )
        }
        "container_checkpoint_list" => api(
            dir,
            "checkpoint.ls",
            json!({"name":string(args, "container")?}),
            quick,
        ),
        "container_checkpoint" => api(
            dir,
            "checkpoint.create",
            json!({"name":string(args, "container")?,"checkpoint":string(args, "name")?,"memory":args["memory"] != false}),
            Duration::from_secs(330),
        ),
        "container_checkpoint_delete" => {
            let name = string(args, "name")?;
            if args["confirm"].as_str() != Some(name.as_str()) {
                bail!("container_checkpoint_delete requires confirm equal to name")
            }
            api(
                dir,
                "checkpoint.rm",
                json!({"name":string(args, "container")?,"checkpoint":name}),
                quick,
            )
        }
        "container_rollback" => {
            let container = string(args, "container")?;
            if args["confirm"].as_str() != Some(container.as_str()) {
                bail!("container_rollback requires confirm equal to container")
            }
            api(
                dir,
                "checkpoint.rollback",
                json!({"name":container,"checkpoint":string(args, "name")?,"start":args["start"].as_bool(),"memory":args["memory"] != false}),
                Duration::from_secs(300),
            )
        }
        "container_fork" => {
            let count = args["count"]
                .as_u64()
                .filter(|n| (1..=16).contains(n))
                .context("count must be 1..16")?;
            let mut request = json!({"name":string(args, "container")?,"count":count,"start":args["start"] != false});
            if args.get("checkpoint").is_some() {
                request["checkpoint"] = json!(string(args, "checkpoint")?);
            }
            api(dir, "checkpoint.fork", request, long)
        }
        "container_fork_rm" => {
            let selector = string(args, "selector")?;
            if args["confirm"].as_str() != Some(selector.as_str()) {
                bail!("container_fork_rm requires confirm equal to selector")
            }
            api(
                dir,
                "checkpoint.unfork",
                json!({"selector":selector}),
                Duration::from_secs(300),
            )
        }
        "container_exec" => {
            let command = strings(args, "command")?;
            if command.is_empty() {
                bail!("command must not be empty")
            }
            let (result, stdout, stderr) = collect(
                dir,
                "container.exec",
                json!({"name":string(args, "container")?,"argv":command}),
            )?;
            Ok(
                json!({"exit_code":result["exitCode"],"stdout":stdout,"stderr":stderr,"timed_out":result["timedOut"]}),
            )
        }
        "image_pull" => Ok(collect(
            dir,
            "image.pull",
            json!({"reference":string(args, "image")?}),
        )?
        .0),
        "container_run" => {
            let mut options = json!({"image":string(args, "image")?,"command":strings(args, "command")?,"detach":true,"quiet":true});
            if let Some(n) = args["name"].as_str() {
                options["name"] = json!(n);
            }
            let env = strings(args, "env")?;
            if env.iter().any(|e| !e.contains('=')) {
                bail!("env entries must be KEY=VALUE")
            }
            options["env"] = json!(env);
            options["ports"] = json!(strings(args, "ports")?
                .into_iter()
                .map(|p| if p.starts_with("127.0.0.1:") {
                    p
                } else {
                    format!("127.0.0.1:{p}")
                })
                .collect::<Vec<_>>());
            if let Some(c) = args["cpus"].as_u64() {
                options["cpus"] = json!(c);
            }
            if let Some(m) = args["memory_mib"].as_u64() {
                options["memoryMiB"] = json!(m);
            }
            if let Some(g) = args["gpus"].as_str() {
                options["gpus"] = json!(g);
            }
            if args["desktop"] == true {
                options["desktop"] = json!(true);
            }
            Ok(collect(dir, "container.run", options)?.0)
        }
        _ => bail!("unknown container tool"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scopes() {
        let names = |l| {
            super::tools(l, false)
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert!(!names(false).contains(&"container_run".to_owned()));
        assert!(names(true).contains(&"container_run".to_owned()));
        assert!(names(false).contains(&"container_list".to_owned()));
        assert!(names(false).contains(&"container_checkpoint_list".to_owned()));
        for mutating in [
            "container_checkpoint",
            "container_rollback",
            "container_checkpoint_delete",
            "container_fork",
            "container_fork_rm",
        ] {
            assert!(!names(false).contains(&mutating.to_owned()), "{mutating}");
            assert!(names(true).contains(&mutating.to_owned()), "{mutating}");
        }
        assert!(super::tools(false, true)
            .iter()
            .any(|t| t["name"] == "container_exec"));
        assert!(!super::tools(true, false)
            .iter()
            .any(|t| t["name"] == "container_exec"));
    }
    #[test]
    fn option_injection_refused() {
        assert!(super::string(&serde_json::json!({"image":"--rm"}), "image").is_err());
    }
    #[test]
    fn destructive_tools_need_confirmation() {
        let dir = std::path::Path::new("/nonexistent");
        for (name, args) in [
            (
                "container_rollback",
                serde_json::json!({"container":"a","name":"x","confirm":"b"}),
            ),
            (
                "container_checkpoint_delete",
                serde_json::json!({"container":"a","name":"x","confirm":"a"}),
            ),
            (
                "container_fork_rm",
                serde_json::json!({"selector":"g1","confirm":"g2"}),
            ),
        ] {
            let error = super::call(dir, name, &args).unwrap_err().to_string();
            assert!(error.contains("confirm"), "{name}: {error}");
        }
        let error = super::call(
            dir,
            "container_fork",
            &serde_json::json!({"container":"a","count":17}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("count"), "{error}");
    }
}
