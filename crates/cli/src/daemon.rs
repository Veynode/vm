//! Per-user control daemon management. `daemon start` registers a launchd
//! user agent so the daemon (and the VMs it owns) outlive the terminal.
use crate::home;
use crate::{daemon_call, print_json, sibling};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

pub const LABEL: &str = "dev.veynode.veynodevm";

/// This home's launchd label. The default home keeps `LABEL`; another
/// `VEYNODEVM_HOME` gets its own, so starting or stopping a test home's daemon
/// never boots out the default one (and the VMs it owns).
fn label() -> Result<String> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(label_for(
        &home::root()?,
        &PathBuf::from(home).join("Library/Application Support/VeynodeVM"),
    ))
}

fn label_for(root: &Path, default: &Path) -> String {
    if root == default {
        return LABEL.to_owned();
    }
    // FNV-1a: stable across builds, unlike std's hasher.
    let hash = root
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    format!("{LABEL}.{hash:016x}")
}

fn plist_path(label: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The daemon a LaunchAgent written by `plist` runs: its first program argument.
fn program(plist: &str) -> Option<String> {
    let arguments = &plist[plist.find("<key>ProgramArguments</key>")?..];
    let first = &arguments[arguments.find("<string>")? + "<string>".len()..];
    let value = &first[..first.find("</string>")?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

pub fn plist(label: &str, control: &str, log: &str, home_dir: Option<&str>) -> String {
    let environment = home_dir
        .map(|h| format!("  <key>EnvironmentVariables</key>\n  <dict><key>VEYNODEVM_HOME</key><string>{}</string></dict>\n", escape(h)))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{}</string>
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
        escape(label),
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
    let label = label()?;
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
            let control = sibling("vm-control")?;
            let path = plist_path(&label)?;
            // The agent names the daemon it runs; one from another build (an
            // older release, a development checkout) is replaced, which stops
            // that daemon and the machines it runs.
            let registered = std::fs::read_to_string(&path).ok();
            let current = registered.as_deref().and_then(program);
            let ours = current.as_deref() == Some(&*control.to_string_lossy());
            if running() && (ours || registered.is_none()) {
                println!("daemon already running");
                return Ok(0);
            }
            if let Some(previous) = current.filter(|_| !ours) {
                println!("replacing the daemon at {previous}");
            }
            let root = home::private_dir(&home::root()?)?;
            std::fs::create_dir_all(path.parent().unwrap())?;
            let custom_home = std::env::var("VEYNODEVM_HOME").ok();
            std::fs::write(
                &path,
                plist(
                    &label,
                    &control.to_string_lossy(),
                    &root.join("daemon.log").to_string_lossy(),
                    custom_home.as_deref(),
                ),
            )?;
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{label}")])
                .output();
            wait(false)?;
            let status = Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{uid}")])
                .arg(&path)
                .status()?;
            if !status.success() {
                bail!("launchctl bootstrap failed")
            }
            wait(true)?;
            println!("daemon started ({label})");
            Ok(0)
        }
        "stop" => {
            let output = Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{label}")])
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
                    "label": label,
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
                    &json!({"running": false, "label": label, "log": home::root()?.join("daemon.log"), "host_cpus": host_cpus()}),
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
        let p = super::plist(super::LABEL, "/a&b/vm-control", "/l<og", Some("/h"));
        assert!(
            p.contains("/a&amp;b/vm-control")
                && p.contains("/l&lt;og")
                && p.contains("VEYNODEVM_HOME")
        );
    }

    #[test]
    fn the_agent_names_its_daemon() {
        let p = super::plist(super::LABEL, "/v/0.1.0/a&b/vm-control", "/log", None);
        assert_eq!(
            super::program(&p).as_deref(),
            Some("/v/0.1.0/a&b/vm-control")
        );
        assert_eq!(super::program("<plist/>"), None);
    }

    #[test]
    fn each_home_has_its_own_label() {
        use std::path::Path;
        let default = Path::new("/Users/u/Library/Application Support/VeynodeVM");
        assert_eq!(super::label_for(default, default), super::LABEL);
        let a = super::label_for(Path::new("/tmp/a"), default);
        let b = super::label_for(Path::new("/tmp/b"), default);
        assert!(a.starts_with("dev.veynode.veynodevm.") && a != b && a != super::LABEL);
        assert_eq!(a, super::label_for(Path::new("/tmp/a"), default));
    }
}
