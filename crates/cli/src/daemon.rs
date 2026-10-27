//! Per-user control daemon management. `daemon start` registers a launchd
//! user agent so the daemon (and the VMs it owns) outlive the terminal.
use crate::{daemon_call, print_json, sibling};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    os::unix::process::CommandExt,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
use vm_control::home;

pub const LABEL: &str = "dev.veynode.veynodevm";

fn plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn plist(control: &str, log: &str, home_dir: Option<&str>) -> String {
    let environment = home_dir
        .map(|h| format!("  <key>EnvironmentVariables</key>\n  <dict><key>VEYNODEVM_HOME</key><string>{}</string></dict>\n", escape(h)))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>daemon</string></array>
{environment}  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        escape(control),
        escape(log),
        escape(log)
    )
}

pub fn running() -> bool {
    daemon_call("capabilities", json!({})).is_ok()
}

fn wait(up: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if running() == up {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!(
        "daemon did not {} in time; see {}",
        if up { "start" } else { "stop" },
        home::root()?.join("daemon.log").display()
    )
}

pub fn command(rest: &[String], json_output: bool) -> Result<i32> {
    let uid = unsafe { libc::getuid() };
    match rest.first().map(String::as_str).unwrap_or("status") {
        "run" => {
            // Foreground, for development and debugging.
            let err = Command::new(sibling("vm-control")?)
                .arg("daemon")
                .args(&rest[1..])
                .exec();
            Err(err.into())
        }
        "start" => {
            if running() {
                println!("daemon already running");
                return Ok(0);
            }
            let root = home::private_dir(&home::root()?)?;
            let control = sibling("vm-control")?;
            let path = plist_path()?;
            std::fs::create_dir_all(path.parent().unwrap())?;
            let custom_home = std::env::var("VEYNODEVM_HOME").ok();
            std::fs::write(
                &path,
                plist(
                    &control.to_string_lossy(),
                    &root.join("daemon.log").to_string_lossy(),
                    custom_home.as_deref(),
                ),
            )?;
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{LABEL}")])
                .output();
            let status = Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{uid}")])
                .arg(&path)
                .status()?;
            if !status.success() {
                bail!("launchctl bootstrap failed")
            }
            wait(true)?;
            println!("daemon started ({LABEL})");
            Ok(0)
        }
        "stop" => {
            let output = Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{LABEL}")])
                .output()?;
            if !output.status.success() && running() {
                bail!("daemon is running outside launchd; stop that process directly")
            }
            wait(false)?;
            println!("daemon stopped");
            Ok(0)
        }
        "status" => match daemon_call("capabilities", json!({})) {
            Ok(c) if json_output => {
                print_json(&json!({
                    "running": true,
                    "state_dir": home::state_dir()?,
                    "log": home::root()?.join("daemon.log"),
                    "label": LABEL,
                    "memory_budget_mib": c["resource_admission"]["vm_memory_budget_mib"],
                    "assigned_memory_mib": c["resource_admission"]["active_assigned_memory_mib"],
                    "qemu_registered": c["backends"]["qemu-hvf"]["registered"],
                    "host_cpus": host_cpus(),
                }))?;
                Ok(0)
            }
            Ok(c) => {
                println!("running  state={}", home::state_dir()?.display());
                println!(
                    "memory budget {} MiB, assigned {} MiB; QEMU/GPU runtime registered: {}",
                    c["resource_admission"]["vm_memory_budget_mib"],
                    c["resource_admission"]["active_assigned_memory_mib"],
                    c["backends"]["qemu-hvf"]["registered"]
                );
                Ok(0)
            }
            Err(_) if json_output => {
                print_json(
                    &json!({"running": false, "label": LABEL, "log": home::root()?.join("daemon.log"), "host_cpus": host_cpus()}),
                )?;
                Ok(1)
            }
            Err(_) => {
                println!("not running (veynodevm daemon start)");
                Ok(1)
            }
        },
        other => bail!("unknown daemon command: {other}"),
    }
}

/// The cores a VM can be given at most, as the UI shows them.
fn host_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

#[cfg(test)]
mod tests {
    #[test]
    fn plist_escapes() {
        let p = super::plist("/a&b/vm-control", "/l<og", Some("/h"));
        assert!(
            p.contains("/a&amp;b/vm-control")
                && p.contains("/l&lt;og")
                && p.contains("VEYNODEVM_HOME")
        );
    }
}
