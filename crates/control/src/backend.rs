//! Trusted host registration is separate from remotely supplied VM configuration.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Command};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    #[default]
    AppleVirtualization,
    QemuHvf,
}
#[derive(Clone, Debug)]
pub struct QemuRegistration {
    pub worker: PathBuf,
    pub launcher: PathBuf,
    pub library: PathBuf,
    pub resources: PathBuf,
    pub spice_library: Option<PathBuf>,
    pub screenshot_helper: Option<PathBuf>,
    pub console_broker: Option<PathBuf>,
    pub console_helper: Option<PathBuf>,
}
impl QemuRegistration {
    pub fn has_screenshot(&self) -> bool {
        self.console_broker.is_some() || self.screenshot_helper.is_some()
    }
    pub fn validate(&self) -> Result<()> {
        if self.console_broker.is_some() != self.console_helper.is_some() {
            bail!("QEMU console broker and native helper must be registered together")
        }
        for path in [&self.worker, &self.launcher, &self.library]
            .into_iter()
            .chain(self.spice_library.iter())
            .chain(self.screenshot_helper.iter())
            .chain(self.console_broker.iter())
            .chain(self.console_helper.iter())
        {
            if !path.is_absolute() || !path.is_file() {
                bail!("QEMU runtime paths must be existing absolute files")
            }
        }
        if !self.resources.is_absolute() || !self.resources.is_dir() {
            bail!("QEMU resources must be an existing absolute directory")
        }
        Ok(())
    }
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.worker);
        command
            .args(["run", "--launcher"])
            .arg(&self.launcher)
            .arg("--library")
            .arg(&self.library)
            .arg("--resources")
            .arg(&self.resources);
        if let Some(path) = &self.spice_library {
            command.arg("--spice-library").arg(path);
        }
        if let Some(path) = &self.screenshot_helper {
            command.arg("--screenshot-helper").arg(path);
        }
        if let Some(path) = &self.console_broker {
            command.arg("--console-broker").arg(path);
        }
        if let Some(path) = &self.console_helper {
            command.arg("--console-helper").arg(path);
        }
        command
    }
}
pub fn capabilities(backend: Backend) -> Value {
    match backend {
        Backend::AppleVirtualization => {
            json!({"guest_os":["linux","macos"],"lifecycle":true,"native_window":true,"screenshot":true,"screenshot_requirements":"macOS Guest service --allow-observe with existing Guest Screen Recording permission; Apple Linux screenshot unsupported","input":false,"guest_integration":"enabled Linux or macOS Guest service over host-only vsock; query vm_guest_info for actual availability","command_execution":true,"command_execution_requirements":"MCP requires enabled Linux async jobs for cancellation; macOS service legacy exec is not exposed by current MCP execution API","gpu":"requires actual Guest measurements"})
        }
        Backend::QemuHvf => {
            json!({"guest_os":["linux"],"lifecycle":true,"native_window":false,"screenshot":false,"input":false,"guest_integration":false,"command_execution":false,"file_transfer":false,"guest_ready":false,"normal_shutdown":"QMP power button request only; requires Guest support; acknowledgement is not completion","storage":"RAM direct boot only in this backend revision","gpu":"experimental virtio-gpu-gl / virgl / ANGLE Metal; SPICE connection does not establish acceleration or release quality"})
        }
    }
}
pub fn guest_capabilities(backend: Backend, guest: &str) -> Value {
    let mut value = capabilities(backend);
    if backend == Backend::AppleVirtualization {
        value["screenshot"] = json!(guest == "macos");
        value["command_execution"] = json!(guest == "linux");
        value["async_commands"] = json!(guest == "linux");
        value["guest_integration"]=json!(format!("requires enabled {guest} Guest vsock service; vm_guest_info reports current permissions and capabilities"));
    }
    value
}
pub fn require_registration(registration: &Option<QemuRegistration>) -> Result<&QemuRegistration> {
    registration
        .as_ref()
        .context("QEMU/HVF backend not registered by the local daemon operator")
}
