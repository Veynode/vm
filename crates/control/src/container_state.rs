//! Warm container checkpoints on Apple Virtualization (`veynodevm checkpoint`).
//!
//! `machine_state_save` asks a running worker to pause, save RAM/CPU/device
//! state, clone the writable disk in the same paused interval and resume
//! (`native/worker/ContainerState.swift`). `start` with
//! `restore_machine_state` restores such a state instead of booting. Both are
//! limited to Apple Linux VMs with a persistent `machine_identifier_file` and a
//! fixed `mac_address`, and to files inside the VM's disk directory (the
//! container directory the CLI owns).
use crate::{Backend, Config};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const SAVE: &str = "machine_state_save";
pub const SAVE_TIMEOUT_SECONDS: u64 = 300;
/// Step the Guest clock to the host's after a restore (`guest/veynode-agent/clock.go`).
pub const CLOCK_SYNC: &str = "clock_sync";

pub fn clock_request(number: u64) -> Result<Value> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    Ok(
        serde_json::json!({"id":number.to_string(),"method":CLOCK_SYNC,"unix_ns":now.as_nanos() as u64}),
    )
}

fn eligible(config: &Config) -> Result<&Path> {
    if config.backend != Backend::AppleVirtualization
        || config.guest != "linux"
        || config.machine_identifier_file.is_none()
        || config.mac_address.is_none()
    {
        bail!("saved machine state needs an Apple Linux VM with machine_identifier_file and mac_address")
    }
    config
        .disk
        .as_deref()
        .and_then(Path::parent)
        .context("saved machine state needs a writable disk")
}

/// An absolute path strictly inside the VM's disk directory.
fn inside(config: &Config, value: &Value, key: &str) -> Result<PathBuf> {
    let root = eligible(config)?.canonicalize()?;
    let path = PathBuf::from(
        value
            .as_str()
            .with_context(|| format!("{key} must be a string"))?,
    );
    if !path.is_absolute() || path.components().any(|c| c.as_os_str() == "..") {
        bail!("{key} must be an absolute path without ..")
    }
    let parent = path
        .parent()
        .context("path has no parent")?
        .canonicalize()
        .with_context(|| format!("{key} directory"))?;
    if !parent.starts_with(&root) {
        bail!("{key} must be inside the VM's disk directory")
    }
    Ok(parent.join(path.file_name().context("path has no file name")?))
}

pub fn save_command(config: &Config, args: &Value, command: &mut Value) -> Result<()> {
    for key in ["path", "disk_clone"] {
        let path = inside(config, &args[key], key)?;
        if path.symlink_metadata().is_ok() {
            bail!("{key} already exists")
        }
        command[key] = Value::String(path.to_string_lossy().into_owned());
    }
    Ok(())
}

pub fn restore_path(config: &Config, value: &Value) -> Result<String> {
    let path = inside(config, value, "restore_machine_state")?;
    if !path.symlink_metadata().is_ok_and(|m| m.is_file()) {
        bail!("restore_machine_state must be an existing regular file")
    }
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(dir: &Path, extra: Value) -> Config {
        let disk = dir.join("rw.raw");
        std::fs::write(&disk, b"").unwrap();
        let mut value = json!({
            "backend":"apple-virtualization","id":"c-a","guest":"linux","cpus":1,"memory_mib":512,
            "kernel":"/k","disk":disk,"mac_address":"02:00:00:00:00:01",
            "machine_identifier_file":dir.join("machine-id.bin"),
        });
        for (k, v) in extra.as_object().unwrap() {
            value[k] = v.clone();
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn save_paths_stay_in_the_container_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::create_dir(dir.join("checkpoints")).unwrap();
        let c = config(dir, json!({}));
        let mut command = json!({});
        save_command(
            &c,
            &json!({"path":dir.join("checkpoints/m.vzstate"),"disk_clone":dir.join("checkpoints/rw.raw")}),
            &mut command,
        )
        .unwrap();
        assert!(command["path"]
            .as_str()
            .unwrap()
            .ends_with("checkpoints/m.vzstate"));
        for bad in [
            json!({"path":"/tmp/x","disk_clone":dir.join("checkpoints/rw.raw")}),
            json!({"path":dir.join("checkpoints/../../x"),"disk_clone":dir.join("checkpoints/rw.raw")}),
            json!({"path":"relative","disk_clone":dir.join("checkpoints/rw.raw")}),
            json!({"path":dir.join("rw.raw"),"disk_clone":dir.join("checkpoints/c.raw")}),
        ] {
            assert!(save_command(&c, &bad, &mut json!({})).is_err(), "{bad}");
        }
    }

    #[test]
    fn restore_needs_identity_and_an_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let state = dir.join("m.vzstate");
        let c = config(dir, json!({}));
        assert!(restore_path(&c, &json!(state)).is_err());
        std::fs::write(&state, b"x").unwrap();
        assert!(restore_path(&c, &json!(state)).is_ok());
        let no_mac = config(dir, json!({"mac_address":null}));
        assert!(restore_path(&no_mac, &json!(state)).is_err());
        let qemu = config(dir, json!({"backend":"qemu-hvf"}));
        assert!(restore_path(&qemu, &json!(state)).is_err());
    }
}
