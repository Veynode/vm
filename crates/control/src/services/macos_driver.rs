//! `vm-control __driver VM`: cua-driver of a macOS container on this
//! process's stdio, for the daemon's driver link (`driver_link.rs`,
//! docs/computer-use.md). The pinned binary is copied to `~/.veynodevm/` on
//! first use, like the desktop helper, and runs embedded
//! (`CUA_DRIVER_EMBEDDED=1`) in the SSH session, so it uses the Accessibility
//! grant that session already has, never prompts and never relaunches as an
//! app; telemetry and update checks are off and there is no overlay.
use super::{
    macos::{ensure_ready, exec_remote, guest_ip, remote},
    macos_desktop::container_of,
};
use crate::driver_link;
use anyhow::{bail, Result};
use std::{fs, time::Duration};

const MISSING: i32 = 97;
const ENV: &str = "DO_NOT_TRACK=1 CUA_TELEMETRY=0 CUA_DRIVER_RS_TELEMETRY_ENABLED=0 CUA_DRIVER_RS_UPDATE_CHECK=0 CUA_DRIVER_EMBEDDED=1";

fn path(sha256: &str) -> String {
    format!(".veynodevm/cua-driver-{}", &sha256[..16])
}

pub fn run(vm: &str) -> Result<i32> {
    let container = container_of(vm)?;
    let (binary, sha256) = driver_link::host_binary("darwin-arm64")?;
    let path = path(&sha256);
    let check = format!("test -x {path} || exit {MISSING}");
    let mut ip = guest_ip(&container, Duration::from_secs(5))?;
    let mut code = remote(&container, &ip, &check, None)?.0;
    if code == 255 {
        ip = ensure_ready(&container, true)?;
        code = remote(&container, &ip, &check, None)?.0;
    }
    if code == MISSING {
        let install = format!(
            "mkdir -p .veynodevm && find .veynodevm -maxdepth 1 -name 'cua-driver-*' -delete && cat > {path}.tmp && chmod 755 {path}.tmp && mv {path}.tmp {path}"
        );
        let (code, _, err) = remote(&container, &ip, &install, Some(&fs::read(&binary)?))?;
        if code != 0 {
            bail!(
                "driver_unavailable: installing cua-driver in the Guest failed: {}",
                String::from_utf8_lossy(&err).trim()
            )
        }
    } else if code != 0 {
        bail!("driver_unavailable: the Guest refused the cua-driver check (exit {code})")
    }
    exec_remote(&container, &ip, &command(&path))?;
    unreachable!("exec_remote returns only on failure")
}

/// `--direct`: the MCP process owns the runtime. Embedded without it, the
/// driver requires a host-provided service socket instead.
fn command(path: &str) -> String {
    format!("{ENV} exec ./{path} mcp --direct --no-overlay")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_driver_runs_embedded_without_telemetry_or_overlay() {
        let path = path("92067848d0eeb9de76929fbd06a137af801b0ff6caedbb3109bd7a6cc84a1f9f");
        assert_eq!(path, ".veynodevm/cua-driver-92067848d0eeb9de");
        assert_eq!(
            command(&path),
            "DO_NOT_TRACK=1 CUA_TELEMETRY=0 CUA_DRIVER_RS_TELEMETRY_ENABLED=0 CUA_DRIVER_RS_UPDATE_CHECK=0 CUA_DRIVER_EMBEDDED=1 exec ./.veynodevm/cua-driver-92067848d0eeb9de mcp --direct --no-overlay"
        );
    }
}
