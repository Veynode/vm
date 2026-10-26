//! `veynodevm __desktop VM METHOD ARGUMENTS`: the desktop and accessibility
//! methods of a macOS container, for the daemon (MCP `vm_desktop_state`,
//! `vm_accessibility_tree`, ...). They run in the Guest as the
//! `vm-macos-desktop` helper (`native/tools/vm-macos-desktop.swift`) over SSH,
//! whose sessions the first boot grants Accessibility. The helper is copied
//! to `~/.veynodevm/` on first use and again whenever the host's copy changes.
use crate::{
    container::{self, Container},
    macos::{ensure_ready, guest_ip, remote},
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{fs, path::PathBuf, time::Duration};
use vm_control::home;

// Exit status of the run script when this version of the helper is missing.
const MISSING: i32 = 97;

fn helper() -> Result<PathBuf> {
    if let Some(path) = home::Runtime::load()?.macos_desktop {
        return Ok(path);
    }
    let sibling = std::env::current_exe()?.with_file_name("vm-macos-desktop");
    if sibling.is_file() {
        return Ok(sibling);
    }
    bail!("unsupported: macOS desktop helper not registered (build native/tools/build-vm-macos-desktop.sh, then scripts/install-dev-runtime.py)")
}

/// FNV-1a of the helper, naming its version in the Guest.
fn version(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    });
    format!("{hash:016x}")
}

pub(crate) fn container_of(vm: &str) -> Result<Container> {
    container::list()?
        .into_iter()
        .find(|c| c.vm_id == vm && c.os == "darwin")
        .with_context(|| format!("{vm} is not a macOS container"))
}

pub fn run(vm: &str, method: &str, arguments: &str) -> Result<Value> {
    let container = container_of(vm)?;
    let binary = fs::read(helper()?)?;
    let path = format!(".veynodevm/desktop-{}", version(&binary));
    let script = format!("test -x {path} || exit {MISSING}; exec ./{path} {method}");
    // The Guest is normally up; finish its first boot only when SSH fails.
    let mut ip = guest_ip(&container, Duration::from_secs(5))?;
    let mut attempt = remote(&container, &ip, &script, Some(arguments.as_bytes()))?;
    if attempt.0 == 255 {
        ip = ensure_ready(&container, true)?;
        attempt = remote(&container, &ip, &script, Some(arguments.as_bytes()))?;
    }
    if attempt.0 == MISSING {
        let install = format!(
            // Older versions go; the login shell (zsh) refuses an unmatched glob.
            "mkdir -p .veynodevm && find .veynodevm -maxdepth 1 -name 'desktop-*' -delete && cat > {path}.tmp && chmod 755 {path}.tmp && mv {path}.tmp {path}"
        );
        let (code, _, err) = remote(&container, &ip, &install, Some(&binary))?;
        if code != 0 {
            bail!(
                "installing the desktop helper in the Guest failed: {}",
                String::from_utf8_lossy(&err).trim()
            )
        }
        attempt = remote(&container, &ip, &script, Some(arguments.as_bytes()))?;
    }
    let (code, out, err) = attempt;
    let value: Value = serde_json::from_slice(&out).with_context(|| {
        format!(
            "desktop helper exited {code}: {}",
            String::from_utf8_lossy(&err).trim()
        )
    })?;
    if let Some(error) = value["error"].as_str() {
        bail!("{error}")
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_versions_follow_its_bytes() {
        assert_eq!(version(b""), "cbf29ce484222325");
        assert_eq!(version(b"a"), "af63dc4c8601ec8c");
        assert_ne!(version(b"helper 1"), version(b"helper 2"));
    }
}
