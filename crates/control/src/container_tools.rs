//! MCP `container_*` and `image_*` tools.
//!
//! Containers and images are owned by the `veynodevm` CLI (image store,
//! container records, port publishers), so these tools run the sibling
//! `veynodevm --json` executable against the same VeynodeVM home as this
//! daemon state directory. Host bind mounts are deliberately not exposed: an
//! MCP client must not be able to map arbitrary host directories.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

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

fn cli() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let path = exe
        .parent()
        .context("executable has no parent")?
        .join("veynodevm");
    if !path.is_file() {
        bail!("veynodevm CLI not found next to {}", exe.display())
    }
    Ok(path)
}

fn run(dir: &Path, args: &[String], timeout: Duration) -> Result<Value> {
    let mut command = Command::new(cli()?);
    command
        .arg("--json")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The daemon state directory is <home>/state; keep the CLI on that home.
    if dir.file_name().is_some_and(|n| n == "state") {
        command.env("VEYNODEVM_HOME", dir.parent().unwrap());
    }
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "veynodevm {} timed out",
                args.first().map(String::as_str).unwrap_or("")
            )
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let bytes = reader.join().unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let value = serde_json::from_str::<Value>(text.trim()).unwrap_or_else(|_| {
        let start = text.len().saturating_sub(OUTPUT_LIMIT);
        let start = (start..text.len())
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(text.len());
        json!({"output": &text[start..], "exit_code": status.code()})
    });
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        bail!("{error}")
    }
    Ok(value)
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

/// A desktop or accessibility method of a macOS container: the CLI runs its
/// helper in the Guest over SSH (`veynodevm __desktop`).
pub fn desktop(dir: &Path, vm: &str, method: &str, arguments: &Value) -> Result<Value> {
    run(
        dir,
        &[
            "__desktop".into(),
            vm.into(),
            method.into(),
            arguments.to_string(),
        ],
        Duration::from_secs(30),
    )
}

pub fn call(dir: &Path, name: &str, args: &Value) -> Result<Value> {
    let quick = Duration::from_secs(30);
    match name {
        "container_list" => run(dir, &["ps".into(), "-a".into()], quick),
        "image_list" => run(dir, &["images".into()], quick),
        "container_inspect" => run(dir, &["inspect".into(), string(args, "container")?], quick),
        "container_logs" => run(dir, &["logs".into(), string(args, "container")?], quick),
        "container_stop" => run(
            dir,
            &["stop".into(), string(args, "container")?],
            Duration::from_secs(60),
        ),
        "container_rm" => {
            let container = string(args, "container")?;
            if args["confirm"].as_str() != Some(container.as_str()) {
                bail!("container_rm requires confirm equal to container")
            }
            run(dir, &["rm".into(), container], Duration::from_secs(60))
        }
        "container_checkpoint_list" => run(
            dir,
            &["checkpoint".into(), "ls".into(), string(args, "container")?],
            quick,
        ),
        "container_checkpoint" => {
            let mut argv = vec!["checkpoint".into(), "create".into()];
            if args["memory"] == false {
                argv.push("--disk-only".into());
            }
            argv.extend([string(args, "container")?, string(args, "name")?]);
            run(dir, &argv, Duration::from_secs(330))
        }
        "container_checkpoint_delete" => {
            let name = string(args, "name")?;
            if args["confirm"].as_str() != Some(name.as_str()) {
                bail!("container_checkpoint_delete requires confirm equal to name")
            }
            run(
                dir,
                &[
                    "checkpoint".into(),
                    "rm".into(),
                    string(args, "container")?,
                    name,
                ],
                quick,
            )
        }
        "container_rollback" => {
            let container = string(args, "container")?;
            if args["confirm"].as_str() != Some(container.as_str()) {
                bail!("container_rollback requires confirm equal to container")
            }
            let mut argv = vec!["rollback".into()];
            match args["start"].as_bool() {
                Some(true) => argv.push("--start".into()),
                Some(false) => argv.push("--no-start".into()),
                None => {}
            }
            if args["memory"] == false {
                argv.push("--cold".into());
            }
            argv.extend([container, string(args, "name")?]);
            run(dir, &argv, Duration::from_secs(300))
        }
        "container_fork" => {
            let count = args["count"]
                .as_u64()
                .filter(|n| (1..=16).contains(n))
                .context("count must be 1..16")?;
            let mut argv = vec!["fork".into(), "-n".into(), count.to_string()];
            if args.get("checkpoint").is_some() {
                argv.extend(["--checkpoint".into(), string(args, "checkpoint")?]);
            }
            if args["start"] == false {
                argv.push("--no-start".into());
            }
            argv.push(string(args, "container")?);
            run(dir, &argv, Duration::from_secs(600))
        }
        "container_fork_rm" => {
            let selector = string(args, "selector")?;
            if args["confirm"].as_str() != Some(selector.as_str()) {
                bail!("container_fork_rm requires confirm equal to selector")
            }
            run(
                dir,
                &["fork".into(), "rm".into(), selector],
                Duration::from_secs(300),
            )
        }
        "container_exec" => {
            let mut argv = vec!["exec".into(), string(args, "container")?];
            let command = strings(args, "command")?;
            if command.is_empty() {
                bail!("command must not be empty")
            }
            argv.extend(command);
            run(dir, &argv, Duration::from_secs(600))
        }
        "image_pull" => run(
            dir,
            &["pull".into(), string(args, "image")?],
            Duration::from_secs(1800),
        ),
        "container_run" => {
            let mut argv: Vec<String> = vec!["run".into(), "-d".into()];
            if let Some(n) = args["name"].as_str() {
                argv.extend(["--name".into(), n.into()]);
            }
            for e in strings(args, "env")? {
                if !e.contains('=') {
                    bail!("env entries must be KEY=VALUE")
                }
                argv.extend(["-e".into(), e]);
            }
            for p in strings(args, "ports")? {
                let p = if p.starts_with("127.0.0.1:") {
                    p
                } else {
                    format!("127.0.0.1:{p}")
                };
                argv.extend(["-p".into(), p]);
            }
            if let Some(c) = args["cpus"].as_u64() {
                argv.extend(["--cpus".into(), c.to_string()]);
            }
            if let Some(m) = args["memory_mib"].as_u64() {
                argv.extend(["--memory".into(), format!("{m}m")]);
            }
            if let Some(g) = args["gpus"].as_str() {
                argv.extend(["--gpus".into(), g.into()]);
            }
            if args["desktop"] == true {
                argv.push("--desktop".into());
            }
            argv.push("--".into());
            argv.push(string(args, "image")?);
            argv.extend(strings(args, "command")?);
            run(dir, &argv, Duration::from_secs(1800))
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
