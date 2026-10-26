//! `veynodevm desktop`: the native desktop console window.
//!
//! AppKit is only used for desktop control. Apple Virtualization containers
//! show `VZVirtualMachineView` (2D virtio-gpu, USB keyboard and pointer); QEMU
//! GPU containers show the SPICE console helper, which sends input while its
//! window has focus and releases held keys when it loses focus. Closing the
//! window leaves the VM running.
use crate::{container, daemon_call};
use anyhow::{bail, Result};
use serde_json::json;

fn vm_id(name: &str) -> Result<String> {
    if let Ok(c) = container::find(name) {
        return Ok(c.vm_id);
    }
    // Plain VM ids registered with `veynodevm vm create` work too.
    daemon_call("status", json!({"id":name}))?;
    Ok(name.to_owned())
}

pub fn open(name: &str) -> Result<i32> {
    let id = vm_id(name)?;
    let status = daemon_call("status", json!({"id":id}))?;
    if matches!(status["state"].as_str(), Some("stopped" | "crashed") | None) {
        bail!("{name} is not running")
    }
    daemon_call("show_window", json!({"id":id}))?;
    println!("desktop window opened for {name}");
    Ok(0)
}

pub fn close(name: &str) -> Result<i32> {
    daemon_call("close_window", json!({"id":vm_id(name)?}))?;
    Ok(0)
}
