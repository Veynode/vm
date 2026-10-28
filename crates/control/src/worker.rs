//! Starting a VM's worker: its arguments and the key event log directory.
use super::*;

pub(crate) type FileSessionRoutes = BTreeMap<String, (std::time::Instant, BTreeSet<String>)>;

/// Test-input diagnostics only: never create or repair the operator's logging directory.
///
/// Returns the canonical path, which is the only path ever handed to the worker. The worker re-resolves the path at
/// open time, so every ancestor must be unable to be renamed or replaced by another local user: owned by root or the
/// current user and not group/other writable (a sticky bit is accepted). The directory itself must be 0700, owned by
/// the current user and carry no extended ACL: an inherited ACL entry would otherwise let another account read or
/// alter the key log although the mode says 0700.
pub fn validate_worker_key_event_log_dir(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("worker key-event log directory must be absolute")
    }
    // Strip trailing separators / `.` so lstat cannot follow a final symlink that way.
    let literal: PathBuf = path.components().collect();
    let metadata = fs::symlink_metadata(&literal).with_context(|| {
        format!(
            "worker key-event log directory must exist: {}",
            literal.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "worker key-event log directory must be a directory, not a symlink: {}",
            literal.display()
        )
    }
    let canonical = fs::canonicalize(&literal).with_context(|| {
        format!(
            "worker key-event log directory cannot be resolved: {}",
            literal.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&canonical)?;
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        bail!(
            "worker key-event log directory must be owned by the current user: {}",
            canonical.display()
        )
    }
    if metadata.mode() & 0o7777 != 0o700 {
        bail!(
            "worker key-event log directory must have mode 0700: {}",
            canonical.display()
        )
    }
    if has_extended_acl(&canonical)? {
        bail!(
            "worker key-event log directory must not carry an extended ACL: {}",
            canonical.display()
        )
    }
    for ancestor in canonical.ancestors().skip(1) {
        let meta = fs::symlink_metadata(ancestor)
            .with_context(|| format!("cannot inspect {}", ancestor.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            bail!(
                "worker key-event log path has a non-directory ancestor: {}",
                ancestor.display()
            )
        }
        if meta.uid() != 0 && meta.uid() != euid {
            bail!(
                "worker key-event log path has an ancestor owned by another user: {}",
                ancestor.display()
            )
        }
        let mode = meta.mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            bail!(
                "worker key-event log path has a group/other-writable ancestor without the sticky bit: {}",
                ancestor.display()
            )
        }
    }
    Ok(canonical)
}

/// True when the path carries an extended ACL (macOS `acl_get_file(ACL_TYPE_EXTENDED)`: NULL with ENOENT when there is
/// none). ACLs are not visible through `listxattr`. Other targets have no such ACL model here and report false (this
/// product runs the macOS worker only).
#[cfg(target_os = "macos")]
pub(crate) fn has_extended_acl(path: &Path) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn acl_get_file(path: *const libc::c_char, acl_type: libc::c_uint) -> *mut libc::c_void;
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    }
    const ACL_TYPE_EXTENDED: libc::c_uint = 0x0000_0100;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let acl = unsafe { acl_get_file(c.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err(error).with_context(|| format!("cannot read the ACL of {}", path.display())),
        };
    }
    unsafe { acl_free(acl) };
    Ok(true)
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn has_extended_acl(_path: &Path) -> Result<bool> {
    Ok(false)
}

pub(crate) fn worker_start_args(
    backend: Backend,
    config_path: &Path,
    log_dir: Option<&Path>,
    id: &str,
    unix_seconds: u64,
    worker_instance: &str,
) -> (Vec<OsString>, Option<PathBuf>) {
    let mut args = Vec::new();
    let log_path = if backend == Backend::AppleVirtualization {
        args.push("run".into());
        log_dir.map(|dir| {
            let prefix: String = worker_instance.chars().take(8).collect();
            dir.join(format!("key-events-{id}-{unix_seconds}-{prefix}.jsonl"))
        })
    } else {
        None
    };
    args.push("--config".into());
    args.push(config_path.as_os_str().into());
    if let Some(path) = &log_path {
        args.push("--log-key-events".into());
        args.push(path.as_os_str().into());
    }
    (args, log_path)
}
