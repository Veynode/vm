//! Local API 1, the daemon's interface for every client (`docs/api.md`).
//!
//! The daemon (`vm-control`) serves it on `<home>/state/control.sock`; the
//! CLI, the MCP server and the web UI are clients, and so is Veynode core's
//! plugin, which implements the same wire in TypeScript. This crate holds both
//! halves of the wire and nothing of the daemon, so a client links it alone.
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub mod client;
pub mod envelope;
pub mod frame;

pub use client::{Client, Event, Stream, StreamSender};
pub use envelope::{
    answer, classify, code_of, open, refusal, refuse, ApiError, ErrorCode, Gpu, Hello, Runtime,
};
pub use frame::Channel;

/// The API version this crate speaks.
pub const API: u64 = 1;
/// The most one request or answer line carries.
pub const MAX_LINE: usize = 1024 * 1024;
/// The most a `screenshot` or `driver_rpc` answer carries.
pub const MAX_LARGE_LINE: usize = 12 * 1024 * 1024;
/// The most one stream frame carries.
pub const FRAME_MAX: usize = 1024 * 1024;

/// The largest answer `method` may give.
pub fn line_limit(method: &str) -> usize {
    if method == "screenshot" || method == "driver_rpc" {
        MAX_LARGE_LINE
    } else {
        MAX_LINE
    }
}

/// The root every client and the daemon share: `VEYNODEVM_HOME`, else
/// `~/Library/Application Support/VeynodeVM`.
pub fn home() -> Result<PathBuf> {
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

/// The daemon's state directory under `home`.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join("state")
}

/// The daemon's socket under `home`.
pub fn socket(home: &Path) -> PathBuf {
    state_dir(home).join("control.sock")
}

/// The rule for VM, machine and container ids: 1–64 ASCII letters, digits,
/// `-` or `_`.
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        anyhow::bail!("id must be 1–64 ASCII letters, digits, '-' or '_'")
    }
    Ok(())
}
