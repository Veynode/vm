//! cua-driver on Windows guests (docs/computer-use.md): the daemon installs
//! the pinned binary over the machine's SSH login on first use
//! (`guest/windows/cua.ps1`: copy, telemetry off, a hidden logon task in the
//! desktop session) and then keeps one SSH session per VM running
//! `cua-driver mcp --socket`, the driver's stdio proxy to that daemon, as a
//! `driver_link::Link`.
use crate::{driver_link, windows_guest::Ssh};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const CUA: &str = include_str!("../../../guest/windows/cua.ps1");
const PLATFORM: &str = "windows-arm64";
pub const EXE: &str = r"C:\Program Files\VeynodeVM\cua\cua-driver.exe";
const STAGED: &str = "C:/ProgramData/VeynodeVM/cua/cua-driver.exe.new";

/// The proxy's own environment: no telemetry or update check from the SSH
/// side either.
fn proxy_command() -> String {
    format!(
        r"$env:DO_NOT_TRACK='1'; $env:CUA_TELEMETRY='0'; $env:CUA_DRIVER_RS_TELEMETRY_ENABLED='0'; $env:CUA_DRIVER_RS_UPDATE_CHECK='0'; & '{EXE}' mcp --socket '\\.\pipe\cua-driver'"
    )
}

fn script(ssh: &Ssh, request: Value, timeout: Duration) -> Result<Value> {
    let mut input = serde_json::to_vec(&request)?;
    input.push(b'\n');
    let (code, out, err) = ssh.powershell(CUA, &input, timeout)?;
    let reply: Value = serde_json::from_slice(&out).with_context(|| {
        format!(
            "cua.ps1 exited {code}: {}",
            String::from_utf8_lossy(&err).trim()
        )
    })?;
    if let Some(error) = reply["error"].as_str() {
        bail!("driver_unavailable: {error}")
    }
    Ok(reply)
}

/// Installs or updates the pinned driver and starts its daemon when needed.
pub fn ensure(ssh: &Ssh) -> Result<()> {
    let (binary, sha256) = driver_link::host_binary(PLATFORM)?;
    let status = script(
        ssh,
        json!({"action":"status","sha256":sha256}),
        Duration::from_secs(30),
    )?;
    if status["installed"] == true && status["running"] == true {
        return Ok(());
    }
    if status["installed"] != true {
        script(ssh, json!({"action":"prepare"}), Duration::from_secs(30))?;
        ssh.copy(&binary, STAGED, Duration::from_secs(300))?;
    }
    script(
        ssh,
        json!({"action":"install","sha256":sha256}),
        Duration::from_secs(60),
    )?;
    Ok(())
}

/// The VM's driver link; each (re)connect first checks the install.
pub fn link(ssh: Ssh) -> driver_link::Link {
    driver_link::Link::new(Box::new(move || {
        ensure(&ssh)?;
        Ok(ssh.stdio(&proxy_command()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_proxy_runs_the_installed_driver_without_telemetry() {
        let command = proxy_command();
        assert!(command.contains(
            r"& 'C:\Program Files\VeynodeVM\cua\cua-driver.exe' mcp --socket '\\.\pipe\cua-driver'"
        ));
        for opt_out in [
            "DO_NOT_TRACK='1'",
            "CUA_TELEMETRY='0'",
            "CUA_DRIVER_RS_TELEMETRY_ENABLED='0'",
            "CUA_DRIVER_RS_UPDATE_CHECK='0'",
        ] {
            assert!(command.contains(opt_out), "{opt_out}");
        }
        // cua.ps1 installs where the proxy looks.
        assert!(CUA.contains(r"Join-Path $env:ProgramFiles 'VeynodeVM\cua'"));
        assert!(
            CUA.contains(r"Join-Path $env:ProgramData 'VeynodeVM\cua'")
                && STAGED.ends_with("VeynodeVM/cua/cua-driver.exe.new")
        );
        assert!(
            CUA.contains("serve --no-overlay")
                && CUA.contains("<RunLevel>HighestAvailable</RunLevel>")
                && CUA.contains("<Hidden>true</Hidden>")
        );
    }
}
