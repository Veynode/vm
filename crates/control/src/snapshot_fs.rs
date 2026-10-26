//! Descriptor-relative storage primitives; never recursively remove a path.
use anyhow::{bail, ensure, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString},
    fs::{File, Metadata},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub(crate) const MAX_ARTIFACT: u64 = 16 * 1024 * 1024 * 1024 * 1024;
const JSON_LIMIT: u64 = 65536;
pub(crate) struct Dir {
    pub file: File,
    pub path: PathBuf,
    // Only product-owned private boundaries are retained; ordinary ancestry
    // such as the account home may legitimately carry a deny-delete ACL.
    parent: Option<Arc<Dir>>,
}
pub(crate) struct Lock {
    file: File,
}
impl Lock {
    pub fn verify(&self, directory: &Dir) -> Result<()> {
        directory
            .verify_file(".lock", &self.file)
            .context("snapshot lock path changed; refuse concurrent operations")
    }
}
struct Entries(*mut libc::DIR);
impl Drop for Entries {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}
fn errno_pointer() -> *mut libc::c_int {
    #[cfg(target_os = "macos")]
    {
        unsafe { libc::__error() }
    }
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::__errno_location() }
    }
}

fn owned(fd: i32) -> Result<File> {
    if fd < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn c(name: &str) -> Result<CString> {
    ensure!(
        !name.is_empty() && !name.contains('/') && name != "." && name != "..",
        "invalid store leaf"
    );
    Ok(CString::new(name)?)
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}
/// Permissions bits alone do not describe Darwin's extended ACL grants. Query
/// the already pinned descriptor; never resolve a second pathname for its ACL.
#[cfg(target_os = "macos")]
pub(crate) fn reject_extended_acl(file: &File) -> Result<()> {
    extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_uint) -> *mut libc::c_void;
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    }
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x0000_0100) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(libc::ENOENT),
            "cannot inspect snapshot descriptor extended ACL: {error}"
        );
        return Ok(());
    }
    unsafe {
        acl_free(acl);
    }
    bail!("snapshot descriptor carries an extended ACL; refuse without changing permissions")
}
#[cfg(target_os = "linux")]
pub(crate) fn reject_extended_acl(file: &File) -> Result<()> {
    // Linux is not a first-release host. This checks POSIX ACL xattrs only;
    // Darwin's ACL behavior and tests are not evidence for other ACL models.
    for name in [c"system.posix_acl_access", c"system.posix_acl_default"] {
        let rc =
            unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if rc >= 0 {
            bail!("snapshot descriptor carries an extended POSIX ACL");
        }
        let error = std::io::Error::last_os_error();
        ensure!(
            matches!(
                error.raw_os_error(),
                Some(libc::ENODATA) | Some(libc::ENOTSUP)
            ),
            "cannot inspect snapshot descriptor POSIX ACL: {error}"
        );
    }
    Ok(())
}
fn private(file: &File, directory: bool) -> Result<()> {
    let m = file.metadata()?;
    ensure!(
        if directory { m.is_dir() } else { m.is_file() },
        "unexpected snapshot file type"
    );
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "snapshot store must be owned by current user and private"
    );
    if !directory {
        ensure!(m.nlink() == 1, "snapshot artifact has hard-link aliases");
    }
    reject_extended_acl(file)?;
    ensure!(
        same(&m, &file.metadata()?),
        "snapshot ownership/ACL metadata changed during validation"
    );
    Ok(())
}
fn absolute_dir(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "snapshot state path must be absolute");
    let mut dir = owned(unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    for part in path.components() {
        match part {
            Component::RootDir => (),
            Component::Normal(name) => {
                let name = CString::new(name.as_bytes())?;
                dir = owned(unsafe {
                    libc::openat(
                        dir.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                })?;
            }
            _ => bail!("snapshot paths must be normalized without dot components"),
        }
    }
    Ok(dir)
}
impl Dir {
    pub fn open(path: &Path) -> Result<Self> {
        let file = absolute_dir(path)?;
        private(&file, true)?;
        Ok(Self {
            file,
            path: path.into(),
            parent: None,
        })
    }
    pub fn verify(&self) -> Result<()> {
        if let Some(parent) = &self.parent {
            parent.verify()?;
        }
        let current = absolute_dir(&self.path)?;
        let a = self.file.metadata()?;
        let b = current.metadata()?;
        private(&self.file, true)?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino(),
            "snapshot directory path replaced"
        );
        Ok(())
    }
    fn retained(&self) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
            parent: self.parent.clone(),
        }))
    }
    pub fn child(&self, name: &str, create: bool) -> Result<(Self, bool)> {
        self.verify()?;
        let leaf = c(name)?;
        let mut created = false;
        if create {
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), leaf.as_ptr(), 0o700) } == 0 {
                created = true;
            } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let file = owned(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        private(&file, true)?;
        if created {
            self.file.sync_all()?;
        }
        Ok((
            Self {
                file,
                path: self.path.join(name),
                parent: Some(self.retained()?),
            },
            created,
        ))
    }
    pub fn file(&self, name: &str, create: bool) -> Result<File> {
        self.verify()?;
        let name = c(name)?;
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create {
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        let file =
            owned(unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, 0o600) })?;
        private(&file, false)?;
        Ok(file)
    }
    pub fn create_writeonly(&self, name: &str) -> Result<File> {
        self.verify()?;
        let name = c(name)?;
        let file = owned(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o600,
            )
        })?;
        private(&file, false)?;
        Ok(file)
    }
    #[cfg(target_os = "macos")]
    pub fn clone_disk(&self, source: &File, name: &str, expected_bytes: u64) -> Result<File> {
        self.verify()?;
        reject_extended_acl(source)?;
        let before = source.metadata()?;
        ensure!(
            before.is_file()
                && before.uid() == unsafe { libc::geteuid() }
                && before.len() == expected_bytes,
            "snapshot source raw disk identity/extent invalid"
        );
        let name = c(name)?;
        // CLONE_NOOWNERCOPY = 0x0002; the new inode belongs to this process.
        // fclonefileat never replaces an existing destination and does not copy ACLs.
        ensure!(
            unsafe {
                libc::fclonefileat(
                    source.as_raw_fd(),
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    0x0002,
                )
            } == 0,
            "snapshot APFS disk clone failed (no full-copy fallback): {}",
            std::io::Error::last_os_error()
        );
        let cloned = owned(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        })?;
        cloned.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let after = cloned.metadata()?;
        private(&cloned, false)?;
        ensure!(
            after.len() == expected_bytes
                && (after.dev(), after.ino()) != (before.dev(), before.ino()),
            "snapshot clone inode/extent invalid"
        );
        ensure!(
            same(&before, &source.metadata()?),
            "source disk changed while cloning; no snapshot published"
        );
        cloned.sync_all()?;
        self.file.sync_all()?;
        self.verify()?;
        Ok(cloned)
    }
    #[cfg(not(target_os = "macos"))]
    pub fn clone_disk(&self, _source: &File, _name: &str, _expected_bytes: u64) -> Result<File> {
        bail!("snapshot APFS disk cloning requires macOS; no full-copy fallback")
    }
    pub fn lock(&self) -> Result<Lock> {
        self.verify()?;
        let name = c".lock";
        let file = owned(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o600,
            )
        })?;
        private(&file, false)?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another snapshot operation owns this VM"
        );
        // Close-only Drop: never unlock an inherited open file description.
        let lock = Lock { file };
        lock.verify(self)?;
        Ok(lock)
    }
    pub fn names(&self) -> Result<Vec<String>> {
        self.verify()?;
        let before = self.file.metadata()?;
        // Enumerate the pinned directory itself. fs::read_dir(path) could be
        // redirected through a replaced ancestor between identity checks.
        let fd = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        ensure!(
            fd >= 0,
            "cannot duplicate snapshot directory: {}",
            std::io::Error::last_os_error()
        );
        let ptr = unsafe { libc::fdopendir(fd) };
        if ptr.is_null() {
            unsafe {
                libc::close(fd);
            }
            bail!(
                "cannot enumerate snapshot directory: {}",
                std::io::Error::last_os_error()
            );
        }
        let entries = Entries(ptr);
        unsafe {
            libc::rewinddir(entries.0);
        }
        let mut names = Vec::new();
        loop {
            unsafe {
                *errno_pointer() = 0;
            }
            let entry = unsafe { libc::readdir(entries.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                ensure!(
                    error.raw_os_error() == Some(0),
                    "snapshot directory read failed: {error}"
                );
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_str()
                .context("non-UTF8 snapshot leaf")?;
            if name == "." || name == ".." {
                continue;
            }
            ensure!(
                names.len() < 1024,
                "snapshot directory entry limit exceeded"
            );
            names.push(name.to_owned());
        }
        ensure!(
            same(&before, &self.file.metadata()?),
            "snapshot directory changed during enumeration"
        );
        self.verify()?;
        names.sort();
        Ok(names)
    }
    fn verify_file(&self, name: &str, expected: &File) -> Result<()> {
        let current = self.file(name, false)?;
        let a = current.metadata()?;
        let b = expected.metadata()?;
        private(expected, false)?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino(),
            "snapshot file path replaced"
        );
        Ok(())
    }
    fn verify_child(&self, name: &str, expected: &Dir) -> Result<()> {
        let (current, _) = self.child(name, false)?;
        let a = current.file.metadata()?;
        let b = expected.file.metadata()?;
        private(&expected.file, true)?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino(),
            "snapshot child directory replaced"
        );
        Ok(())
    }
    pub fn json<T: DeserializeOwned>(&self, name: &str) -> Result<T> {
        let file = self.file(name, false)?;
        ensure!(
            file.metadata()?.len() <= JSON_LIMIT,
            "snapshot metadata too large"
        );
        let mut bytes = Vec::new();
        (&file).take(JSON_LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= JSON_LIMIT, "snapshot metadata grew");
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub fn write_json(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        ensure!(
            bytes.len() as u64 <= JSON_LIMIT,
            "snapshot metadata too large"
        );
        let mut file = self.file(name, true)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        self.file.sync_all()?;
        Ok(())
    }
    pub fn reserve(&self, bytes: u64) -> Result<()> {
        let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        ensure!(
            unsafe { libc::fstatvfs(self.file.as_raw_fd(), value.as_mut_ptr()) } == 0,
            "cannot determine snapshot free space"
        );
        let v = unsafe { value.assume_init() };
        let free = (v.f_bavail as u64)
            .checked_mul(v.f_frsize)
            .context("free space overflow")?;
        let required = bytes
            .checked_add(64 * 1024 * 1024)
            .context("snapshot reservation overflow")?;
        ensure!(
            free >= required,
            "insufficient snapshot space (need {required}, available {free})"
        );
        Ok(())
    }
    pub fn rename_new(&self, old: &str, new: &str) -> Result<()> {
        self.verify()?;
        let old = c(old)?;
        let new = c(new)?;
        #[cfg(target_os = "macos")]
        let rc = unsafe {
            libc::renameatx_np(
                self.file.as_raw_fd(),
                old.as_ptr(),
                self.file.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(target_os = "linux")]
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                self.file.as_raw_fd(),
                old.as_ptr(),
                self.file.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            ) as i32
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let rc = -1;
        ensure!(
            rc == 0,
            "snapshot no-replace rename failed: {}",
            std::io::Error::last_os_error()
        );
        self.file.sync_all()?;
        Ok(())
    }
    pub fn rename_child_new(&self, old: &str, new: &str, expected: &Dir) -> Result<Dir> {
        self.verify_child(old, expected)?;
        self.rename_new(old, new)?;
        // If the source was replaced concurrently, retain the hidden directory
        // and fail before deleting anything. Never remove a different inode.
        self.verify_child(new, expected)?;
        let moved = Dir {
            file: expected.file.try_clone()?,
            path: self.path.join(new),
            parent: Some(self.retained()?),
        };
        moved.verify()?;
        Ok(moved)
    }
    pub fn unlink_file(&self, name: &str, expected: &File) -> Result<()> {
        self.verify_file(name, expected)?;
        self.unlink(name, false)
    }
    pub fn unlink_child(&self, name: &str, expected: &Dir) -> Result<()> {
        self.verify_child(name, expected)?;
        self.unlink(name, true)
    }
    fn unlink(&self, name: &str, directory: bool) -> Result<()> {
        let name = c(name)?;
        ensure!(
            unsafe {
                libc::unlinkat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            } == 0,
            "snapshot unlink failed: {}",
            std::io::Error::last_os_error()
        );
        self.file.sync_all()?;
        Ok(())
    }
}
pub(crate) fn nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|x| format!("{x:02x}")).collect())
}
pub(crate) fn hash(file: &File) -> Result<(u64, String)> {
    reject_extended_acl(file)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() > 0 && before.len() <= MAX_ARTIFACT,
        "snapshot artifact size/type invalid"
    );
    #[cfg(target_os = "macos")]
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) } == 0,
        "cannot disable snapshot read cache"
    );
    #[cfg(target_os = "macos")]
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_RDAHEAD, 0) } == 0,
        "cannot disable snapshot read-ahead"
    );
    #[repr(C, align(4096))]
    #[derive(Clone)]
    struct Block([u8; 4096]);
    // Page-aligned one MiB heap storage preserves F_NOCACHE's bounded-memory
    // path on macOS instead of populating the host cache for an entire VM disk.
    let mut blocks = vec![Block([0; 4096]); 256];
    let buffer =
        unsafe { std::slice::from_raw_parts_mut(blocks.as_mut_ptr().cast::<u8>(), 1024 * 1024) };
    let mut offset = 0;
    let mut h = Sha256::new();
    while offset < before.len() {
        let want = buffer.len().min((before.len() - offset) as usize);
        let n = file.read_at(&mut buffer[..want], offset)?;
        ensure!(n > 0, "snapshot artifact truncated while hashing");
        h.update(&buffer[..n]);
        offset += n as u64;
    }
    ensure!(
        same(&before, &file.metadata()?),
        "snapshot artifact changed while hashing"
    );
    reject_extended_acl(file)?;
    Ok((offset, format!("{:x}", h.finalize())))
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod acl_test_support {
    use super::*;
    const RULE: &str = "everyone allow read";
    /// Operates only on a fresh test leaf which had no ACL. Cleanup removes this
    /// exact rule, never clears another ACL or broadens a user's permissions.
    pub(crate) struct Entry {
        path: PathBuf,
        attrs: Attributes,
        bytes: Option<Vec<u8>>,
    }
    type Attributes = (u64, u64, u32, u32, u32, u64, u64, i64, i64, i64, i64);
    fn attributes(m: &Metadata) -> Attributes {
        (
            m.dev(),
            m.ino(),
            m.uid(),
            m.gid(),
            m.mode(),
            m.nlink(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }
    impl Entry {
        pub(crate) fn add(path: &Path) -> Self {
            let file = File::open(path).unwrap();
            reject_extended_acl(&file).unwrap();
            let before = file.metadata().unwrap();
            assert!(
                before.mode() & 0o077 == 0,
                "ACL test requires private fresh leaf"
            );
            let output = std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg(RULE)
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture ACL setup failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let after = file.metadata().unwrap();
            assert_eq!(before.mode(), after.mode());
            assert!(reject_extended_acl(&file).is_err());
            let bytes = after.is_file().then(|| std::fs::read(path).unwrap());
            Self {
                path: path.into(),
                attrs: attributes(&after),
                bytes,
            }
        }
        pub(crate) fn assert_unchanged(&self) {
            let file = File::open(&self.path).unwrap();
            assert_eq!(
                attributes(&file.metadata().unwrap()),
                self.attrs,
                "product mutated fixture ACL/metadata"
            );
            assert!(reject_extended_acl(&file)
                .unwrap_err()
                .to_string()
                .contains("extended ACL"));
            if let Some(bytes) = &self.bytes {
                assert_eq!(&std::fs::read(&self.path).unwrap(), bytes);
            }
        }
    }
    impl Drop for Entry {
        fn drop(&mut self) {
            let result = std::process::Command::new("/bin/chmod")
                .arg("-a")
                .arg(RULE)
                .arg(&self.path)
                .output();
            let success = result.as_ref().is_ok_and(|o| o.status.success());
            if !std::thread::panicking() {
                assert!(success, "exact fixture ACL cleanup failed");
            } else if !success {
                eprintln!("exact fixture ACL cleanup failed during test unwind");
            }
        }
    }
}
