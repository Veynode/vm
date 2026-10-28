//! The VeynodeVM home as this client sees it: where the daemon's socket and
//! log are, and the web UI it registered. Everything else under it is the
//! daemon's.
use anyhow::{bail, Context, Result};
use std::{
    fs::{self, DirBuilder},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub use veynodevm_api::home as root;

pub fn state_dir() -> Result<PathBuf> {
    Ok(veynodevm_api::state_dir(&root()?))
}

/// `path`, created private (0700) when missing; refused unless it is a
/// directory of this user's that nobody else can read.
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

/// The web UI's Node host registered in `runtime.json` (`"web"`).
pub fn web_server() -> Result<Option<PathBuf>> {
    let path = root()?.join("runtime.json");
    if !path.exists() {
        return Ok(None);
    }
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o022 != 0 {
        bail!(
            "{} must be a regular file writable only by this user",
            path.display()
        )
    }
    let runtime: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("cannot parse {}", path.display()))?;
    Ok(runtime["web"].as_str().map(PathBuf::from))
}

/// An executable on `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
}
