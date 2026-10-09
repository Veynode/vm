//! Pinned Linux raw disk and cooperating VZ/image/QEMU inode lock.
//!
//! Only close descriptors on Drop: LOCK_UN would unlock the shared open-file
//! description in a still-running inherited backend after worker death.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::{CStr, CString},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt},
            process::CommandExt,
        },
    },
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Arc,
};

const DISK_SET: u64 = 1;
const LOCK_SET: u64 = 2;
const MAX_RAW_BYTES: u64 = 16 * 1024 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}
impl Identity {
    fn of(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
        })
    }
    fn matches(&self, st: &libc::stat) -> bool {
        self.device == st.st_dev as u64 && self.inode == st.st_ino
    }
}
struct Edge {
    parent: RawFd,
    name: CString,
    identity: Identity,
    kind: libc::mode_t,
}
#[derive(Default)]
struct Pins {
    directories: Vec<File>,
    edges: Vec<Edge>,
}
impl Pins {
    fn root(&mut self) -> Result<RawFd> {
        let fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        let f = owned(fd)?;
        let fd = f.as_raw_fd();
        self.directories.push(f);
        Ok(fd)
    }
    fn directory(&mut self, path: &Path) -> Result<RawFd> {
        ensure!(path.is_absolute(), "disk paths must be absolute");
        let mut parent = self.root()?;
        for part in path.components() {
            match part {
                Component::RootDir => (),
                Component::Normal(name) => {
                    parent =
                        self.child_directory(parent, &CString::new(name.as_bytes())?, false, None)?
                }
                _ => bail!("disk path must be normalized; dot components are not permitted"),
            }
        }
        Ok(parent)
    }
    fn child_directory(
        &mut self,
        parent: RawFd,
        name: &CStr,
        create: bool,
        forbidden_mode: Option<u32>,
    ) -> Result<RawFd> {
        if create
            && unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) } != 0
            && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
        {
            return Err(io::Error::last_os_error().into());
        }
        let f = owned(unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
        .context("disk directory is missing, replaced, or a symbolic link")?;
        let m = f.metadata()?;
        if let Some(mask) = forbidden_mode {
            ensure!(
                m.uid() == unsafe { libc::geteuid() } && m.mode() & mask == 0,
                "unsafe disk lock directory ownership or permissions"
            );
        }
        self.edges.push(Edge {
            parent,
            name: name.to_owned(),
            identity: Identity::of(&f)?,
            kind: libc::S_IFDIR,
        });
        let fd = f.as_raw_fd();
        self.directories.push(f);
        Ok(fd)
    }
    fn leaf(&mut self, parent: RawFd, name: &CStr, create: bool) -> Result<File> {
        let flags = libc::O_RDWR
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create { libc::O_CREAT } else { 0 };
        let original = owned(unsafe { libc::openat(parent, name.as_ptr(), flags, 0o600) })
            .context("cannot safely open regular raw disk or lock")?;
        ensure!(
            original.metadata()?.is_file(),
            "disk and sidecar must be regular files"
        );
        // QEMU add-fd forbids standard descriptors; never rely on caller stdio.
        let f = owned(unsafe { libc::fcntl(original.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) })?;
        self.edges.push(Edge {
            parent,
            name: name.to_owned(),
            identity: Identity::of(&f)?,
            kind: libc::S_IFREG,
        });
        Ok(f)
    }
    // Async-signal-safe: preallocated names/edges, libc fstatat only, no locks,
    // allocation, formatting, filesystem library calls or Rust destructors.
    fn verify_child(&self) -> io::Result<()> {
        for edge in &self.edges {
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    edge.parent,
                    edge.name.as_ptr(),
                    st.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let st = unsafe { st.assume_init() };
            if st.st_mode & libc::S_IFMT != edge.kind || !edge.identity.matches(&st) {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
        }
        Ok(())
    }
}
fn owned(fd: RawFd) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn home() -> Result<PathBuf> {
    let mut buffer = vec![0u8; 65536];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            pwd.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    ensure!(
        rc == 0 && !result.is_null(),
        "cannot resolve account home for shared disk lock"
    );
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr((*result).pw_dir) }.to_bytes(),
    )))
}
fn raw_file(path: &Path, pins: &mut Pins) -> Result<File> {
    ensure!(
        path.is_absolute() && path.extension().is_some_and(|e| e == "raw"),
        "QEMU disk must be an absolute .raw file, explicitly raw format"
    );
    let parent = pins.directory(path.parent().context("disk parent required")?)?;
    let file = pins.leaf(
        parent,
        &CString::new(
            path.file_name()
                .context("disk filename required")?
                .as_bytes(),
        )?,
        false,
    )?;
    let m = file.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
        "raw disk must belong to current user and not be group/other writable"
    );
    ensure!(
        (512..=MAX_RAW_BYTES).contains(&m.len()) && m.len() % 512 == 0,
        "raw disk size must be sector aligned, 512 bytes through 16 TiB"
    );
    let mut header = [0u8; 512];
    file.read_exact_at(&mut header, 0)?;
    let mut footer = [0u8; 512];
    file.read_exact_at(&mut footer, m.len() - 512)?;
    ensure!(
        ![
            b"QFI\xfb".as_slice(),
            b"QED\0",
            b"KDMV",
            b"vhdxfile",
            b"# Disk DescriptorFile",
            b"<<< Oracle VM VirtualBox Disk Image >>>",
            b"<<< Sun xVM VirtualBox Disk Image >>>"
        ]
        .iter()
        .any(|p| header.starts_with(p))
            && !footer.starts_with(b"conectix"),
        "recognized container format is not a raw disk"
    );
    pins.verify_child()?;
    Ok(file)
}
/// Registration validation does not acquire a runtime lock or create sidecars.
pub fn validate_path(path: &Path) -> Result<()> {
    raw_file(path, &mut Pins::default()).map(|_| ())
}

struct OwnedDisk {
    disk: File,
    lock: File,
    pins: Pins,
    identity: Identity,
    path: PathBuf,
}
/// The Command's pre-exec closure also retains these descriptors; keep the guard
/// in the worker until the exact QEMU child has been reaped for normal lifecycle.
pub struct DiskGuard {
    inner: Arc<OwnedDisk>,
}
impl DiskGuard {
    pub fn open(path: &Path) -> Result<Self> {
        let mut pins = Pins::default();
        let disk = raw_file(path, &mut pins)?;
        let support = pins.directory(&home()?.join("Library/Application Support"))?;
        let product = pins.child_directory(support, c"Veynode", true, Some(0o022))?;
        let locks = pins.child_directory(product, c"locks", true, Some(0o077))?;
        Self::finish(path, disk, pins, locks)
    }
    fn finish(path: &Path, disk: File, mut pins: Pins, locks: RawFd) -> Result<Self> {
        let identity = Identity::of(&disk)?;
        let name = CString::new(format!("{}-{}.lock", identity.device, identity.inode))?;
        let lock = pins.leaf(locks, &name, true)?;
        let m = lock.metadata()?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.nlink() == 1 && m.mode() & 0o077 == 0,
            "unsafe disk lock sidecar ownership, links, or mode"
        );
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("disk inode is busy (Veynode VZ/QEMU/image sidecar)");
        }
        pins.verify_child()
            .context("disk or lock path changed while locking")?;
        let canonical = fs::canonicalize(path)?;
        ensure!(
            canonical == path,
            "disk path must be canonical without symbolic-link aliases"
        );
        Ok(Self {
            inner: Arc::new(OwnedDisk {
                disk,
                lock,
                pins,
                identity,
                path: canonical,
            }),
        })
    }
    pub fn identity(&self) -> Identity {
        self.inner.identity
    }
    pub fn canonical_path(&self) -> &Path {
        &self.inner.path
    }
    pub fn verify_paths(&self) -> Result<()> {
        self.inner
            .pins
            .verify_child()
            .context("disk or sidecar path identity changed")
    }
    fn disk_opaque(&self) -> String {
        format!(
            "veynode-raw-disk:{}:{}",
            self.inner.identity.device, self.inner.identity.inode
        )
    }
    fn lock_opaque(&self) -> String {
        format!(
            "veynode-disk-lock:{}:{}",
            self.inner.identity.device, self.inner.identity.inode
        )
    }
    fn install_inheritance(&self, command: &mut Command) -> Result<()> {
        self.verify_paths()?;
        let owned = Arc::clone(&self.inner);
        unsafe {
            command.pre_exec(move || {
                owned.pins.verify_child()?;
                for fd in [owned.disk.as_raw_fd(), owned.lock.as_raw_fd()] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }
    pub fn configure_command(&self, command: &mut Command) -> Result<()> {
        self.install_inheritance(command)?;
        command.args([
            "-add-fd",
            &format!(
                "fd={},set={DISK_SET},opaque={}",
                self.inner.disk.as_raw_fd(),
                self.disk_opaque()
            ),
        ]);
        command.args([
            "-add-fd",
            &format!(
                "fd={},set={LOCK_SET},opaque={}",
                self.inner.lock.as_raw_fd(),
                self.lock_opaque()
            ),
        ]);
        command.arg("-blockdev").arg(serde_json::to_string(&json!({"driver":"file","node-name":"veynode-disk-file","filename":"/dev/fdset/1","locking":"on","cache":{"direct":false,"no-flush":false}}))?);
        command.arg("-blockdev").arg(serde_json::to_string(&json!({"driver":"raw","node-name":"veynode-disk","file":"veynode-disk-file","read-only":false}))?);
        command.args([
            "-device",
            "virtio-blk-pci,drive=veynode-disk,id=veynode-disk-device",
        ]);
        Ok(())
    }
    /// Validate the actual QMP query-fdsets result before QMP cont. Registration
    /// is not itself proof: the executing loader/backend must retain both sets.
    pub fn verify_fdsets(&self, value: &Value) -> Result<()> {
        let sets = value
            .as_array()
            .context("query-fdsets must return an array")?;
        for (id, opaque) in [
            (DISK_SET, self.disk_opaque()),
            (LOCK_SET, self.lock_opaque()),
        ] {
            let found: Vec<_> = sets
                .iter()
                .filter(|s| s["fdset-id"].as_u64() == Some(id))
                .collect();
            ensure!(
                found.len() == 1,
                "required inherited QEMU fdset missing or duplicated"
            );
            let fds = found[0]["fds"]
                .as_array()
                .context("fdset descriptors missing")?;
            ensure!(
                fds.len() == 1
                    && fds[0]["fd"].as_i64().is_some_and(|fd| fd > 2)
                    && fds[0]["opaque"].as_str() == Some(opaque.as_str()),
                "inherited QEMU disk/lock descriptor proof mismatch"
            );
        }
        Ok(())
    }
    #[cfg(test)]
    fn test_open(path: &Path, directory: &Path) -> Result<Self> {
        let mut pins = Pins::default();
        let disk = raw_file(path, &mut pins)?;
        let locks = pins.directory(directory)?;
        let m = pins.directories.last().unwrap().metadata()?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "unsafe test lock root"
        );
        Self::finish(path, disk, pins, locks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::fs::{symlink, PermissionsExt},
        process::Stdio,
        time::{Duration, Instant},
    };
    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        disk: PathBuf,
        locks: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let disk = root.join("disk.raw");
            fs::write(&disk, vec![0u8; 65536]).unwrap();
            fs::set_permissions(&disk, fs::Permissions::from_mode(0o600)).unwrap();
            let locks = root.join("locks");
            fs::create_dir(&locks).unwrap();
            fs::set_permissions(&locks, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                _temp: temp,
                root,
                disk,
                locks,
            }
        }
        fn open(&self) -> Result<DiskGuard> {
            DiskGuard::test_open(&self.disk, &self.locks)
        }
    }
    // Close-only disk locks intentionally survive fork. A concurrent test's
    // fork can retain an otherwise unrelated CLOEXEC descriptor until exec.
    // Reacquisition after owner teardown may therefore wait for that bounded
    // handoff; acquisition while an actual owner lives must still fail at once.
    fn await_available(path: &Path, locks: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match DiskGuard::test_open(path, locks) {
                Ok(guard) => {
                    drop(guard);
                    return;
                }
                Err(error) => {
                    assert!(
                        error.to_string().contains("disk inode is busy"),
                        "unexpected reopen error: {error:#}"
                    );
                    assert!(
                        Instant::now() < deadline,
                        "all owner handles closed but lock still unavailable: {error:#}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    #[test]
    fn cloexec_fork_reference_keeps_lock_until_child_exit() {
        // A no-exec fork retains *all* other test threads' CLOEXEC locks. Run
        // this intentional inheritance experiment in its own test process so
        // unrelated journal completion tests keep their exact lock assertions.
        const ISOLATED: &str = "VEYNODE_ISOLATED_DISK_FORK_TEST";
        if std::env::var_os(ISOLATED).is_none() {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "qemu_disk::tests::cloexec_fork_reference_keeps_lock_until_child_exit",
                    "--nocapture",
                ])
                .env(ISOLATED, "1")
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("isolated fork fixture deadline");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            return;
        }
        let f = Fixture::new();
        let witness_temp = tempfile::tempdir().unwrap();
        let witness_dir = witness_temp.path().canonicalize().unwrap();
        fs::set_permissions(&witness_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let unrelated =
            crate::exec_witness::Witness::create(&witness_dir, "unrelated", &"a".repeat(64)).unwrap();
        let unrelated_id = unrelated.identity.clone();
        let guard = f.open().unwrap();
        assert_ne!(
            unsafe { libc::fcntl(guard.inner.lock.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let mut pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // After multithreaded fork use only async-signal-safe libc calls.
            // This child deliberately does not exec or explicitly unlock.
            unsafe {
                libc::close(pipe[1]);
                libc::alarm(3);
                let mut byte = 0_u8;
                while libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1) < 0 {}
                libc::_exit(0);
            }
        }
        unsafe {
            libc::close(pipe[0]);
        }
        drop(guard);
        drop(unrelated);
        let while_inherited = f.open();
        let unrelated_while_inherited =
            crate::exec_witness::ReconciliationGuard::acquire(&witness_dir, &unrelated_id);
        // Signal and reap our exact still-owned child before assertions.
        let byte = 1_u8;
        let sent = unsafe { libc::write(pipe[1], (&byte as *const u8).cast(), 1) };
        unsafe {
            libc::close(pipe[1]);
        }
        let mut status = 0;
        loop {
            if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
                break;
            }
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EINTR));
        }
        assert_eq!(sent, 1);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(
            while_inherited.is_err(),
            "CLOEXEC unexpectedly released a live inherited disk lock before exec"
        );
        assert!(
            unrelated_while_inherited.is_err(),
            "fork unexpectedly did not inherit unrelated witness"
        );
        crate::exec_witness::ReconciliationGuard::acquire(&witness_dir, &unrelated_id).unwrap();
        println!("unrelated witness: busy while inherited; exact child reaped; exclusive proof succeeds");
        await_available(&f.disk, &f.locks);
    }
    #[test]
    fn hardlink_inode_and_shared_import_locks_conflict() {
        let f = Fixture::new();
        let guard = f.open().unwrap();
        let alias = f.root.join("alias.raw");
        fs::hard_link(&f.disk, &alias).unwrap();
        assert!(DiskGuard::test_open(&alias, &f.locks).is_err());
        let sidecar = f.locks.join(format!(
            "{}-{}.lock",
            guard.identity().device,
            guard.identity().inode
        ));
        drop(guard);
        // A distinct process models vm-image's shared sidecar lock.
        let mut child=Command::new("/usr/bin/python3").args(["-c","import sys,fcntl; f=open(sys.argv[1],'rb'); fcntl.flock(f,fcntl.LOCK_SH); print('ready',flush=True); sys.stdin.read()"])
            .arg(&sidecar).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        assert!(f.open().is_err());
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        await_available(&f.disk, &f.locks);
    }
    #[test]
    fn rejects_aliases_formats_permissions_and_sidecar_replacement() {
        let f = Fixture::new();
        let link = f.root.join("link.raw");
        symlink(&f.disk, &link).unwrap();
        assert!(validate_path(&link).is_err());
        let dirlink = f.root.join("alias");
        symlink(&f.root, &dirlink).unwrap();
        assert!(validate_path(&dirlink.join("disk.raw")).is_err());
        let file = fs::OpenOptions::new().write(true).open(&f.disk).unwrap();
        file.write_all_at(b"QFI\xfb", 0).unwrap();
        assert!(validate_path(&f.disk).is_err());
        file.write_all_at(&[0; 4], 0).unwrap();
        file.set_len(513).unwrap();
        assert!(validate_path(&f.disk).is_err());
        file.set_len(4096).unwrap();
        fs::set_permissions(&f.disk, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(f.open().is_err());
        fs::set_permissions(&f.disk, fs::Permissions::from_mode(0o600)).unwrap();
        let guard = f.open().unwrap();
        let sidecar = f.locks.join(format!(
            "{}-{}.lock",
            guard.identity().device,
            guard.identity().inode
        ));
        drop(guard);
        let extra = f.root.join("lock-alias");
        fs::hard_link(&sidecar, &extra).unwrap();
        assert!(f.open().is_err());
        fs::remove_file(&extra).unwrap();
        fs::remove_file(&sidecar).unwrap();
        symlink(&f.disk, &sidecar).unwrap();
        assert!(f.open().is_err());
    }
    #[test]
    fn replacement_is_detected_at_spawn_without_reopening_disk() {
        let f = Fixture::new();
        let guard = f.open().unwrap();
        let mut command = Command::new("/usr/bin/true");
        guard.install_inheritance(&mut command).unwrap();
        fs::rename(&f.disk, f.root.join("old.raw")).unwrap();
        fs::write(&f.disk, vec![0u8; 4096]).unwrap();
        assert!(guard.verify_paths().is_err());
        assert_eq!(
            command.spawn().unwrap_err().raw_os_error(),
            Some(libc::ESTALE)
        );
        assert!(DiskGuard::test_open(&f.root.join("old.raw"), &f.locks).is_err());
        drop(command);
        drop(guard);
        await_available(&f.root.join("old.raw"), &f.locks);
    }
    #[test]
    fn renamed_ancestor_cannot_be_replaced_with_another_directory() {
        let f = Fixture::new();
        let nested = f.root.join("nested");
        fs::create_dir(&nested).unwrap();
        let disk = nested.join("disk.raw");
        fs::rename(&f.disk, &disk).unwrap();
        let guard = DiskGuard::test_open(&disk, &f.locks).unwrap();
        fs::rename(&nested, f.root.join("old")).unwrap();
        fs::create_dir(&nested).unwrap();
        assert!(guard.verify_paths().is_err());
    }
    #[test]
    fn concurrent_first_open_has_exactly_one_owner() {
        use std::sync::Barrier;
        let f = Fixture::new();
        let start = Arc::new(Barrier::new(3));
        let hold = Arc::new(Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let disk = f.disk.clone();
                let locks = f.locks.clone();
                let start = Arc::clone(&start);
                let hold = Arc::clone(&hold);
                std::thread::spawn(move || {
                    start.wait();
                    let guard = DiskGuard::test_open(&disk, &locks);
                    hold.wait();
                    guard.is_ok()
                })
            })
            .collect();
        start.wait();
        hold.wait();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .filter(|ok| *ok)
                .count(),
            1
        );
        await_available(&f.disk, &f.locks);
    }
    #[test]
    fn typed_arguments_and_fdset_proof_fail_closed() {
        let f = Fixture::new();
        let guard = f.open().unwrap();
        let mut c = Command::new("qemu");
        guard.configure_command(&mut c).unwrap();
        let args: Vec<_> = c
            .get_args()
            .map(|x| x.to_str().unwrap().to_owned())
            .collect();
        assert!(!args.iter().any(|a| a.contains(f.disk.to_str().unwrap())));
        let blocks: Vec<Value> = args
            .windows(2)
            .filter(|w| w[0] == "-blockdev")
            .map(|w| serde_json::from_str(&w[1]).unwrap())
            .collect();
        assert_eq!(blocks[0]["filename"], "/dev/fdset/1");
        assert_eq!(blocks[0]["locking"], "on");
        assert_eq!(blocks[1]["driver"], "raw");
        let good = json!([{"fdset-id":1,"fds":[{"fd":31,"opaque":guard.disk_opaque()}]},{"fdset-id":2,"fds":[{"fd":32,"opaque":guard.lock_opaque()}]}]);
        guard.verify_fdsets(&good).unwrap();
        for bad in [json!([]), json!({}), json!([good[0], good[0], good[1]])] {
            assert!(guard.verify_fdsets(&bad).is_err());
        }
        let mut bad = good.clone();
        bad[1]["fds"][0]["opaque"] = json!("wrong");
        assert!(guard.verify_fdsets(&bad).is_err());
        bad = good.clone();
        bad[1]["fds"][0]["fd"] = json!(true);
        assert!(guard.verify_fdsets(&bad).is_err());
    }
    #[test]
    fn child_inherits_both_fds_and_parent_drop_does_not_unlock() {
        let f = Fixture::new();
        let guard = f.open().unwrap();
        let disk = guard.inner.disk.as_raw_fd();
        let lock = guard.inner.lock.as_raw_fd();
        for fd in [disk, lock] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-c","import os,sys,fcntl; fds=[int(x) for x in sys.argv[1:]]; assert all(fcntl.fcntl(f,fcntl.F_GETFD)&fcntl.FD_CLOEXEC==0 for f in fds); assert all(os.fstat(f).st_ino>0 for f in fds); print('ready',flush=True); sys.stdin.read()"])
            .args([disk.to_string(),lock.to_string()]).stdin(Stdio::piped()).stdout(Stdio::piped());
        guard.install_inheritance(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        drop(command);
        drop(guard);
        assert!(f.open().is_err());
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        await_available(&f.disk, &f.locks);
    }
    #[test]
    fn worker_sigkill_does_not_release_backend_inherited_sidecar() {
        let f = Fixture::new();
        let guard = f.open().unwrap();
        let ready = f.root.join("ready");
        let stop = f.root.join("stop");
        let mut cmd = Command::new("/usr/bin/python3");
        cmd.args([
            "-c",
            r#"import os,sys,time
pid=os.fork()
if pid:
 while True: time.sleep(.05)
else:
 open(sys.argv[1],'w').write(str(os.getpid()))
 deadline=time.monotonic()+10
 while not os.path.exists(sys.argv[2]) and time.monotonic()<deadline: time.sleep(.01)
 os._exit(0)
"#,
        ])
        .arg(&ready)
        .arg(&stop)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        guard.install_inheritance(&mut cmd).unwrap();
        let mut worker = cmd.spawn().unwrap();
        drop(cmd);
        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let became_ready = ready.exists();
        worker.kill().unwrap();
        worker.wait().unwrap();
        let busy = f.open().is_err();
        fs::write(stop, b"stop").unwrap();
        assert!(became_ready);
        assert!(busy, "worker death unlocked live backend");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if f.open().is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "backend failed bounded exit/descriptor release"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Uses the explicitly selected existing loader/libraries, a new 64 KiB disk,
    /// machine none and TCG. No Guest OS, HVF or existing disk is started.
    #[test]
    #[ignore = "requires explicit real QEMU fixture paths and evidence destination"]
    fn actual_loader_fdsets_survive_cont_and_monitor_reconnect() {
        use std::{io::Write, os::unix::net::UnixStream};
        let options: Vec<String> = serde_json::from_slice(
            &fs::read(std::env::var("VEYNODE_QEMU_PROBE_ARGUMENTS").unwrap()).unwrap(),
        )
        .unwrap();
        let option =
            |name: &str| options[options.iter().position(|x| x == name).unwrap() + 1].clone();
        let f = Fixture::new();
        let guard = f.open().unwrap();
        let identity = guard.identity();
        let sidecar = f
            .locks
            .join(format!("{}-{}.lock", identity.device, identity.inode));
        let socket = f.root.join("qmp.sock");
        let stderr_path = f.root.join("qemu.log");
        let mut template = Command::new("unused");
        guard.configure_command(&mut template).unwrap();
        let args: Vec<_> = template.get_args().map(|x| x.to_owned()).collect();
        let mut command = Command::new(option("--qemu-launcher"));
        command
            .arg(option("--qemu-library"))
            .env("VEYNODE_SPICE_LIBRARY", option("--qemu-spice-library"))
            .args([
                "-machine",
                "none",
                "-accel",
                "tcg",
                "-m",
                "16M",
                "-S",
                "-nodefaults",
                "-display",
                "none",
                "-qmp",
            ])
            .arg(format!("unix:{},server=on,wait=off", socket.display()))
            // machine none has no PCI bus; the fixed virtio-blk device is tested
            // separately by actual VM acceptance. This tests the identical raw
            // file nodes and inherited lock set in the actual QEMU implementation.
            .args(&args[..args.len() - 2])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&stderr_path).unwrap());
        guard.install_inheritance(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        drop(command);
        drop(template);
        let result = (|| -> Result<Value> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !socket.exists() {
                ensure!(
                    child.try_wait()?.is_none(),
                    "QEMU early exit: {}",
                    fs::read_to_string(&stderr_path)?
                );
                ensure!(Instant::now() < deadline, "QMP socket timeout");
                std::thread::sleep(Duration::from_millis(10));
            }
            fn connect(path: &Path) -> Result<BufReader<UnixStream>> {
                let stream = UnixStream::connect(path)?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                let mut r = BufReader::new(stream);
                let mut greeting = String::new();
                r.read_line(&mut greeting)?;
                ensure!(
                    serde_json::from_str::<Value>(&greeting)?["QMP"].is_object(),
                    "QMP greeting absent"
                );
                rpc(&mut r, "qmp_capabilities")?;
                Ok(r)
            }
            fn rpc(r: &mut BufReader<UnixStream>, name: &str) -> Result<Value> {
                writeln!(r.get_mut(), "{}", json!({"execute":name,"id":name}))?;
                for _ in 0..32 {
                    let mut line = String::new();
                    ensure!(
                        r.read_line(&mut line)? > 0 && line.len() < 65536,
                        "QMP EOF or oversized response"
                    );
                    let v: Value = serde_json::from_str(&line)?;
                    if v["id"] == name {
                        ensure!(v.get("error").is_none(), "QMP {name}: {v}");
                        return Ok(v["return"].clone());
                    }
                }
                bail!("QMP response missing")
            }
            let mut qmp = connect(&socket)?;
            let before = rpc(&mut qmp, "query-fdsets")?;
            guard.verify_fdsets(&before)?;
            rpc(&mut qmp, "cont")?;
            let state = rpc(&mut qmp, "query-status")?;
            ensure!(
                state["running"] == true,
                "machine-none did not enter running: {state}"
            );
            let after = rpc(&mut qmp, "query-fdsets")?;
            guard.verify_fdsets(&after)?;
            drop(qmp);
            std::thread::sleep(Duration::from_millis(30));
            let mut qmp = connect(&socket)?;
            let reconnected = rpc(&mut qmp, "query-fdsets")?;
            guard.verify_fdsets(&reconnected)?;
            // Drop all worker copies while actual QEMU remains running. Parent
            // process still exists, but can no longer be the lock owner.
            drop(guard);
            ensure!(f.open().is_err(), "actual backend failed to retain sidecar");
            let lock_probe = Command::new("/usr/bin/python3")
                .args([
                    "-c",
                    r#"import fcntl,sys
f=open(sys.argv[1],'rb')
try: fcntl.flock(f,fcntl.LOCK_SH|fcntl.LOCK_NB)
except BlockingIOError: sys.exit(0)
sys.exit(1)
"#,
                ])
                .arg(&sidecar)
                .status()?;
            ensure!(
                lock_probe.success(),
                "external importer acquired live QEMU sidecar"
            );
            rpc(&mut qmp, "quit")?;
            Ok(
                json!({"passed":true,"machine":"none","guest_started":false,"before_cont":before,"after_cont":after,"after_monitor_reconnect":reconnected,"running":state,"external_shared_lock_denied":true,"all_worker_descriptor_copies_dropped":true,"actual_backend_retained_lock":true,"disk_bytes":65536,"virtio_device_attachment_tested":false}),
            )
        })();
        // Always reap only this directly owned process; failed assertions must
        // not leave the diagnostic QEMU running.
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                break child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let log = fs::read_to_string(&stderr_path).unwrap();
        let mut report = match result {
            Ok(v) => v,
            Err(e) => json!({"passed":false,"error":format!("{e:#}")}),
        };
        report["exit_success"] = json!(status.success());
        report["stderr"] = json!(log);
        report["lock_available_after_reap"] = json!(f.open().is_ok());
        let out = std::env::var("VEYNODE_QEMU_PROBE_OUTPUT").unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(out)
            .unwrap();
        file.write_all(serde_json::to_string_pretty(&report).unwrap().as_bytes())
            .unwrap();
        assert_eq!(report["passed"], true, "{report}");
        assert!(status.success());
        assert_eq!(report["lock_available_after_reap"], true);
    }
}
