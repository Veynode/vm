pub mod action_result;
pub mod agent_view;
pub mod api_server;
#[cfg(test)]
mod api_server_tests;
pub mod backend;
pub mod bridge;
mod container_state;
pub mod container_tools;
pub mod control_lease;
pub mod cua_bridge;
pub mod desktop_input;
pub mod desktop_tools;
pub mod driver_link;
mod events;
pub mod home;
pub mod input;
mod jobs;
pub mod mcp_events;
mod native_control;
pub mod native_input;
pub mod qemu_audio;
pub mod qemu_disk;
pub mod qemu_driver_channel;
pub mod qemu_extras;
pub mod qemu_graphics;
pub mod qemu_snapshot;
pub mod qemu_uefi;
mod resources;
pub mod screen_text;
pub mod screen_wait;
pub mod screenshot_scaled;
pub mod services;
#[cfg(test)]
mod services_tests;
pub mod snapshot;
mod snapshot_control;
mod snapshot_restore;
pub use veynodevm_mcp::tool_groups;
#[cfg(test)]
mod tool_groups_tests;
mod windows;
pub mod windows_driver;
pub mod windows_guest;
use anyhow::{bail, Context, Result};
use backend::{Backend, QemuRegistration};
use resources::{DiskIdentity, MemoryBudget};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

mod capabilities;
mod config;
mod daemon_log;
mod exec;
mod manager;
mod mcp;
mod pending;
mod registry;
mod running;
mod server;
mod wire;
mod worker;
pub use capabilities::*;
pub use config::*;
pub use daemon_log::*;
use exec::*;
use manager::*;
pub use mcp::*;
use pending::*;
pub use registry::*;
use running::*;
pub use server::*;
pub use wire::*;
pub use worker::*;

#[cfg(test)]
#[path = "../tests/worker_key_event_log/unit.rs"]
mod worker_key_event_log_tests;

#[cfg(test)]
mod cua_bridge_tests;
#[cfg(test)]
mod mcp_cleanup_tests;
#[cfg(test)]
mod registry_load_tests;

/// Unit fixtures entering the private Manager directly must satisfy the same
/// canonical0700 state-directory contract as serve_with_worker_key_event_log.
#[cfg(test)]
pub(crate) fn test_private_state(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap();
    let metadata = fs::symlink_metadata(&canonical).unwrap();
    assert!(metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() });
    assert_ne!(canonical, Path::new("/"));
    fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700)).unwrap();
    canonical
}
#[cfg(test)]
mod capabilities_tests;
#[cfg(test)]
mod daemon_log_tests;
#[cfg(test)]
mod screenshot_limits_tests;
#[cfg(test)]
mod screenshot_scaled_tests;
#[cfg(test)]
mod screenshot_test_support;
#[cfg(test)]
mod tests;
