use anyhow::{bail, Context, Result};
use std::path::PathBuf;
fn main() {
    if let Err(e) = run() {
        eprintln!("vm-control: {e:#}");
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let action=args.next().context("usage: vm-control daemon|mcp|call --state-dir PATH [--backend PATH] [--memory-budget-mib MIB] [--allow-lifecycle] [--allow-execute] [--allow-files] [METHOD JSON]")?;
    let mut dir = None;
    let mut backend = None;
    let mut memory_budget_mib = None;
    let mut qemu_options = std::collections::BTreeMap::new();
    let mut lifecycle = false;
    let mut execute = false;
    let mut files = false;
    let mut input = false;
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
            "--qemu-worker"
            | "--qemu-launcher"
            | "--qemu-library"
            | "--qemu-resources"
            | "--qemu-spice-library"
            | "--qemu-screenshot-helper"
            | "--qemu-console-broker"
            | "--qemu-console-helper" => {
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
            _ => positional.push(arg),
        }
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
    let dir = dir
        .context("--state-dir is required (use a private directory with a short absolute path)")?;
    if !dir.is_absolute() {
        bail!("state directory must be absolute")
    }
    match action.as_str() {
        "daemon" => {
            let backend = backend
                .context("daemon requires --backend pointing to vm-apple executable")?
                .canonicalize()
                .context("backend executable not found")?;
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
                })
            };
            vm_control::serve_with_qemu(dir, backend, memory_budget_mib, qemu)
        }
        "mcp" => vm_control::mcp_with_input(&dir, lifecycle, execute, files, input),
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
