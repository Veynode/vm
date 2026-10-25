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

pub fn tools(lifecycle: bool) -> Vec<Value> {
    let container = json!({"type":"object","properties":{"container":{"type":"string","description":"Container name or id (prefix)"}},"required":["container"],"additionalProperties":false});
    let mut tools = vec![
        json!({"name":"container_list","description":"List micro-VM containers (all states) with image, status, published ports, GPU profile and the backing vm_id usable with vm_* tools (screenshot, exec, input).","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"container_inspect","description":"Container record, current state, guest IP, exit code, GPU device report and backing VM status.","inputSchema":container}),
        json!({"name":"container_logs","description":"Container output so far (console between start and exit markers), last 64 KiB.","inputSchema":container}),
        json!({"name":"image_list","description":"List local OCI images (linux/arm64).","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ];
    if lifecycle {
        tools.push(json!({"name":"image_pull","description":"Pull a linux/arm64 OCI image from a registry (anonymous) and prepare its read-only root.","inputSchema":{"type":"object","properties":{"image":{"type":"string"}},"required":["image"],"additionalProperties":false}}));
        tools.push(json!({"name":"container_run","description":"Create and start a detached container as its own micro-VM. Without gpus/desktop it runs on Apple Virtualization; gpus selects QEMU/HVF with virgl (GL) or experimental Venus (Vulkan via MoltenVK). Host bind mounts are not available through MCP.","inputSchema":{"type":"object","properties":{
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
            super::tools(l)
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert!(!names(false).contains(&"container_run".to_owned()));
        assert!(names(true).contains(&"container_run".to_owned()));
        assert!(names(false).contains(&"container_list".to_owned()));
    }
    #[test]
    fn option_injection_refused() {
        assert!(super::string(&serde_json::json!({"image":"--rm"}), "image").is_err());
    }
}
