//! `veynodevm desktop`: the native desktop console window (`desktop.open`,
//! `desktop.close`).
//!
//! AppKit is only used for desktop control. Apple Virtualization containers
//! show `VZVirtualMachineView` (2D virtio-gpu, USB keyboard and pointer); QEMU
//! GPU containers show the SPICE console helper, which sends input while its
//! window has focus and releases held keys when it loses focus. Closing the
//! window leaves the VM running.
use crate::api_call;
use anyhow::Result;
use serde_json::json;

pub fn open(name: &str) -> Result<i32> {
    api_call("desktop.open", json!({"name":name}))?;
    println!("desktop window opened for {name}");
    Ok(0)
}

pub fn close(name: &str) -> Result<i32> {
    api_call("desktop.close", json!({"name":name}))?;
    Ok(0)
}
