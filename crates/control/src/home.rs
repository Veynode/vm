//! Default product locations and the local runtime registration file.
//!
//! Everything lives under one private per-user root so that the daemon, CLI,
//! web UI and MCP adapters agree without repeating absolute paths:
//! `~/Library/Application Support/VeynodeVM` unless `VEYNODEVM_HOME` is set.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, DirBuilder},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const RUNTIME_FILE: &str = "runtime.json";

pub fn root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("VEYNODEVM_HOME") {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            bail!("VEYNODEVM_HOME must be absolute")
        }
        return Ok(dir);
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/VeynodeVM"))
}

/// Create (or verify) a private directory owned by this user.
pub fn private_dir(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        bail!(
            "{} must be a private directory owned by this user",
            path.display()
        )
    }
    Ok(path.to_path_buf())
}

pub fn state_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("state"))
}
pub fn images_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("images"))
}
pub fn containers_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("containers"))
}
/// Full-distribution machines (UEFI boot, installer ISO, persistent disk).
pub fn machines_dir() -> Result<PathBuf> {
    private_dir(&private_dir(&root()?)?.join("machines"))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boot {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// Kernel module squashfs matching `kernel`, attached read-only.
    pub modloop: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QemuPaths {
    pub worker: PathBuf,
    pub launcher: PathBuf,
    pub library: PathBuf,
    pub resources: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spice_library: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_helper: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_broker: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_helper: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venus_runtime: Option<PathBuf>,
}

/// Locally registered runtime pieces. Written by the trusted local operator
/// (for development: `scripts/install-dev-runtime.py`); never by MCP clients.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    /// Apple Virtualization worker executable (`vm-apple`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<PathBuf>,
    /// Kernel and initramfs used for OCI micro-VMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<Boot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu: Option<QemuPaths>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_budget_mib: Option<u64>,
    /// Default container DNS servers. Empty: the NAT gateway forwarder from
    /// DHCP, which cannot reach host resolvers bound only to loopback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<std::net::IpAddr>,
    /// Default container NIC MTU (576..9000); unset keeps 1500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// Web UI Node host (`web/server.mjs`, next to its built `dist/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<PathBuf>,
    /// Host encoder for Apple Virtualization Linux browser desktops
    /// (`native/tools/guest-video-stream.m`: guest NV12 -> VideoToolbox).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vz_desktop_stream: Option<PathBuf>,
    /// Host text recognition for `vm_screen_text` (`native/tools/vm-vision.swift`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<PathBuf>,
    /// Desktop and accessibility helper that macOS containers run over SSH
    /// (`native/tools/vm-macos-desktop.swift`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macos_desktop: Option<PathBuf>,
    /// `runtime/fetch-cua-driver.sh` output: `<platform>/<binary>` of the
    /// pinned cua-driver (`runtime/cua-driver.json`), which the daemon
    /// copies into Windows and macOS Guests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cua_driver: Option<PathBuf>,
}

impl Runtime {
    pub fn path() -> Result<PathBuf> {
        Ok(root()?.join(RUNTIME_FILE))
    }
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o022 != 0 {
            bail!(
                "{} must be a regular file writable only by this user",
                path.display()
            )
        }
        let bytes = fs::read(&path)?;
        if bytes.len() > 64 * 1024 {
            bail!("runtime.json exceeds size limit")
        }
        let runtime: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}", path.display()))?;
        runtime.validate()?;
        Ok(runtime)
    }
    pub fn validate(&self) -> Result<()> {
        let mut paths: Vec<&Path> = Vec::new();
        paths.extend(self.backend.as_deref());
        if let Some(boot) = &self.boot {
            paths.extend([
                boot.kernel.as_path(),
                boot.initrd.as_path(),
                boot.modloop.as_path(),
            ]);
        }
        if let Some(q) = &self.qemu {
            paths.extend([
                q.worker.as_path(),
                q.launcher.as_path(),
                q.library.as_path(),
                q.resources.as_path(),
            ]);
            for p in [
                &q.spice_library,
                &q.screenshot_helper,
                &q.console_broker,
                &q.console_helper,
                &q.venus_runtime,
            ]
            .into_iter()
            .flatten()
            {
                paths.push(p);
            }
        }
        paths.extend(self.web.as_deref());
        paths.extend(self.vz_desktop_stream.as_deref());
        paths.extend(self.vision.as_deref());
        paths.extend(self.macos_desktop.as_deref());
        paths.extend(self.cua_driver.as_deref());
        for p in paths {
            if !p.is_absolute() {
                bail!("runtime paths must be absolute: {}", p.display())
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_runtime_paths_are_rejected() {
        let runtime: Runtime = serde_json::from_value(serde_json::json!({
            "backend": "vm-apple"
        }))
        .unwrap();
        assert!(runtime.validate().is_err());
        let runtime: Runtime = serde_json::from_value(serde_json::json!({
            "backend": "/opt/vm-apple",
            "boot": {"kernel": "/k", "initrd": "/i", "modloop": "/m"}
        }))
        .unwrap();
        runtime.validate().unwrap();
    }
    #[test]
    fn unknown_runtime_fields_are_rejected() {
        assert!(serde_json::from_value::<Runtime>(serde_json::json!({"backnd": "/x"})).is_err());
    }
}
