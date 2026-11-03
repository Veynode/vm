use anyhow::{bail, Context, Result};
use std::path::PathBuf;
fn main() {
    // Image pulls (`image.pull`, `container.run`) are HTTPS from the daemon.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    if let Err(e) = run() {
        eprintln!("vm-control: {e:#}");
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    // Helpers the daemon runs as its own processes: a container's port
    // publisher, ssh's way to a macOS machine's relay, and a macOS Guest's
    // driver link and desktop methods.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["__proxy", id] => return vm_control::services::proxy::run(id),
        ["__guest_port", socket, port] => {
            return vm_control::services::macos_relay::proxy(socket, port)
        }
        ["__driver", vm] => std::process::exit(vm_control::services::macos_driver::run(vm)?),
        ["__desktop", vm, method, arguments] => {
            let value = vm_control::services::macos_desktop::run(vm, method, arguments)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
        _ => {}
    }
    let mut args = std::env::args().skip(1);
    let action=args.next().context("usage: vm-control daemon|mcp|call [--state-dir PATH] [--backend PATH] [--memory-budget-mib MIB] [--worker-key-event-log-dir DIR] [--allow-lifecycle] [--allow-execute] [--allow-files] [--allow-input] [--allow-browser] [--allow-clipboard] [--tools GROUP,...] [--vm ID] [METHOD JSON]")?;
    let mut dir = None;
    let mut backend = None;
    let mut memory_budget_mib = None;
    let mut worker_key_event_log_dir = None;
    let mut qemu_options = std::collections::BTreeMap::new();
    let mut lifecycle = false;
    let mut execute = false;
    let mut files = false;
    let mut input = false;
    let mut browser = false;
    let mut clipboard = false;
    let mut vm = None;
    let mut groups = vm_control::tool_groups::default();
    let mut positional = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => {
                dir = Some(PathBuf::from(
                    args.next().context("missing state directory")?,
                ))
            }
            "--backend" => {
                backend = Some(PathBuf::from(args.next().context("missing backend path")?))
            }
            "--worker-key-event-log-dir" => {
                if worker_key_event_log_dir
                    .replace(PathBuf::from(
                        args.next()
                            .context("missing worker key-event log directory")?,
                    ))
                    .is_some()
                {
                    bail!("--worker-key-event-log-dir given more than once")
                }
            }
            "--qemu-worker"
            | "--qemu-launcher"
            | "--qemu-library"
            | "--qemu-resources"
            | "--qemu-spice-library"
            | "--qemu-screenshot-helper"
            | "--qemu-console-broker"
            | "--qemu-console-helper"
            | "--qemu-venus-runtime" => {
                let path = PathBuf::from(args.next().context("missing QEMU runtime path")?)
                    .canonicalize()
                    .context("QEMU runtime path not found")?;
                qemu_options.insert(arg, path);
            }
            "--memory-budget-mib" => {
                memory_budget_mib = Some(
                    args.next()
                        .context("missing memory budget")?
                        .parse::<u64>()
                        .context("memory budget must be positive integer MiB")?,
                )
            }
            "--allow-lifecycle" => lifecycle = true,
            "--allow-execute" => execute = true,
            "--allow-files" => files = true,
            "--allow-input" => input = true,
            "--allow-browser" => browser = true,
            "--allow-clipboard" => clipboard = true,
            "--vm" => vm = Some(args.next().context("missing --vm id")?),
            "--tools" => {
                groups =
                    vm_control::tool_groups::parse(&args.next().context("missing --tools groups")?)?
            }
            _ => positional.push(arg),
        }
    }
    if let Some(path) = &worker_key_event_log_dir {
        if action != "daemon" {
            bail!("--worker-key-event-log-dir is a local daemon option only")
        }
        vm_control::validate_worker_key_event_log_dir(path)?;
    }
    if action != "daemon" && (memory_budget_mib.is_some() || !qemu_options.is_empty()) {
        bail!("--memory-budget-mib is a local daemon option only")
    }
    if action != "call" && !positional.is_empty() {
        bail!("unexpected arguments for {action}")
    }
    if action == "call" && positional.len() > 2 {
        bail!("call accepts exactly METHOD and optional JSON")
    }
    let dir = match dir {
        Some(dir) => dir,
        None => vm_control::home::state_dir()?,
    };
    if !dir.is_absolute() {
        bail!("state directory must be absolute")
    }
    match action.as_str() {
        "daemon" => {
            // Explicit flags override the locally registered runtime.json.
            // What runtime.json names is optional except the Apple
            // Virtualization worker: a missing piece turns its feature off
            // (said in `hello`) instead of stopping the daemon.
            let (runtime, mut notices) = vm_control::home::Runtime::load_with_notes()?;
            let memory_budget_mib = memory_budget_mib.or(runtime.memory_budget_mib);
            let backend = backend
                .or(runtime.backend)
                .context("no Apple Virtualization worker: the daemon requires --backend or \"backend\" (the vm-apple executable) in runtime.json")?;
            let backend = backend.canonicalize().with_context(|| {
                format!(
                    "the Apple Virtualization worker {} does not exist",
                    backend.display()
                )
            })?;
            if qemu_options.is_empty() {
                if let Some(q) = runtime.qemu {
                    let required = [
                        ("--qemu-worker", q.worker),
                        ("--qemu-launcher", q.launcher),
                        ("--qemu-library", q.library),
                        ("--qemu-resources", q.resources),
                    ];
                    let missing: Vec<String> = required
                        .iter()
                        .filter(|(_, path)| path.canonicalize().is_err())
                        .map(|(_, path)| path.display().to_string())
                        .collect();
                    if missing.is_empty() {
                        for (flag, path) in required {
                            qemu_options.insert(flag.to_owned(), path.canonicalize()?);
                        }
                        for (flag, path) in [
                            ("--qemu-spice-library", q.spice_library),
                            ("--qemu-screenshot-helper", q.screenshot_helper),
                            ("--qemu-console-broker", q.console_broker),
                            ("--qemu-console-helper", q.console_helper),
                            ("--qemu-venus-runtime", q.venus_runtime),
                        ] {
                            let Some(path) = path else { continue };
                            match path.canonicalize() {
                                Ok(path) => {
                                    qemu_options.insert(flag.to_owned(), path);
                                }
                                Err(_) => notices.push(format!(
                                    "{} is missing; QEMU runs without it",
                                    path.display()
                                )),
                            }
                        }
                    } else {
                        notices.push(format!(
                            "QEMU machines are off: the QEMU runtime is missing {}",
                            missing.join(", ")
                        ));
                    }
                }
            }
            let qemu = if qemu_options.is_empty() {
                None
            } else {
                Some(vm_control::backend::QemuRegistration {
                    worker: qemu_options
                        .remove("--qemu-worker")
                        .context("missing --qemu-worker")?,
                    launcher: qemu_options
                        .remove("--qemu-launcher")
                        .context("missing --qemu-launcher")?,
                    library: qemu_options
                        .remove("--qemu-library")
                        .context("missing --qemu-library")?,
                    resources: qemu_options
                        .remove("--qemu-resources")
                        .context("missing --qemu-resources")?,
                    spice_library: qemu_options.remove("--qemu-spice-library"),
                    screenshot_helper: qemu_options.remove("--qemu-screenshot-helper"),
                    console_broker: qemu_options.remove("--qemu-console-broker"),
                    console_helper: qemu_options.remove("--qemu-console-helper"),
                    venus_runtime: qemu_options.remove("--qemu-venus-runtime"),
                })
            };
            migrate_records(&dir);
            vm_control::serve_with_worker_key_event_log(
                dir,
                backend,
                memory_budget_mib,
                qemu,
                worker_key_event_log_dir,
                notices,
            )
        }
        "mcp" => vm_control::mcp_serve(
            &dir,
            &vm_control::McpOptions {
                lifecycle,
                execute,
                files,
                input,
                browser,
                clipboard,
                vm,
            },
            groups,
        ),
        "call" => {
            let method = positional.first().context("call requires METHOD [JSON]")?;
            let payload = positional
                .get(1)
                .map(|s| serde_json::from_str(s))
                .transpose()?
                .unwrap_or(serde_json::json!({}));
            println!(
                "{}",
                serde_json::to_string_pretty(&vm_control::call(&dir, method, payload)?)?
            );
            Ok(())
        }
        _ => bail!("unknown command"),
    }
}

/// Records older than this daemon are upgraded before it serves them, when
/// `state` is its home's: a daemon on another state directory leaves the
/// home's records to the daemon that serves them.
fn migrate_records(state: &std::path::Path) {
    let Ok(root) = vm_control::home::root() else {
        return;
    };
    if state != root.join("state") {
        return;
    }
    for problem in
        vm_control::services::records::migrate(&root.join("containers"), &root.join("machines"))
    {
        eprintln!("vm-control: record left as it is: {problem}");
    }
}
